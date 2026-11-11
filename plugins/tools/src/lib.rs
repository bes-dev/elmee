#![forbid(unsafe_code)]

//! Built-in local coding tools and the workspace adapter.
//!
//! [`tools::ToolSet`] exposes bounded reading, one atomic patch and a shell
//! family over durable runs as one model-tool handler, leaving process
//! capture and project-specific mechanisms to their own crates. Searching
//! and listing are commands rather than tools of their own: a schema over `rg`
//! would take capability away from a model that already knows the real thing.
//! [`WorkspacePlugin`] supplies workspace
//! identity, image attachments and filesystem fingerprints separately, so an
//! agent may have a workspace without inheriting the coding tool set.

mod changed;
pub mod plugin;
pub mod tools;

// Every module keeps its short name: the directories are for the reader, and
// a caller's `elmee_plugin_tools::runs` should not care which file holds it.
pub use tools::runs;

pub use elmee_core::{Error, Result};
pub use plugin::WorkspacePlugin;
