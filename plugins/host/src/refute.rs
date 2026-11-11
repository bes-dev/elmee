//! An independent pair of eyes on a claim that work is finished.
//!
//! Two shipped harnesses answer "is it done" in ways that each give something
//! up. One asks a small model that sees the conversation and nothing else, so
//! it judges the worker's prose rather than the project — its own
//! documentation says the evaluator does not run commands or read files. The
//! other lets the agent decide about itself under a long oath, which is the
//! claimant grading its own paper.
//!
//! Here the checker is a fresh agent offered the reading tool and nothing else.
//! It cannot run commands because it was never handed the tool that runs them,
//! which is composition rather than policy — but it can
//! open the files, which is more than the first can do, and it is a different
//! context from the one that made the claim, which is more than the second
//! offers. It costs one short run per claim rather than one per turn, and it
//! asks the user for nothing: there is no file to write and no question to
//! answer before a goal can be set.
//!
//! What it still cannot do is make a check execute. A worker that never ran
//! the tests is caught — there is no output to point at — but the tests are
//! not run on its behalf. That is what a project's `before_finish` is for, and
//! why it remains the floor when someone has declared one.

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// How long to wait for a verdict. A checker that reads a few files is quick;
/// one that has gone wandering is not worth blocking a claim on.
const LIMIT: std::time::Duration = std::time::Duration::from_secs(180);

const PROMPT: &str = "You are checking someone else's claim, not doing the work.

Try to REFUTE the claim below. Look for the requirement it does not meet, the file it says was \
changed that was not, the check whose output appears nowhere. Read the project: the claim is about \
the project's current state, not about how convincing the words are.

You cannot run commands. If the claim rests on a check that was never run — tests said to pass with \
no test output anywhere — that is itself a refutation.

Answer with one line, either:
REFUTED: <the requirement that is not met>
UPHELD: <what you read that shows it holds>

Refuse to uphold anything you could not confirm by reading.";

/// `Some(reason)` when the claim does not survive; `None` when it does, or
/// when no verdict could be had — an absent checker must never block work.
pub async fn refute(
    cwd: &std::path::Path,
    model: &str,
    objective: &str,
    evidence: &str,
    max_tokens: u64,
    token_ledger: Option<&std::path::Path>,
) -> Option<String> {
    let Ok(executable) = std::env::current_exe() else { return None };
    let prompt =
        format!("{PROMPT}\n\n<objective>\n{objective}\n</objective>\n\n<claim>\n{evidence}\n</claim>");
    let mut command = Command::new(executable);
    command
        // Reads a claim and tries to break it. Offered one tool, because that
        // is the whole of what breaking a claim about files needs.
        .args(["--model", model, "--tools", "read"])
        .args(["--agent-depth", "1", "--no-session"])
        .args(["--max-billed-tokens", &max_tokens.to_string()])
        .arg("--cwd")
        .arg(cwd);
    // The checker spends from the same pot as every other child: an audit that
    // could mint its own budget would route around the cap it audits under.
    if let Some(ledger) = token_ledger {
        command.args(["--token-ledger"]).arg(ledger);
    }
    let mut child = command
        .arg(prompt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let verdict = tokio::time::timeout(LIMIT, async {
        let mut lines = BufReader::new(stdout).lines();
        let mut last = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            if !line.trim().is_empty() {
                last = line;
            }
        }
        last
    })
    .await
    .ok()?;
    verdict_of(&verdict)
}

/// Read the checker's last line.
///
/// Deliberately one-sided: only an explicit refusal blocks. Silence, a
/// timeout, a checker that could not start, or an answer in no particular
/// shape all mean the same thing — nothing was found against the claim — so a
/// confused checker cannot stop work it never actually disputed.
fn verdict_of(line: &str) -> Option<String> {
    line.trim().strip_prefix("REFUTED:").map(|reason| reason.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_refusal_blocks_a_claim() {
        assert_eq!(
            verdict_of("  REFUTED: three call sites still use the old signature "),
            Some("three call sites still use the old signature".to_owned())
        );
        for benign in ["UPHELD: tests report 81 passing", "", "I am not sure", "refuted: lowercase"] {
            assert_eq!(verdict_of(benign), None, "{benign:?}");
        }
    }
}
