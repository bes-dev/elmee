#![forbid(unsafe_code)]

//! Process-level runtime shared by Elmee frontends.
//!
//! The host turns an assembled agent into a drivable process: [`Driver`]
//! takes turns and answers everything else a frontend asks of a session;
//! [`Runtime`] is what reaches the agent between requests. A frontend owns
//! presentation and how long the process stays. Keeping this outside core
//! prevents TUI and process policy from becoming part of the agent loop.
//!
//! With the `agents` feature the runtime also starts subagents and answers to
//! a name in the project mesh. Without it a session is one agent, and the
//! mesh is not linked at all.

pub mod driver;
mod events;
// One runtime or the other, behind one name: an agent that runs others and
// answers to a name in its project, or an agent on its own.
pub mod preference;
mod refute;
#[cfg(feature = "agents")]
#[path = "agents/mod.rs"]
mod runtime;
#[cfg(not(feature = "agents"))]
#[path = "solo.rs"]
mod runtime;
pub mod session;

pub use driver::Driver;
pub use events::{ChildEvent, Options, Participant, Responder, RuntimeEvent, Steerer, TaskView};
pub use preference::{last_model, remember_model};
pub use runtime::{Runtime, host_tools};
