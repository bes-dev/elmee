//! Tree-bound evidence for completion claims.
//!
//! A command saying "tests passed" is only true of the tree it observed. Each
//! record therefore carries the snapshot tree hash and is marked stale when
//! the working tree moves. It lives as long as the agent does: proof of an
//! earlier run is not proof of this one, so nothing is read back from disk.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationLevel {
    Unverified,
    Static,
    Integration,
    E2e,
    External,
}

impl VerificationLevel {
    fn executable_rank(self) -> Option<u8> {
        match self {
            Self::Unverified => Some(0),
            Self::Static => Some(1),
            Self::Integration => Some(2),
            Self::E2e => Some(3),
            Self::External => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Unverified => "unverified",
            Self::Static => "static",
            Self::Integration => "integration",
            Self::E2e => "e2e",
            Self::External => "external",
        }
    }

    pub fn parse(value: &str) -> crate::Result<Self> {
        match value {
            "unverified" => Ok(Self::Unverified),
            "static" => Ok(Self::Static),
            "integration" => Ok(Self::Integration),
            "e2e" => Ok(Self::E2e),
            "external" => Ok(Self::External),
            _ => Err(crate::Error::Config(format!(
                "unknown verification level {value:?}; expected unverified, static, integration, e2e, or external"
            ))),
        }
    }
}

/// The kind of entry that records a command the model ran. It says the
/// command finished well on this tree, which is a fact — and a level, which is
/// a guess from the words in the command line: `echo cargo test` reads like a
/// test run and tests nothing. So it is kept, a plan step may cite it as
/// having run, and it never satisfies a required level of verification. Only
/// what the harness ran itself as a configured check, or checked
/// independently, does that.
pub const OBSERVED: &str = "command";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceEntry {
    pub id: String,
    pub kind: String,
    pub claim: String,
    pub command: Option<String>,
    pub tree_hash: Option<String>,
    pub observed_at: u64,
    pub output_digest: String,
    pub level: VerificationLevel,
    pub stale: bool,
}

impl EvidenceEntry {
    /// Whether this is proof of something: current, and not merely a command
    /// whose purpose was inferred from how it was spelled.
    fn proves(&self) -> bool {
        !self.stale && self.kind != OBSERVED
    }
}

#[derive(Default)]
pub struct EvidenceManifest {
    pub current_tree_hash: Option<String>,
    pub entries: Vec<EvidenceEntry>,
}

impl EvidenceManifest {
    pub fn record(
        &mut self,
        kind: impl Into<String>,
        claim: impl Into<String>,
        command: Option<String>,
        tree_hash: Option<String>,
        output: &str,
        level: VerificationLevel,
    ) {
        let kind = kind.into();
        let claim = claim.into();
        let digest = digest(output);
        if self.entries.iter().any(|entry| {
            entry.kind == kind
                && entry.command == command
                && entry.tree_hash == tree_hash
                && entry.output_digest == digest
        }) {
            return;
        }
        let observed_at = crate::now();
        self.entries.push(EvidenceEntry {
            id: format!("evidence-{observed_at}-{}", self.entries.len() + 1),
            kind,
            claim,
            command,
            tree_hash,
            observed_at,
            output_digest: digest,
            level,
            stale: false,
        });
    }

    /// Nothing has been proven about this tree yet, so nothing can go stale.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn refresh(&mut self, current_tree_hash: Option<String>) {
        self.current_tree_hash = current_tree_hash.clone();
        for entry in &mut self.entries {
            entry.stale = entry.tree_hash.is_none() || entry.tree_hash != current_tree_hash;
        }
    }

    pub fn level(&self) -> VerificationLevel {
        self.entries
            .iter()
            .filter(|entry| entry.proves())
            .filter_map(|entry| entry.level.executable_rank().map(|rank| (rank, entry.level)))
            .max_by_key(|(rank, _)| *rank)
            .map(|(_, level)| level)
            .or_else(|| {
                self.entries
                    .iter()
                    .any(|entry| entry.proves() && entry.level == VerificationLevel::External)
                    .then_some(VerificationLevel::External)
            })
            .unwrap_or(VerificationLevel::Unverified)
    }

    pub fn satisfies(&self, required: VerificationLevel) -> bool {
        if required == VerificationLevel::Unverified {
            return true;
        }
        if required == VerificationLevel::External {
            return self
                .entries
                .iter()
                .any(|entry| entry.proves() && entry.level == VerificationLevel::External);
        }
        let required = required.executable_rank().expect("external handled above");
        self.entries
            .iter()
            .filter(|entry| entry.proves())
            .filter_map(|entry| entry.level.executable_rank())
            .any(|rank| rank >= required)
    }
}

/// Identity of one output, for deduplicating repeat observations. Nothing
/// verifies it later, so a fast std hash is enough where SHA-256 was habit.
fn digest(text: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_becomes_stale_when_the_tree_moves() {
        let mut manifest = EvidenceManifest::default();
        manifest.record(
            "completion_hook",
            "tests passed",
            Some("cargo test".into()),
            Some("tree-a".into()),
            "ok",
            VerificationLevel::Integration,
        );
        manifest.refresh(Some("tree-a".into()));
        assert_eq!(manifest.level(), VerificationLevel::Integration);
        manifest.refresh(Some("tree-b".into()));
        assert!(manifest.entries[0].stale);
        assert_eq!(manifest.level(), VerificationLevel::Unverified);
    }

    #[test]
    fn independent_review_does_not_substitute_for_executable_e2e_evidence() {
        let mut manifest = EvidenceManifest::default();
        manifest.record("review", "reviewed", None, Some("tree".into()), "ok", VerificationLevel::External);
        manifest.refresh(Some("tree".into()));
        assert!(manifest.satisfies(VerificationLevel::External));
        assert!(!manifest.satisfies(VerificationLevel::E2e));
    }

    #[test]
    fn external_review_does_not_hide_stronger_executable_status() {
        let mut manifest = EvidenceManifest::default();
        manifest.record("review", "reviewed", None, Some("tree".into()), "ok", VerificationLevel::External);
        manifest.record(
            "completion_hook",
            "tests passed",
            Some("cargo test".into()),
            Some("tree".into()),
            "ok",
            VerificationLevel::Integration,
        );
        manifest.refresh(Some("tree".into()));
        assert_eq!(manifest.level(), VerificationLevel::Integration);
        assert!(manifest.satisfies(VerificationLevel::External));
    }

    /// `echo cargo test` reads like a test run and tests nothing: a command the
    /// model ran is recorded, and proves no level of verification.
    #[test]
    fn a_command_the_model_ran_is_recorded_and_proves_nothing() {
        let mut manifest = EvidenceManifest::default();
        manifest.record(
            OBSERVED,
            "echo cargo test completed successfully",
            Some("echo cargo test".into()),
            Some("tree".into()),
            "cargo test",
            VerificationLevel::Integration,
        );
        manifest.refresh(Some("tree".into()));
        assert_eq!(manifest.entries.len(), 1, "it is kept");
        assert_eq!(manifest.level(), VerificationLevel::Unverified);
        assert!(!manifest.satisfies(VerificationLevel::Static));
        assert!(manifest.satisfies(VerificationLevel::Unverified));
    }
}
