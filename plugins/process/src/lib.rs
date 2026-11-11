#![forbid(unsafe_code)]

//! Shared process capability for tools, hooks, frontends, and protocol plugins.
//!
//! It centralizes bounded stdout/stderr capture, controlled environment
//! construction, stdin feeding and timeouts. The crate is a reusable mechanism
//! rather than a model-facing handler: higher-level plugins decide which
//! programs may run and expose the relevant operation.

mod capture;
pub mod env;
mod group;

pub use capture::{CommandOutput, command, feed};
pub use group::{Group, scope};

/// Classify a successful command by the kind of verification it performed.
/// This vocabulary belongs beside process execution, not in the agent kernel.
pub fn verification_level(command: &str) -> Option<elmee_core::evidence::VerificationLevel> {
    use elmee_core::evidence::VerificationLevel;

    let words = command
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '-')
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    let has = |word: &str| words.iter().any(|candidate| candidate == word);
    let phrase = |left: &str, right: &str| words.windows(2).any(|pair| pair == [left, right]);
    if ["e2e", "playwright", "cypress", "smoke"].iter().any(|word| has(word)) {
        Some(VerificationLevel::E2e)
    } else if phrase("cargo", "test")
        || phrase("npm", "test")
        || phrase("pnpm", "test")
        || phrase("go", "test")
        || ["pytest", "vitest", "jest", "test-suite", "run-tests"].iter().any(|word| has(word))
    {
        Some(VerificationLevel::Integration)
    } else if ["clippy", "lint", "typecheck"].iter().any(|word| has(word))
        || phrase("cargo", "check")
        || phrase("cargo", "build")
        || words.windows(3).any(|part| part == ["npm", "run", "build"])
    {
        Some(VerificationLevel::Static)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use elmee_core::evidence::VerificationLevel;

    #[test]
    fn command_levels_are_conservative() {
        assert_eq!(verification_level("cargo clippy --all-targets"), Some(VerificationLevel::Static));
        assert_eq!(verification_level("cargo test --workspace"), Some(VerificationLevel::Integration));
        assert_eq!(verification_level("npm run e2e"), Some(VerificationLevel::E2e));
        assert_eq!(verification_level("sed -n 1,20p file"), None);
        assert_eq!(verification_level("ls majestic"), None);
    }
}
