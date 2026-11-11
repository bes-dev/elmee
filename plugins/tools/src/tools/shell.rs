//! The shell as a family: starting a command, following it while answering it,
//! ending it.
//!
//! One tool would be enough if commands always finished and never asked
//! anything. They do neither, so the other two exist to carry on a conversation
//! with a run and to end it — the same run in every case, named by the id
//! `exec` handed back.

use std::{path::Path, time::Duration};

use super::{
    TOOL_TIMEOUT,
    schema::{ExecArgs, StopRunArgs, WriteRunArgs},
};
use crate::{Result, runs};

/// Start a command and wait for as long as waiting is still useful.
///
/// Running out of time is a demotion here, not a death. The log is already
/// collecting and the run is already registered, so there is nothing to do but
/// stop waiting and say how to carry on — and the agent hears about the ending
/// through its inbox, like anything else that finishes while it is busy.
pub(super) async fn exec(cwd: &Path, args: &ExecArgs) -> Result<String> {
    let limit = args.max_output_bytes.unwrap_or(runs::MAX_RUN_OUTPUT);
    let yield_for = args.yield_ms.map_or(*TOOL_TIMEOUT, Duration::from_millis);
    let mut started = runs::start(cwd, &args.command).await?;
    let id = started.run.id.clone();
    if yield_for.is_zero() {
        return Ok(format!(
            "Started run {id}; nothing is waiting on it. Use write_run to see what it writes and \
             to answer it, stop_run to end it."
        ));
    }
    // A wait that is dropped is a turn someone stopped: the command goes with
    // it. A wait that runs out is different — that run carries on by design.
    let mut waiting = Abandon { cwd, id: &id, armed: true };
    let waited = tokio::time::timeout(yield_for, started.wait()).await;
    waiting.armed = false;
    match waited {
        Ok(result) => {
            result?;
            let output = runs::output_limited(cwd, &id, limit);
            runs::forget(cwd, &id);
            Ok(output)
        }
        // A zero wait for what it has already written: everything, and the
        // offset moves, so the next write_run continues rather than repeats.
        Err(_) => {
            let seen = runs::follow(cwd, &id, Duration::ZERO, limit).await?;
            Ok(format!(
                "{}\n[run {id} is still going after {}ms and was not stopped — write_run for the \
                 rest or to answer it, stop_run to end it]",
                seen.output,
                yield_for.as_millis()
            ))
        }
    }
}

struct Abandon<'a> {
    cwd: &'a Path,
    id: &'a str,
    armed: bool,
}

impl Drop for Abandon<'_> {
    fn drop(&mut self) {
        if self.armed {
            runs::abandon(self.cwd, self.id);
        }
    }
}

/// Type to a run, then read it back — or, with nothing to say, only read it
/// back.
///
/// Writing and following were two tools until it turned out they were one
/// conversation: an answer is only worth sending because of what comes after
/// it, and what comes after it is what waiting ever returned. Nothing to say is
/// therefore not an empty call but a plain look, and the run's own read offset
/// keeps either kind moving forward instead of repeating.
pub(super) async fn feed(cwd: &Path, args: &WriteRunArgs) -> Result<String> {
    let id = &args.id;
    let wrote = match args.input.as_deref().filter(|input| !input.is_empty()) {
        Some(input) => {
            runs::feed(cwd, id, input, args.newline.unwrap_or(true)).await?;
            true
        }
        None => false,
    };
    // Typing and waiting are different intents, so they cannot share a default.
    // A poll is asking to be woken when there is more, and waiting minutes for
    // that is the point. A line typed at a prompt wants its reply — and a
    // process that answers silently would otherwise hold the turn for the whole
    // watching budget for having said nothing.
    let unhurried = if wrote { Duration::from_secs(2) } else { *TOOL_TIMEOUT };
    let yield_for = args.yield_ms.map_or(unhurried, Duration::from_millis);
    let progress = runs::follow(cwd, id, yield_for, runs::MAX_RUN_OUTPUT).await?;
    Ok(match progress.exit {
        Some(code) => format!("{}\n[run {id} finished with exit status {code}]", progress.output),
        None if progress.output.is_empty() => {
            format!("[run {id} is still going and has written nothing new]")
        }
        None => format!("{}\n[run {id} is still going]", progress.output),
    })
}

pub(super) fn stop(cwd: &Path, args: &StopRunArgs) -> Result<String> {
    runs::stop(cwd, &args.id, args.force.unwrap_or(false))?;
    Ok(format!("Stop signal sent to run {}", args.id))
}

/// What a frontend should say before the first prompt, rather than letting the
/// agent discover it mid-task.
///
/// Searching is the shell's now, and ripgrep is what searching means: it reads
/// `.gitignore`, it is fast enough to be used casually, and it is what the
/// person at the keyboard has. Looked up on `PATH` rather than run — asking a
/// tool for its version is the obvious probe and the wrong one, because a
/// wrapper on `PATH` can answer for a program that is not there.
pub fn missing_ripgrep() -> Option<&'static str> {
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let present = *FOUND.get_or_init(|| {
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|directory| directory.join("rg").is_file()))
    });
    (!present).then_some(
        "ripgrep (rg) is not installed, so searching falls back to grep and find — install it \
         (brew install ripgrep, apt install ripgrep, cargo install ripgrep) for a faster search \
         that honours .gitignore.",
    )
}
