//! models.json as it is written: the machine file, the smaller project file,
//! the built-in providers, and how they become a `Registry`.

use super::*;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ModelsFile {
    #[serde(default)]
    providers: BTreeMap<String, ProviderConfig>,
    default_model: Option<String>,
    /// Names for models, so a delegated agent can be pointed at "cheap" or
    /// "review" without a spec being written into a prompt. Machine-side only,
    /// and deliberately not mirrored on the project side even though
    /// `defaultModel` is: the model a session runs on is chosen by a person at
    /// startup and stands in the status line the whole time, while a child's
    /// is chosen by the model mid-turn. A checked-in file that quietly
    /// re-points the second is a different kind of surprise from one that
    /// suggests the first. Not roles — a role is what an agent is, lives in
    /// the project, and names no model at all.
    #[serde(default, rename = "modelAliases")]
    pub(super) aliases: BTreeMap<String, Roster>,
}

/// One model or several. Several is a set to choose from — the shape a
/// multi-model review is declared in.
#[derive(Deserialize, Clone)]
#[serde(untagged)]
pub(super) enum Roster {
    One(String),
    Many(Vec<String>),
}

impl Roster {
    pub(super) fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(model) => vec![model],
            Self::Many(models) => models,
        }
    }
}

/// The deliberately smaller project-side schema. Unknown fields are rejected
/// rather than ignored so a checked-in `baseUrl` or `apiKeyEnv` never looks as
/// though it took effect. Provider transports belong in ~/.elmee/models.json.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectModelsFile {
    #[serde(default)]
    providers: BTreeMap<String, ProjectProviderConfig>,
    default_model: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProjectProviderConfig {
    #[serde(default)]
    models: Vec<ModelEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderConfig {
    base_url: String,
    api: String,
    api_key: Option<String>,
    api_key_env: Option<String>,
    /// Take OAuth credentials from an `elmee login` account instead of a key:
    /// the name of the account kind, `"codex"`.
    subscription: Option<String>,
    /// What this client claims to implement, for an endpoint that serves its
    /// catalogue by version. Each model there declares a
    /// `minimal_client_version`, so this decides how much of the catalogue is
    /// offered. Machine-side only, like every other transport detail.
    client_version: Option<String>,
    /// Whether to ask the endpoint what else it serves.
    ///
    /// Absent means: ask when nothing is declared, and do not when something
    /// is. Listing models is how a person says which ones they want, and a
    /// gateway that serves four hundred would otherwise bury them; declaring
    /// none is how they say "show me what is there", which is the only way a
    /// local runtime whose models change weekly can be usable at all.
    discover: Option<bool>,
    #[serde(default)]
    models: Vec<ModelEntry>,
}

fn subscription(name: Option<String>, api: Api, provider: &str) -> Result<Option<Subscription>> {
    match name.as_deref() {
        None => Ok(None),
        Some("codex") if api == Api::OpenAiResponses => Ok(Some(Subscription::Codex)),
        Some("codex") => Err(Error::Config(format!(
            "provider {provider}: subscription \"codex\" is incompatible with its API"
        ))),
        Some(name) => {
            Err(Error::Config(format!("provider {provider}: unknown subscription {name:?} (expected codex)")))
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelEntry {
    id: String,
    /// Tokens the model accepts. Neither the OpenAI nor the Anthropic
    /// `/models` endpoint reports it, so it is declared here or not known —
    /// and without it the compaction threshold is guesswork.
    context_window: Option<usize>,
}

fn default_key(api: Api) -> Option<String> {
    match api {
        Api::OpenAiCompletions => {
            std::env::var("ELMEE_API_KEY").or_else(|_| std::env::var("OPENAI_API_KEY")).ok()
        }
        Api::AnthropicMessages => std::env::var("ANTHROPIC_API_KEY").ok(),
        Api::OpenAiResponses => std::env::var("OPENAI_API_KEY").ok(),
    }
}

/// What to claim when asking the subscription endpoint for its catalogue.
///
/// Every model there declares a `minimal_client_version`, and the endpoint
/// hands over only the models at or below what the caller claims. Elmee's own
/// version is `0.1.0`, which sits under every gate in the catalogue, so asking
/// honestly returned nothing at all and a list compiled into the binary was the
/// only answer there could be — which is how a model the account had been
/// served for months stayed invisible.
///
/// A number above every gate rather than some real Codex release: this says
/// "do not filter by version", which is true, instead of claiming to be a
/// particular build of another program, which would not be. Identity is not in
/// this field — it is the `originator: elmee` header, and that stays honest.
/// The parameter reaches only the catalogue request; generation never carries
/// it, so nothing about a turn depends on it. `clientVersion` in models.json
/// overrides it for anyone who wants the version-gated view back.
const UNGATED_CATALOGUE: &str = "999.0.0";

pub(super) fn default_entries() -> Vec<ProviderEntry> {
    vec![
        // Named for what it is, not for the dialect it speaks. This entry was
        // called `openai` once, and the name did exactly what a lying name
        // does: an agent asked to pick a model read "openai" in an error,
        // chose `openai/gpt-4.1`, and the request went to a local Ollama that
        // serves no such thing. The dialect lives in `api`, where it belongs;
        // a real OpenAI endpoint is one models.json entry away.
        ProviderEntry {
            name: "ollama".to_owned(),
            api: Api::OpenAiCompletions,
            base_url: std::env::var("ELMEE_BASE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:11434/v1".to_owned()),
            api_key: default_key(Api::OpenAiCompletions),
            subscription: None,
            declared: Vec::new(),
            client_version: None,
            discover: None,
        },
        ProviderEntry {
            name: "anthropic".to_owned(),
            api: Api::AnthropicMessages,
            base_url: std::env::var("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|_| "https://api.anthropic.com".to_owned()),
            api_key: default_key(Api::AnthropicMessages),
            subscription: None,
            declared: Vec::new(),
            client_version: None,
            discover: None,
        },
        // Codex, once `elmee login` has been run — signed in to with a ChatGPT
        // account, which is what pays for it, but Codex is what is asked and
        // what decides what is served. It keeps a real catalogue including each
        // model's own context window, so that catalogue *is* the list: nothing
        // is declared here, and a model that appears this week needs no release.
        // `discover` stays explicit rather than implied by the empty list — a
        // person pinning a favourite in models.json would otherwise switch
        // discovery off and silently shrink their own model list.
        ProviderEntry {
            name: "codex".to_owned(),
            api: Api::OpenAiResponses,
            base_url: "https://chatgpt.com/backend-api/codex".to_owned(),
            api_key: None,
            subscription: Some(Subscription::Codex),
            declared: Vec::new(),
            client_version: Some(UNGATED_CATALOGUE.to_owned()),
            discover: Some(true),
        },
    ]
}

fn validate_subscription_endpoint(
    subscription: Option<Subscription>,
    base_url: &str,
    provider: &str,
) -> Result<()> {
    let expected = match subscription {
        Some(Subscription::Codex) => "https://chatgpt.com/backend-api/codex",
        None => return Ok(()),
    };
    if base_url != expected {
        return Err(Error::Config(format!(
            "provider {provider}: subscription credentials may only be sent to {expected}, not {base_url}"
        )));
    }
    Ok(())
}

pub fn load(cwd: &Path) -> Result<Registry> {
    let mut entries = default_entries();
    let mut default_model = None;
    let mut windows: BTreeMap<String, usize> = BTreeMap::new();
    let mut aliases = BTreeMap::new();
    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home).join(".elmee/models.json");
        if let Ok(text) = std::fs::read_to_string(&path) {
            let mut file: ModelsFile = serde_json::from_str(&text)
                .map_err(|error| Error::Config(format!("{}: {error}", path.display())))?;
            // Taken out before the rest is applied, because it is the one thing
            // in the file no other stage has an opinion about.
            aliases = std::mem::take(&mut file.aliases)
                .into_iter()
                .map(|(name, models)| (name, models.into_vec()))
                .collect();
            apply_machine_file(file, &path, &mut entries, &mut default_model, &mut windows)?;
        }
    }
    let project_path = cwd.join(".elmee/models.json");
    if let Ok(text) = std::fs::read_to_string(&project_path) {
        let file: ProjectModelsFile = serde_json::from_str(&text).map_err(|error| {
            Error::Config(format!(
                "{}: {error}; project models may contain only defaultModel and model metadata; configure provider endpoints and credentials in ~/.elmee/models.json",
                project_path.display()
            ))
        })?;
        if file.default_model.is_some() {
            default_model = file.default_model;
        }
        for (name, provider) in file.providers {
            let Some(entry) = entries.iter_mut().find(|entry| entry.name == name) else {
                return Err(Error::Config(format!(
                    "{}: project refers to unknown provider {name:?}; declare its transport in ~/.elmee/models.json",
                    project_path.display()
                )));
            };
            for model in provider.models {
                if let Some(window) = model.context_window {
                    windows.insert(format!("{name}/{}", model.id), window);
                }
                if !entry.declared.contains(&model.id) {
                    entry.declared.push(model.id);
                }
            }
        }
    }
    Ok(Registry { entries, default_model, windows, aliases })
}

pub(super) fn apply_machine_file(
    file: ModelsFile,
    path: &Path,
    entries: &mut Vec<ProviderEntry>,
    default_model: &mut Option<String>,
    windows: &mut BTreeMap<String, usize>,
) -> Result<()> {
    if file.default_model.is_some() {
        *default_model = file.default_model;
    }
    for (name, provider) in file.providers {
        let api = match provider.api.as_str() {
            "openai-completions" => Api::OpenAiCompletions,
            "anthropic-messages" => Api::AnthropicMessages,
            "openai-responses" => Api::OpenAiResponses,
            other => {
                return Err(Error::Config(format!(
                    "{}: provider {name}: unsupported api {other:?} (expected \
                         openai-completions, anthropic-messages, or openai-responses)",
                    path.display()
                )));
            }
        };
        let api_key = provider
            .api_key
            .or_else(|| provider.api_key_env.and_then(|variable| std::env::var(variable).ok()))
            .or_else(|| default_key(api));
        let mut declared = Vec::new();
        for model in provider.models {
            if let Some(window) = model.context_window {
                windows.insert(format!("{name}/{}", model.id), window);
            }
            declared.push(model.id);
        }
        let subscription = subscription(provider.subscription, api, &name)?;
        let base_url = provider.base_url.trim_end_matches('/').to_owned();
        validate_subscription_endpoint(subscription, &base_url, &name)?;
        let entry = ProviderEntry {
            name: name.clone(),
            api,
            base_url,
            api_key,
            subscription,
            declared,
            client_version: provider.client_version.clone(),
            discover: provider.discover,
        };
        match entries.iter_mut().find(|existing| existing.name == name) {
            Some(existing) => *existing = entry,
            None => entries.push(entry),
        }
    }
    Ok(())
}
