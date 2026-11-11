#![forbid(unsafe_code)]

//! The agent loop, and what it takes for a turn to be done.
//!
//! This crate is deliberately only that. It holds the conversation (`session`),
//! the turn loop (`agent`), and the loop's own semantics: what may run
//! (`interaction`), what counts as proof (`evidence`), and what the agent is
//! working through (`plan`). Everything
//! the loop cannot do alone — talk to a model, touch a file, run a command,
//! remember across sessions — arrives through the model [`provider::Provider`],
//! installed [`plugin::Plugin`] capabilities, and [`interaction::Interactor`]. Implementations live
//! in plugin crates; nothing here depends on them, and Cargo enforces the
//! direction.

pub mod agent;
pub mod builder;
pub mod error;
pub mod evidence;
pub mod interaction;
pub mod message;
pub mod plan;
pub mod plugin;
pub mod provider;
pub mod session;
pub mod text;

pub use agent::{Agent, AgentEvent, TokenLedger};
pub use async_trait::async_trait;
pub use builder::AgentBuilder;
pub use error::{Error, Result};
pub use interaction::{BackgroundEvent, InteractionRequest, InteractionResponse, Interactor};
pub use plugin::{
    CommandEvidence, CommandOutcome, CommandSpec, CompletionClaim, CompletionRequest, Handler, Middleware,
    MiddlewareEffect, MiddlewareEvent, MiddlewarePhase, ModelService, Pending, Plugin, PluginFuture,
    Registrar, Registry, Services, SessionSeed, ToolOutcome, Workspace,
};

/// Seconds since the epoch, for records that carry their age.
pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// A short identifier, unique within this process and unlikely to collide
/// across them, suitable for filenames and protocol ids.
pub fn unique(prefix: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let sequence = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let seed = [started.as_nanos() as u64, u64::from(std::process::id()), sequence];
    let mut value = stable_hash(seed.into_iter().flat_map(u64::to_le_bytes));
    let mut encoded = Vec::with_capacity(13);
    loop {
        encoded.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(value % 36) as usize]);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    encoded.reverse();
    format!("{prefix}-{}", String::from_utf8(encoded).expect("base36 uses ASCII digits"))
}

/// Stable, deliberately non-cryptographic identity for persisted local keys.
pub fn stable_hash(bytes: impl IntoIterator<Item = u8>) -> u64 {
    bytes.into_iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Stable identity for a filesystem path used as a persisted local key.
pub fn stable_path_hash(path: &std::path::Path) -> u64 {
    stable_hash(path.as_os_str().as_encoded_bytes().iter().copied())
}

/// Reply budget shared by context compaction and provider adapters.
pub fn output_budget(context_window_tokens: usize) -> usize {
    const CEILING: usize = 32_000;
    let proportional = (context_window_tokens / 4).max(1);
    CEILING.min(proportional).min(context_window_tokens.saturating_sub(1).max(1))
}

/// The compaction threshold when the model's context window is unknown. A
/// caller that knows the window computes the threshold from it instead, since
/// this constant means half of one window and a tenth of another.
pub const DEFAULT_COMPACT_CHARS: usize = 400_000;

/// The compaction threshold for a known window: the window, less the reply we
/// will ask for, less headroom for the summary request itself. Characters, at
/// the ~4-per-token approximation every harness uses for this.
pub fn compact_chars_for(context_window_tokens: usize) -> usize {
    const HEADROOM_CEILING: usize = 8_192;
    let reply = output_budget(context_window_tokens);
    let headroom = HEADROOM_CEILING.min((context_window_tokens / 8).max(1));
    context_window_tokens.saturating_sub(reply).saturating_sub(headroom) * 4
}

/// The threshold a selected model implies: a declared context window turns it
/// from a guess into that model's actual capacity, and an undeclared one leaves
/// the constant every frontend falls back to.
pub fn compact_chars_for_window(context_window_tokens: Option<usize>) -> usize {
    context_window_tokens.map_or(DEFAULT_COMPACT_CHARS, compact_chars_for)
}

#[cfg(test)]
mod utility_tests {
    use super::*;

    #[test]
    fn the_threshold_follows_the_window_instead_of_a_constant() {
        // The old fixed default is three times a 32k window: compaction would
        // never fire and the request would simply overflow.
        assert!(compact_chars_for(32_000) < DEFAULT_COMPACT_CHARS);
        assert!(compact_chars_for(1_000_000) > DEFAULT_COMPACT_CHARS);
        assert!(compact_chars_for(1_000) > 0); // reserves scale down with small windows
    }

    #[test]
    fn the_context_and_the_reply_it_asks_for_both_fit_the_window() {
        // The failure this guards against: a threshold that leaves less room
        // than the reply we request, so reaching it overflows the window.
        for window in [256, 512, 1_024, 2_048, 4_096, 8_000, 32_000, 128_000, 200_000, 1_000_000] {
            let context = compact_chars_for(window) / 4;
            let reply = output_budget(window);
            assert!(
                context + reply < window,
                "window {window}: context {context} + reply {reply} does not fit"
            );
            assert!(reply <= window / 4, "window {window}: reply {reply} is too greedy");
            assert!(reply < window, "window {window}: reply consumes the whole window");
        }
    }

    #[test]
    fn an_undeclared_window_leaves_the_constant_an_explicit_flag_overrides() {
        // Both applications recognise an explicit --compact-chars by it
        // differing from the default, so an unknown window must return exactly
        // that default and nothing near it.
        assert_eq!(compact_chars_for_window(None), DEFAULT_COMPACT_CHARS);
        assert_eq!(compact_chars_for_window(Some(200_000)), compact_chars_for(200_000));
    }

    #[test]
    fn reply_budget_always_fits_its_window() {
        for window in [1, 2, 2_000, 128_000, 1_000_000] {
            let output = super::output_budget(window);
            assert!(output >= 1 && output <= window.max(1));
            if window > 1 {
                assert!(output < window);
            }
        }
    }
}
