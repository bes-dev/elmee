//! How external events enter the host runtime.
//!
//! One small loop, polling rather than watching: a directory read every few
//! hundred milliseconds costs nothing measurable and needs no file-system
//! notification API, no daemon, and no second thread per source. It carries
//! three unrelated things because they share the one thing that matters —
//! they must reach the agent *between* requests rather than interrupt one:
//! messages that arrived in the inbox, long commands that have finished, and
//! the periodic refresh of what the frontend displays.

use std::{sync::atomic::Ordering, time::Duration};

use elmee_core::BackgroundEvent;

use super::{Runtime, finished_runs};

impl Runtime {
    /// Say so when the poller has died: a panic in it otherwise turns into an
    /// inbox that silently never fills and completions nobody hears about.
    pub(super) fn surface_dead_mailbox(&mut self) {
        let died = self.mailbox.as_ref().is_some_and(tokio::task::JoinHandle::is_finished)
            && !self.shutdown.load(Ordering::Relaxed);
        if died {
            self.mailbox = None;
            self.inbox.defer(BackgroundEvent {
                id: elmee_core::unique("fault"),
                prompt: "<runtime_fault>the inbox poller stopped unexpectedly; peer messages and \
                         finished background runs will not be delivered for the rest of this \
                         session</runtime_fault>"
                    .into(),
            });
        }
    }

    pub(super) fn start_mailbox(&mut self) {
        let base = self.base.clone();
        let cwd = self.cwd.clone();
        let state = self.state.clone();
        let events = self.events.clone();
        let inbox = self.inbox.clone();
        let shutdown = self.shutdown.clone();
        let reporter = self.reporter();
        self.mailbox = Some(tokio::task::spawn_local(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(250));
            let mut beats = 0;
            let mut inbox_unreadable = false;
            loop {
                tick.tick().await;
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                reporter.retry_replies();
                let name = &reporter.name;
                beats += 1;
                // The registration is written once at startup and removed on
                // exit; only the view refreshes on a timer.
                if beats % 8 == 0 {
                    super::emit_snapshot(&state, &events, &base, name);
                }
                let finished = finished_runs(&cwd, &mut state.lock().unwrap().reported);
                for event in finished {
                    inbox.push(event);
                }
                let inspection = match elmee_plugin_mesh::inspect_inbox(&base, name) {
                    Ok(inspection) => {
                        if inbox_unreadable {
                            eprintln!("elmee: mesh inbox for @{name} is readable again");
                        }
                        inbox_unreadable = false;
                        inspection
                    }
                    Err(error) => {
                        if !inbox_unreadable {
                            eprintln!("elmee: cannot read mesh inbox for @{name}: {error}");
                            inbox.push(BackgroundEvent {
                                id: elmee_core::unique("fault"),
                                prompt: "<runtime_fault>mesh inbox inspection failed. Peer message delivery is unavailable; the host will retry automatically. Background runs remain monitored. Inspect the host diagnostic log.</runtime_fault>".into(),
                            });
                        }
                        inbox_unreadable = true;
                        continue;
                    }
                };
                for (path, envelope) in inspection.messages {
                    let mut state = state.lock().unwrap();
                    if state.seen.contains(&envelope.id) {
                        continue;
                    }
                    match elmee_plugin_mesh::replay_completed(&base, name, &envelope) {
                        Ok(true) => {
                            let _ = std::fs::remove_file(path);
                            continue;
                        }
                        Ok(false) => {}
                        Err(error) => {
                            eprintln!("elmee: mesh recovery {} failed: {error}", envelope.id);
                            continue;
                        }
                    }
                    if state.seen.insert(envelope.id.clone()) {
                        state.pending_messages.insert(envelope.id.clone(), envelope.clone());
                        let prompt = envelope.agent_input();
                        inbox.push(BackgroundEvent { id: envelope.id, prompt });
                    }
                }
            }
        }));
    }
}
