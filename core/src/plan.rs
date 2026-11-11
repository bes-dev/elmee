//! The plan an agent is working through, and what lets a step be called done.
//!
//! One owner: the agent. The frontend is sent the whole plan whenever it
//! changes, including when it empties, and draws what it was sent — it keeps no
//! opinion of its own. The previous arrangement had two copies and no way to
//! clear either, so a half-finished plan hung in the footer over the next three
//! unrelated tasks, and the model lost its own plan at the first compaction
//! because the only durable trace was one tool result.
//!
//! What makes this worth having rather than a place to write intentions: a step
//! cannot be marked done on the model's own word alone. `done` may cite a
//! command, and the harness looks that command up in the evidence this session
//! actually recorded — a command that ran and exited zero. It is a lookup, not
//! a judgement: nothing is generated, nothing is run, and a citation that does
//! not check out does not block the work, it just stops the step from claiming
//! to be proven.
//!
//! Neither Claude Code nor Codex makes a plan binding — Codex says outright
//! that an approved plan is a starting point and not a contract. This does not
//! either. What it does is refuse to let "done" mean nothing at all.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Pending,
    /// The one being worked on. Exactly one, which is what makes a plan a
    /// sequence rather than a wish list — and what lets a watcher see where the
    /// agent is rather than only how far it got.
    Doing,
    Done,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Step {
    pub step: String,
    #[serde(default)]
    pub status: Status,
    /// A command that backs `done`, if one can.
    #[serde(default)]
    pub proof: Option<String>,
    /// Whether `proof` names something this session really ran and passed. Set
    /// here and never read from the model: a claim that scores itself is the
    /// thing this exists to replace.
    #[serde(default, skip_deserializing)]
    pub proven: bool,
}

impl Step {
    pub fn open(&self) -> bool {
        self.status != Status::Done
    }
}

/// Take a plan the model just wrote, check what can be checked, and say what
/// stands.
///
/// The report is deliberately short. The old one echoed the whole plan back on
/// every call, which spent tokens restating what the model had written one
/// message earlier; the plan now survives on its own, so what comes back is the
/// shape of it and any complaint.
pub fn apply(steps: Vec<Step>, evidence: &crate::evidence::EvidenceManifest) -> Result<(Vec<Step>, String)> {
    if steps.is_empty() {
        return Err(Error::Tool("plan needs a non-empty steps array".into()));
    }
    let doing = steps.iter().filter(|step| step.status == Status::Doing).count();
    if doing > 1 {
        return Err(Error::Tool(format!(
            "{doing} steps are marked doing; exactly one step is in progress at a time"
        )));
    }
    let mut unproven = Vec::new();
    let steps: Vec<Step> = steps
        .into_iter()
        .map(|mut step| {
            step.proven = step.status == Status::Done
                && step.proof.as_deref().is_some_and(|proof| ran(evidence, proof));
            if step.status == Status::Done && step.proof.is_some() && !step.proven {
                unproven.push(step.proof.clone().unwrap_or_default());
            }
            step
        })
        .collect();

    let done = steps.iter().filter(|step| step.status == Status::Done).count();
    let mut report = format!("{done}/{} done", steps.len());
    if doing == 1 {
        report.push_str(", 1 in progress");
    }
    if !unproven.is_empty() {
        report.push_str(&format!(
            ". No command matching this ran here: {} — cite the command itself, or run it",
            unproven.iter().map(|proof| proof.trim_end_matches('.')).collect::<Vec<_>>().join("; ")
        ));
    }
    Ok((steps, report))
}

/// Whether a command like this one ran and passed in this session.
///
/// Overlap in either direction, because both happen and both are honest. A
/// model cites `cargo test` for a step a longer `cargo test --workspace`
/// proved; it also wraps the command in a sentence — "`cargo fmt --check`
/// exited clean" — and the recorded command sits inside that. Demanding an
/// exact match teaches it to stop citing rather than to cite better, which was
/// visible the first time this ran: four honest citations in a row rejected.
fn ran(evidence: &crate::evidence::EvidenceManifest, proof: &str) -> bool {
    let proof = proof.trim();
    !proof.is_empty()
        && evidence.entries.iter().filter(|entry| entry.kind == crate::evidence::OBSERVED).any(|entry| {
            entry.command.as_deref().is_some_and(|command| command.contains(proof) || proof.contains(command))
        })
}

/// The steps still open, for the one moment they would otherwise be lost.
///
/// Compaction throws the conversation away and keeps a summary; without this
/// the plan goes with it, which is exactly when it was worth most.
pub fn carry(steps: &[Step]) -> String {
    let open: Vec<&Step> = steps.iter().filter(|step| step.open()).collect();
    match open.is_empty() {
        true => String::new(),
        false => format!(
            "\n\nStill to do, from the plan in force when this was summarized:\n{}",
            open.iter()
                .map(|step| match step.status {
                    Status::Doing => format!("- {} (in progress)", step.step),
                    _ => format!("- {}", step.step),
                })
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{EvidenceManifest, VerificationLevel};

    fn step(text: &str, status: Status, proof: Option<&str>) -> Step {
        Step { step: text.into(), status, proof: proof.map(str::to_owned), proven: false }
    }

    /// The point of the whole module: "done" cannot mean only that the model
    /// said so. A citation is checked against what this session really ran.
    #[test]
    fn done_is_backed_by_something_that_actually_ran() {
        let mut evidence = EvidenceManifest::default();
        evidence.record(
            "command",
            "cargo test --workspace completed successfully",
            Some("cargo test --workspace".into()),
            None,
            "ok",
            VerificationLevel::Integration,
        );

        let (steps, report) = apply(
            vec![
                step("wire it up", Status::Done, Some("cargo test")),
                step("write the docs", Status::Done, Some("mkdocs build")),
                step("ship", Status::Doing, None),
            ],
            &evidence,
        )
        .unwrap();

        assert!(steps[0].proven, "cited a command that ran and passed");
        assert!(!steps[1].proven, "cited one that never ran");
        assert!(report.contains("mkdocs build"), "and the report says which: {report}");

        // The citation a model actually writes wraps the command in a sentence,
        // and the recorded command sits inside it. Rejecting that taught it to
        // stop citing — four honest citations in a row were refused the first
        // time this ran.
        let (wrapped, _) = apply(
            vec![step("wire it up", Status::Done, Some("`cargo test --workspace` exited clean"))],
            &evidence,
        )
        .unwrap();
        assert!(wrapped[0].proven, "overlap counts in either direction");
        assert!(report.contains("2/3 done") && report.contains("1 in progress"), "{report}");

        // A plan is a sequence, not a wish list.
        let two = apply(vec![step("a", Status::Doing, None), step("b", Status::Doing, None)], &evidence);
        assert!(two.unwrap_err().to_string().contains("exactly one step"));
    }

    /// Compaction is the one moment a plan is lost, so it is the one moment it
    /// has to be carried.
    #[test]
    fn what_is_still_open_survives_the_context_being_thrown_away() {
        let steps = vec![
            step("done already", Status::Done, None),
            step("halfway", Status::Doing, None),
            step("not started", Status::Pending, None),
        ];
        let carried = carry(&steps);
        assert!(carried.contains("halfway (in progress)"), "{carried}");
        assert!(carried.contains("not started") && !carried.contains("done already"), "{carried}");
        assert!(carry(&steps[..1]).is_empty(), "a finished plan carries nothing");
    }
}
