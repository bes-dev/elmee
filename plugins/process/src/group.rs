//! A child and everything it started, ended together.
//!
//! Killing a child kills that one process. A script that started a build, a
//! hook that started a server, a command that started agents of its own: those
//! are the child's children, and they outlived every cancellation here —
//! still writing files and spending money after the person had pressed stop.
//! So a child is put in a process group of its own, and what ends is the
//! group.

use std::time::Duration;

/// How long a group is given to end on being asked before it is made to.
const GRACE: Duration = Duration::from_secs(2);

/// Start `command` in a process group of its own.
pub fn scope(command: &mut tokio::process::Command) -> &mut tokio::process::Command {
    #[cfg(unix)]
    command.process_group(0);
    command.kill_on_drop(true)
}

/// The group a spawned child leads. Dropping it ends the group: asked first,
/// and killed if it is still there after a grace period.
pub struct Group(Option<u32>);

impl Group {
    /// Take charge of the group led by a child started through [`scope`].
    pub fn of(child: &tokio::process::Child) -> Self {
        Self(child.id())
    }

    /// The child ended on its own and took nothing with it that needs ending.
    pub fn release(mut self) {
        self.0 = None;
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.0.and_then(|id| rustix::process::Pid::from_raw(id as i32)) {
            use rustix::process::{Signal, kill_process_group};
            if kill_process_group(group, Signal::TERM).is_err() {
                return; // nobody left in it
            }
            // Off the runtime: this runs from destructors, including the ones
            // that run while the runtime itself is going away.
            std::thread::spawn(move || {
                std::thread::sleep(GRACE);
                let _ = kill_process_group(group, Signal::KILL);
            });
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The failure this exists for: a script's own child went on running
    /// after the script was cancelled.
    #[tokio::test]
    async fn dropping_a_group_ends_what_the_child_started() {
        let marker = std::env::temp_dir().join(format!("elmee-group-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg(format!("(sleep 1; touch {}) & wait", marker.display()));
        let child = scope(&mut command).spawn().unwrap();
        let group = Group::of(&child);
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(group);
        drop(child);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!marker.exists(), "the grandchild ran on after its group was ended");
    }
}
