//! What the user last chose, shared by the host and its frontends.
//!
//! Every other source of a model is something written down on purpose: `-m`
//! for this run, `ELMEE_MODEL` for this shell, `defaultModel` for this project
//! or this machine. Those keep winning. This is the one below them — a note of
//! what was actually used last time, so that having picked a model once is
//! enough, and starting elmee never fails for a reason the last session
//! already answered.
//!
//! Kept machine-global for the same reason prompt history is: it follows the
//! person, and a project with an opinion has already stated it in a file that
//! outranks this. Written only by a session a human is driving — a subagent
//! runs whatever model it was handed, which is not a choice anyone made.

use std::{
    fs,
    path::{Path, PathBuf},
};

fn note() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".elmee/last-model.json"))
}

/// The spec last used, if there is one and it still parses.
pub fn last_model() -> Option<String> {
    stored(&note()?)
}

/// Record a spec as the one to fall back on next time.
///
/// Best effort in both directions: a run is never failed over a preference,
/// and an unchanged choice is not rewritten, so the child processes that
/// inherit their parent's model never touch the file at all.
pub fn remember_model(spec: &str) {
    if let Some(note) = note() {
        record(&note, spec);
    }
}

fn stored(note: &Path) -> Option<String> {
    let data = fs::read(note).ok()?;
    let model =
        serde_json::from_slice::<serde_json::Value>(&data).ok()?.get("model")?.as_str()?.trim().to_owned();
    (!model.is_empty()).then_some(model)
}

fn record(note: &Path, spec: &str) {
    if spec.trim().is_empty() || stored(note).as_deref() == Some(spec) {
        return;
    }
    let _ = elmee_plugin_atomic::write_json(note, &serde_json::json!({ "model": spec }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_choice_is_remembered_and_an_unchanged_one_is_not_rewritten() {
        let home = std::env::temp_dir().join(elmee_core::unique("elmee-preference-test"));
        let note = home.join(".elmee/last-model.json");

        assert_eq!(stored(&note), None, "nothing is remembered before anything is chosen");
        record(&note, "anthropic/claude-opus-5");
        assert_eq!(stored(&note).as_deref(), Some("anthropic/claude-opus-5"));

        let written = fs::metadata(&note).unwrap().modified().unwrap();
        record(&note, "anthropic/claude-opus-5");
        assert_eq!(fs::metadata(&note).unwrap().modified().unwrap(), written, "rewritten anyway");

        record(&note, "local/qwen3");
        assert_eq!(stored(&note).as_deref(), Some("local/qwen3"), "a new choice replaces it");
        record(&note, "   ");
        assert_eq!(stored(&note).as_deref(), Some("local/qwen3"), "an empty one is not a choice");

        fs::write(&note, b"not json at all").unwrap();
        assert_eq!(stored(&note), None, "a damaged note is no note, not a failure");
        let _ = fs::remove_dir_all(home);
    }
}
