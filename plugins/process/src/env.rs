//! The environment elmee hands to what it starts, and how a person changes it.
//!
//! A child process cannot change its parent's environment, so `! export FOO=1`
//! run as a subprocess would set a variable that dies with the shell that set
//! it — which is not what anyone means by typing it. What they mean is the
//! terminal the agent is sitting in, so the harness has to take the change on
//! itself and hand it to everything it starts afterwards.
//!
//! Kept as an overlay rather than written into this process's own environment.
//! `set_var` is unsafe in this edition for a reason that applies here exactly —
//! it races every concurrent `getenv` in a threaded process — and this crate
//! forbids unsafe. An overlay is also the more honest shape: what elmee gives a
//! child is a decision it makes at each spawn, visible in one place, rather
//! than a global that anything might have moved.

use std::{
    collections::BTreeMap,
    sync::{LazyLock, Mutex},
};

/// `None` means the variable was unset rather than changed.
type Overlay = BTreeMap<String, Option<String>>;

static CHANGES: LazyLock<Mutex<Overlay>> = LazyLock::new(|| Mutex::new(Overlay::new()));

/// Variables a shell changes on its own account, every time, and which say
/// nothing about what the person asked for.
const NOISE: &[&str] = &["_", "PWD", "OLDPWD", "SHLVL", "PS1", "PS2", "RANDOM", "LINENO"];

/// What this session has changed, for a caller about to start a process.
pub fn overlay() -> Vec<(String, Option<String>)> {
    CHANGES
        .lock()
        .map(|changes| changes.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

/// Put the overlay onto a command that has not been started.
pub fn apply(command: &mut std::process::Command) {
    for (name, value) in overlay() {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }
}

/// Take on what a shell changed about its own environment.
pub fn adopt(changes: Vec<(String, Option<String>)>) {
    if let Ok(mut overlay) = CHANGES.lock() {
        overlay.extend(changes);
    }
}

/// What is set now: this process's environment with the overlay over it.
fn effective() -> BTreeMap<String, String> {
    let mut current: BTreeMap<String, String> = std::env::vars().collect();
    for (name, value) in overlay() {
        match value {
            Some(value) => current.insert(name, value),
            None => current.remove(&name),
        };
    }
    current
}

/// The difference between what a shell was given and what it ended up with.
///
/// `dump` is `env -0` output: NUL-separated `NAME=VALUE`, which is the only
/// portable form that survives a value containing a newline — and a value
/// containing a newline is exactly what a person pasting a key will produce.
pub fn changed(dump: &[u8]) -> Vec<(String, Option<String>)> {
    let before = effective();
    let mut after = BTreeMap::new();
    for entry in dump.split(|byte| *byte == 0).filter(|entry| !entry.is_empty()) {
        let entry = String::from_utf8_lossy(entry);
        if let Some((name, value)) = entry.split_once('=') {
            after.insert(name.to_owned(), value.to_owned());
        }
    }
    // A shell that produced no dump at all — it exited, or `env` is missing —
    // must not read as "the person unset their whole environment".
    if after.is_empty() {
        return Vec::new();
    }
    let mut changes = Vec::new();
    for (name, value) in &after {
        if NOISE.contains(&name.as_str()) {
            continue;
        }
        if before.get(name) != Some(value) {
            changes.push((name.clone(), Some(value.clone())));
        }
    }
    for name in before.keys() {
        if !NOISE.contains(&name.as_str()) && !after.contains_key(name) {
            changes.push((name.clone(), None));
        }
    }
    changes
}

/// Run a line the person typed, and keep whatever it changed about the
/// environment.
///
/// This is the person acting, not the model: a line somebody typed into their
/// own terminal runs with their own authority and their whole environment.
///
/// The shell dumps its environment when it is done, to a file rather than to
/// stdout, so the command's own output stays the command's own output. A
/// command that calls `exit` never reaches the dump, and then nothing is
/// adopted, which is the right answer rather than a guess.
pub async fn run_here(cwd: &std::path::Path, line: &str) -> (bool, String) {
    let dump = std::env::temp_dir().join(format!("elmee-env-{}-{}", std::process::id(), fresh()));
    let script = format!(
        "{line}
__elmee_status=$?
env -0 > {dump:?} 2>/dev/null
exit $__elmee_status"
    );
    let mut command = tokio::process::Command::new("sh");
    command.arg("-c").arg(&script).current_dir(cwd);
    apply(command.as_std_mut());
    let outcome = crate::command(&mut command, std::time::Duration::from_secs(600), "shell command").await;
    if let Ok(bytes) = std::fs::read(&dump) {
        adopt(changed(&bytes));
    }
    let _ = std::fs::remove_file(&dump);
    match outcome {
        Ok(output) => {
            let mut text = output.stdout;
            if !output.stderr.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&output.stderr);
            }
            (output.status.success(), text)
        }
        Err(error) => (false, error.to_string()),
    }
}

fn fresh() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dump(entries: &[&str]) -> Vec<u8> {
        entries.iter().flat_map(|entry| entry.bytes().chain(std::iter::once(0))).collect()
    }

    /// The point of the whole module: what a person exports in one line has to
    /// be there for the command the agent runs in the next.
    #[test]
    fn what_a_shell_changed_is_what_is_taken_on() {
        let path = std::env::var("PATH").expect("a PATH");
        let changes = changed(&dump(&[
            &format!("PATH={path}"),
            "ELMEE_TEST_NEW=fresh",
            // Noise a shell writes on its own account, every time.
            "SHLVL=9",
            "_=/usr/bin/env",
        ]));
        assert_eq!(changes.iter().filter(|(name, _)| name == "ELMEE_TEST_NEW").count(), 1);
        assert!(!changes.iter().any(|(name, _)| name == "SHLVL" || name == "PATH"));
        // Everything else this process has is missing from that dump, so it
        // reads as unset — which is correct, and is why the caller passes the
        // whole dump rather than a line.
        assert!(changes.iter().any(|(_, value)| value.is_none()));

        // A value with a newline in it is why the dump is NUL-separated: it is
        // what pasting a key produces.
        let multiline = changed(&dump(&["ELMEE_TEST_KEY=line one\nline two"]));
        assert_eq!(
            multiline.iter().find(|(name, _)| name == "ELMEE_TEST_KEY").unwrap().1.as_deref(),
            Some("line one\nline two")
        );
    }

    /// A shell that died before it could say anything did not unset the world.
    #[test]
    fn an_empty_dump_changes_nothing() {
        assert!(changed(b"").is_empty());
    }

    #[test]
    fn the_overlay_reaches_a_command() {
        adopt(vec![("ELMEE_OVERLAY_PROBE".into(), Some("yes".into()))]);
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg("printf %s \"$ELMEE_OVERLAY_PROBE\"");
        apply(&mut command);
        let output = command.output().expect("sh runs");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "yes");
        adopt(vec![("ELMEE_OVERLAY_PROBE".into(), None)]);
    }
}
