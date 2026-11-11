//! Process entry point for the stock coding agent.
//!
//! The library target is this application's composition root; reusable
//! capabilities remain independent plugins.

fn main() {
    std::process::exit(elmee::run());
}
