//! Declarative provider registry, pi-style: built-in defaults (a local
//! OpenAI-compatible endpoint and Anthropic) extended or overridden by
//! `~/.elmee/models.json` and model metadata from the project's
//! `.elmee/models.json`. Provider endpoints and credentials are machine-owned:
//! a repository may select and describe a provider, but cannot redirect one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::{Error, Result};
use serde::Deserialize;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Api {
    OpenAiCompletions,
    AnthropicMessages,
    OpenAiResponses,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Subscription {
    /// Signing in through Codex's own OAuth client to Codex's own endpoint. A
    /// ChatGPT subscription is what pays for it, which is why this used to be
    /// named after ChatGPT — but what you sign in to, and what decides which
    /// models you are served, is Codex.
    Codex,
}

#[derive(Clone)]
pub struct ProviderEntry {
    pub name: String,
    pub api: Api,
    pub base_url: String,
    pub api_key: Option<String>,
    /// Subscription credentials, fetched per request because OAuth tokens expire.
    pub subscription: Option<Subscription>,
    /// Models declared in the config; live endpoint listings are merged in.
    pub declared: Vec<String>,
    /// What this client says it implements, for an endpoint that serves its
    /// catalogue by version. Absent means elmee's own version.
    pub client_version: Option<String>,
    /// Whether the endpoint's own catalogue is merged into the offered list.
    /// Absent leaves the decision to whether anything was declared.
    pub discover: Option<bool>,
}

mod files;
pub use files::*;

/// Which model a delegated agent runs on.
///
/// An alias rather than a spec, so the vocabulary an agent uses is the one the
/// machine's owner wrote and can re-point without touching a prompt. Held apart
/// from `Registry` because it travels: the loop hands it to the host, which is
/// the only thing that starts another elmee.
///
/// Not a role. A role is what an agent *is* — its instructions and the slice of
/// the project it works with — and lives in the repository; an alias is a name
/// for a model and lives on the machine, because the same reviewer should ride
/// a frontier model on one desk and a local one on another. Calling these roles
/// is what left the harness with no word for the other thing.
#[derive(Clone, Default, Debug)]
pub struct ModelAliases {
    aliases: BTreeMap<String, Vec<String>>,
    /// What a spec may name, so an unknown one is refused before a child is
    /// started rather than after its first request fails.
    providers: Vec<String>,
    declared: Vec<String>,
}

impl ModelAliases {
    /// The model a child should run on: what was asked for, else the alias
    /// bearing its mode's name, else what the parent runs on.
    ///
    /// An alias naming several models is refused when it is *asked for*,
    /// because then the caller is choosing and has to choose; as a mode's
    /// default it takes the first, because there is nobody choosing.
    pub fn resolve(&self, requested: Option<&str>, mode: &str, inherited: &str) -> Result<String> {
        let Some(requested) = requested.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(self
                .aliases
                .get(mode)
                .and_then(|models| models.first())
                .cloned()
                .unwrap_or_else(|| inherited.to_owned()));
        };
        if let Some(models) = self.aliases.get(requested) {
            return match models.as_slice() {
                [only] => Ok(only.clone()),
                [] => Err(Error::Config(format!("alias {requested} names no model"))),
                many => Err(Error::Config(format!(
                    "{requested} is a set of {} models, so name one of them: {}",
                    many.len(),
                    many.join(", ")
                ))),
            };
        }
        if self.serves(requested) {
            return Ok(requested.to_owned());
        }
        Err(Error::Config(format!(
            "unknown model {requested}{}; providers: {}. Omit model to run on this session's",
            if self.aliases.is_empty() {
                String::new()
            } else {
                format!("; aliases: {}", self.aliases.keys().cloned().collect::<Vec<_>>().join(", "))
            },
            self.providers.join(", ")
        )))
    }

    /// Whether this registry can carry a spec at all. A provider prefix is
    /// enough, because an endpoint like Ollama serves whatever id it is handed
    /// and second-guessing that is how a working model gets refused.
    fn serves(&self, spec: &str) -> bool {
        spec.split_once('/').is_some_and(|(prefix, _)| self.providers.iter().any(|name| name == prefix))
            || self.declared.iter().any(|model| model == spec)
    }

    /// The aliases, as one line for the tool that spawns agents. `None` where
    /// none are declared, so a harness without them pays nothing for the idea.
    pub fn roster(&self) -> Option<String> {
        if self.aliases.is_empty() {
            return None;
        }
        let named: Vec<String> =
            self.aliases.iter().map(|(name, models)| format!("{name} ({})", models.join(", "))).collect();
        Some(format!(
            "Declared model aliases: {}. An alias naming several models is a set — spawn one agent \
             per member to have them answer separately.",
            named.join("; ")
        ))
    }
}

/// The resolved provider registry plus config-level defaults.
pub struct Registry {
    pub entries: Vec<ProviderEntry>,
    /// `defaultModel` from models.json — used when no -m/ELMEE_MODEL is given.
    pub default_model: Option<String>,
    /// Declared context windows, keyed by `provider/model`.
    windows: BTreeMap<String, usize>,
    aliases: BTreeMap<String, Vec<String>>,
}

impl Registry {
    /// The vocabulary a delegating agent may use, resolved against what this
    /// registry can actually serve.
    pub fn model_aliases(&self) -> ModelAliases {
        ModelAliases {
            aliases: self.aliases.clone(),
            providers: self.entries.iter().map(|entry| entry.name.clone()).collect(),
            declared: self.entries.iter().flat_map(|entry| entry.declared.clone()).collect(),
        }
    }

    /// The context window for a resolved spec, in tokens: what models.json
    /// declares, else what the endpoint itself last reported.
    pub fn context_window(&self, spec: &str) -> Option<usize> {
        window_for(&self.windows, spec)
    }
}

/// The window for a spec, from the more authoritative source that knows one:
/// what this machine declared, then what the endpoint itself last reported.
/// Shared with [`crate::MultiProvider`], which re-derives it on every model
/// switch.
pub(crate) fn window_for(windows: &BTreeMap<String, usize>, spec: &str) -> Option<usize> {
    pick_window(windows, learned_windows(), spec)
}

/// What this machine declared outranks what an endpoint said, because a
/// person who wrote a number down meant it. A model neither knows runs on the
/// default threshold until someone declares its `contextWindow`.
fn pick_window(
    declared: &BTreeMap<String, usize>,
    learned: &BTreeMap<String, usize>,
    spec: &str,
) -> Option<usize> {
    declared.get(spec).or_else(|| learned.get(spec)).copied()
}

mod catalogue;
pub use catalogue::*;

/// Provider construction shared by every application composition root.
pub struct Selection {
    pub provider: crate::MultiProvider,
    pub spec: String,
    pub context_window: Option<usize>,
    /// Whether the endpoint is spoken to without TLS — in practice, a model
    /// running on this machine or beside it.
    pub local: bool,
    pub aliases: ModelAliases,
}

pub fn select(cwd: &Path, requested: Option<String>, fallback: Option<String>) -> Result<Selection> {
    let registry = load(cwd)?;
    let aliases = registry.model_aliases();
    let selected = requested.or_else(|| registry.default_model.clone()).or(fallback).ok_or_else(|| {
        // What to actually pass. Declared models first, and otherwise whatever
        // the endpoints last reported: naming a remedy without naming a value
        // to give it sends a person to the documentation for a word this
        // machine already knows.
        let mut known = registry
            .entries
            .iter()
            .flat_map(|entry| entry.declared.iter().map(move |model| format!("{}/{model}", entry.name)))
            .collect::<Vec<_>>();
        if known.is_empty() {
            known = learned_windows().keys().cloned().collect();
        }
        let offer = if known.is_empty() {
            "declare one in ~/.elmee/models.json".to_owned()
        } else {
            format!("this machine has: {}", known.join(", "))
        };
        Error::Config(format!(
            "no model chosen yet: pass -m/--model, set ELMEE_MODEL, or set defaultModel in \
             models.json — {offer}. The model a run uses is remembered for the next one."
        ))
    })?;
    // A dotted prefix (hf.co/...) is part of the model id, as `resolve`
    // documents; only a dotless unknown prefix is a misnamed provider.
    if let Some((prefix, _)) = selected.split_once('/')
        && !prefix.contains('.')
        && !registry.entries.iter().any(|entry| entry.name == prefix)
    {
        return Err(Error::Config(format!(
            "no provider called {prefix}; available providers: {}",
            registry.entries.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>().join(", ")
        )));
    }
    let (active, model) = resolve(&registry.entries, &selected);
    let spec = crate::spec(&registry.entries[active].name, &model);
    let local = registry.entries[active].base_url.starts_with("http://");
    let provider = crate::MultiProvider::new(registry.entries, active, model).with_windows(registry.windows);
    Ok(Selection { context_window: provider.context_window(), provider, spec, local, aliases })
}

/// Resolve a model spec to (provider index, bare model id). `provider/model`
/// is explicit; a bare id matches a uniquely declared model, then `claude-*`
/// routes to the first Anthropic provider, then the default endpoint. An
/// unknown prefix is treated as part of the model id (hf.co/... names).
pub fn resolve(entries: &[ProviderEntry], spec: &str) -> (usize, String) {
    if let Some((prefix, rest)) = spec.split_once('/')
        && let Some(index) = entries.iter().position(|entry| entry.name == prefix)
    {
        return (index, rest.to_owned());
    }
    let declared: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.declared.iter().any(|model| model == spec))
        .map(|(index, _)| index)
        .collect();
    if let [only] = declared[..] {
        return (only, spec.to_owned());
    }
    if spec.starts_with("claude-")
        && let Some(index) = entries.iter().position(|entry| entry.api == Api::AnthropicMessages)
    {
        return (index, spec.to_owned());
    }
    let index = entries.iter().position(|entry| entry.name == "ollama").unwrap_or(0);
    (index, spec.to_owned())
}

#[cfg(test)]
mod tests;
