#![forbid(unsafe_code)]

//! What a repository teaches its agents: the assembled prompt, the skills it
//! offers, the roles it declares, the hooks it gates with, and what the agent
//! has kept about it.
//!
//! Discovery is separated from enforcement: context/skills/roles are read by
//! a profile, while directives, hooks and memory install as loop middleware.
//! Project-owned executable behavior is accepted only when the application
//! explicitly trusts the checkout.

pub mod context;
pub mod directives;
pub mod hooks;
pub mod memory;
pub mod roles;
pub mod skills;

pub use directives::Directives;
pub use elmee_core::{Error, Result};
pub use memory::MemoryPlugin;
