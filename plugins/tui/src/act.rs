//! Carrying out what the frontend keymap decided.
//!
//! Split from the loop that reads events because the two answer different
//! questions: the loop asks what happened, `keymap` asks what the user meant,
//! and this asks what to do about it — which is the only one of the three that
//! needs to know whether a turn is running, whether the line is empty, or what
//! is queued behind the current dialog.
//!
//! One function per mode the keymap distinguishes, because the modes are the
//! reason an intent means different things: `Esc` cancels a dialog, closes a
//! picker, abandons a search, or aborts a turn, and which of those it is was
//! already decided before anything here runs.

use std::path::PathBuf;

use crossterm::event::KeyEvent;

use super::app::{self, Action, App, Picker, PickerKind};
use super::keymap::{Intent, Mode};
use super::worker::{Cmd, Handle};
use super::{commands, copy_to_clipboard, dialog};
use elmee_host::session::Restore;

pub(super) fn carry_out(
    app: &mut App,
    worker: &Handle,
    mode: Mode,
    intent: Intent,
    key: KeyEvent,
    control: bool,
) {
    match intent {
        Intent::Ignore => {}
        Intent::DialogSubmit
        | Intent::DialogCancel
        | Intent::DialogUp
        | Intent::DialogDown
        | Intent::DialogToggle => in_dialog(app, worker, intent),
        Intent::PickerUp
        | Intent::PickerDown
        | Intent::PickerCancel
        | Intent::PickerAccept
        | Intent::PickerType(_)
        | Intent::PickerErase => {
            in_picker(app, worker, intent);
        }
        Intent::Submit => submit(app, worker),
        _ => while_editing(app, worker, mode, intent, key, control),
    }
}

/// A question. Answering one lets the next in the queue take the
/// screen, so a burst of children asking at once does not lose any of them.
fn in_dialog(app: &mut App, worker: &Handle, intent: Intent) {
    let Some(dialog) = &mut app.dialog else { return };
    let answer = match intent {
        Intent::DialogSubmit => dialog.submit(),
        Intent::DialogCancel => Some(dialog.cancel()),
        // Moving and toggling change the dialog without answering it.
        Intent::DialogUp => return dialog.move_cursor(-1),
        Intent::DialogDown => return dialog.move_cursor(1),
        _ => return dialog.toggle(),
    };
    let Some(answer) = answer else { return };
    let id = dialog.id().to_owned();
    app.dialog =
        app.pending_dialogs.pop_front().map(|(origin, request)| dialog::Dialog::with_origin(request, origin));
    let _ = worker.interaction.send((id, answer));
}

/// How long after a turn ends a stop gesture is still read as meant for it.
const JUST_ENDED: std::time::Duration = std::time::Duration::from_millis(1500);

/// Choosing a model, a session, a prompt to go back to or a branch. What the choice
/// means is the only thing that differs between them.
fn in_picker(app: &mut App, worker: &Handle, intent: Intent) {
    match intent {
        Intent::PickerUp => return app.picker_move(-1),
        Intent::PickerDown => return app.picker_move(1),
        Intent::PickerCancel => return app.picker = None,
        Intent::PickerType(_) | Intent::PickerErase => {
            let typed = if let Intent::PickerType(character) = intent { Some(character) } else { None };
            if let Some(picker) = &mut app.picker {
                picker.narrow(typed);
            }
            return;
        }
        _ => {}
    }
    // Enter on a filter that leaves nothing chooses nothing and stays open.
    if app.picker_selected().is_none() {
        return;
    }
    if let Some((kind, value)) = app.picker_selected() {
        let command = match kind {
            PickerKind::Model => Some(Cmd::SetModel { spec: value }),
            PickerKind::Session => Some(Cmd::OpenSession(PathBuf::from(value))),
            PickerKind::Branch => value.parse().ok().map(Cmd::Checkout),
            PickerKind::Rewind => match value.parse::<u64>() {
                // Where the files were recorded there is a second question;
                // where they were not, only the conversation can go back.
                Ok(prompt) if app.recorded.contains(&prompt) => {
                    app.picker = Some(Picker::new(
                        "what goes back",
                        [
                            (Restore::Everything, "the conversation and the files"),
                            (Restore::Conversation, "the conversation — files stay as they are"),
                            (Restore::Files, "the files — the conversation stays"),
                        ]
                        .map(|(restore, label)| (format!("{prompt} {restore:?}"), label.to_owned()))
                        .to_vec(),
                        PickerKind::Restore,
                        None,
                    ));
                    return;
                }
                Ok(prompt) => Some(Cmd::Rewind { prompt, restore: Restore::Conversation }),
                Err(_) => None,
            },
            PickerKind::Restore => value.split_once(' ').and_then(|(prompt, restore)| {
                let restore = [Restore::Everything, Restore::Conversation, Restore::Files]
                    .into_iter()
                    .find(|known| format!("{known:?}") == restore)?;
                Some(Cmd::Rewind { prompt: prompt.parse().ok()?, restore })
            }),
        };
        if let Some(command) = command {
            let _ = worker.cmd.send(command);
        }
    }
    app.picker = None;
}

/// Everything that happens with the cursor on the prompt.
fn while_editing(app: &mut App, worker: &Handle, mode: Mode, intent: Intent, key: KeyEvent, control: bool) {
    match intent {
        Intent::SearchStep => app.editor.search_step(),
        Intent::SearchBack => app.editor.search_back(),
        Intent::SearchAccept => app.editor.search_accept(),
        Intent::SearchCancel => app.editor.search_cancel(),
        // Ctrl+C is a stop button while a turn runs and a quit confirmation
        // otherwise; Esc is only ever the first of those.
        Intent::Interrupt if app.busy => drop(worker.abort.send(())),
        Intent::Interrupt if app.ctrl_c_armed => app.should_quit = true,
        Intent::Interrupt => app.ctrl_c_armed = true,
        // A report is a different surface and closes on the first press: it is
        // covering the conversation, and asking twice to see it again is a
        // question about nothing.
        Intent::Escape if app.panel.is_some() => app.panel = None,
        // Everything else needs two presses, because all of it destroys
        // something — a typed line, a turn in flight, the last thing said —
        // and a single key that sits next to the one people hit to dismiss a
        // completion is not where any of that belongs. What the second press
        // does is read off the line: text means clear it, and an empty line
        // means there is nothing to clear, so the turn is what is meant.
        Intent::Escape if !app.escape_armed => app.escape_armed = true,
        Intent::Escape => {
            app.escape_armed = false;
            if !app.editor.is_empty() {
                app.editor.clear();
            } else if app.busy {
                let _ = worker.abort.send(());
            } else if app.ended_at.is_some_and(|at| at.elapsed() < JUST_ENDED) {
                // Meant for the turn that ended a moment ago. Taking back the
                // last prompt instead would rewind a conversation over a
                // keypress that arrived half a second late.
            } else {
                // Nothing typed and nothing running: offer the way back, with
                // the last thing said first — the fastest way to correct a
                // prompt and the only key that reaches it.
                let _ = worker.cmd.send(Cmd::RewindList);
            }
        }
        Intent::QuitIfEmpty if app.editor.is_empty() => app.should_quit = true,
        Intent::QuitIfEmpty => {}
        Intent::Newline => app.editor.insert('\n'),
        Intent::Indent => app.editor.insert_str("  "),
        Intent::ToggleExpanded => app.toggle_expanded(),
        Intent::PanelUp => scroll_panel(app, -1),
        Intent::PanelDown => scroll_panel(app, 1),
        Intent::PanelPageUp => scroll_panel(app, -page(app)),
        Intent::PanelPageDown => scroll_panel(app, page(app)),
        Intent::PanelClose => app.panel = None,
        Intent::MenuUp => app.menu_move(-1),
        Intent::MenuDown => app.menu_move(1),
        Intent::MenuComplete => app.complete_menu(),
        Intent::HistoryUp => app.editor.up(),
        Intent::HistoryDown => app.editor.down(),
        Intent::Edit => {
            // The dialog owns the line while it is up; otherwise the prompt does.
            let editor = match (&mut app.dialog, mode) {
                (Some(dialog), Mode::Dialog { .. }) => &mut dialog.editor,
                _ => &mut app.editor,
            };
            editor.handle_key(key.code, control, key.modifiers.contains(crossterm::event::KeyModifiers::ALT));
        }
        _ => {}
    }
}

/// Enter sends the line — and a line that is only the start of a command's
/// name is the command the menu has highlighted.
///
/// Both halves were learnt the hard way. Enter once meant "take the highlighted
/// completion" whenever the menu was open, and the menu is open for as long as
/// anything matches, so a command with an argument could never be sent: Enter
/// put the argument in again and the menu reopened on it. Then Enter meant
/// "send what is typed, always", and `/mo` with `/model` highlighted answered
/// "unknown command: /mo" — as did choosing a row with the arrows. So the menu
/// takes Enter only where typing has not said anything else: while the line is
/// one unfinished command name. A command that wants an argument is completed
/// and left for the argument; one that does not is run.
fn submit(app: &mut App, worker: &Handle) {
    let command = app.highlighted_command();
    if command.is_some() || app.choosing_argument() {
        app.complete_menu();
        if command.is_some_and(|chosen| chosen.hint.is_some()) {
            return;
        }
        // Completing leaves a space for more, and there is no more to come.
        let line = app.editor.text().trim_end().to_owned();
        app.editor.replace(&line);
    }
    let text = app.editor.take();
    if let Some(warning) = app.editor.take_history_warning() {
        app.cells.push(app::Cell::Error(warning));
    }
    match commands::route(app, &text) {
        Action::Worker(cmd) => drop(worker.cmd.send(cmd)),
        Action::Steer(text) => worker.steer.steer(&text),
        Action::Shell(command) => {
            let report = worker.report.clone();
            let cwd = app.cwd.clone();
            tokio::task::spawn_local(async move {
                let (ok, output) = elmee_plugin_process::env::run_here(&cwd, &command).await;
                let _ = report.send(super::worker::Out::Shell { ok, output });
            });
        }
        Action::Expand { template, arguments } => {
            let report = worker.report.clone();
            let cwd = app.cwd.clone();
            tokio::task::spawn_local(async move {
                let prompt = elmee_plugin_command::expand(&template, &arguments, &cwd).await;
                let _ = report.send(super::worker::Out::Expanded(prompt));
            });
        }
        Action::RefreshModels => {
            let (report, cwd) = (worker.report.clone(), app.cwd.clone());
            if let Some(picker) = &mut app.picker {
                picker.pending = true;
            }
            tokio::task::spawn_local(async move {
                let _ = report.send(match elmee_host::session::fresh_models(&cwd).await {
                    Some(Ok(models)) => super::worker::Out::Models(models),
                    // Asked a moment ago: what is on screen is the answer.
                    None => super::worker::Out::Models(Vec::new()),
                    Some(Err(error)) => super::worker::Out::Info(format!("cannot list models: {error}")),
                });
            });
        }
        Action::Copy(text) => copy_to_clipboard(app, &text),
        Action::New => new_session(app, worker),
        Action::ResumeList => resume_list(app),
        Action::Export(path) => export(app, &path),
        Action::Quit => app.should_quit = true,
        Action::None => {}
    }
}

fn new_session(app: &mut App, worker: &Handle) {
    // With no persistence there is no file to make; the context resets either way.
    let path = if app.persist {
        match elmee_host::session::fresh(&app.cwd) {
            Ok(path) => Some(path),
            Err(error) => {
                app.cells.push(app::Cell::Error(format!("cannot create: {error}")));
                return;
            }
        }
    } else {
        None
    };
    let _ = worker.cmd.send(Cmd::NewSession(path));
}

fn resume_list(app: &mut App) {
    // Only the recent ones are read: a summary costs a pass over the file, and
    // nobody picks the fortieth session back out of a list.
    let recent = match elmee_host::session::past(&app.cwd, 20) {
        Ok(recent) => recent,
        Err(error) => {
            app.cells.push(app::Cell::Error(format!("cannot list sessions: {error}")));
            return;
        }
    };
    let items: Vec<(String, String)> = recent
        .into_iter()
        // elmee opens a session file at startup, so a run where nothing was
        // typed leaves one behind. Listing those is listing nothing.
        .filter(|summary| summary.prompts > 0)
        .map(|summary| {
            let subject = elmee_core::text::clip(&summary.subject, 60);
            let prompts = if summary.prompts == 1 { "prompt" } else { "prompts" };
            let label = format!("{} · {} {prompts} · {subject}", summary.age, summary.prompts);
            (summary.path.display().to_string(), label)
        })
        .collect();
    if items.is_empty() {
        app.cells.push(app::Cell::Info("no past sessions for this directory".to_owned()));
        return;
    }
    app.picker = Some(Picker::new("resume a session", items, PickerKind::Session, None));
}

fn export(app: &mut App, path: &str) {
    let written = std::fs::write(path, app.transcript_markdown());
    app.cells.push(match written {
        Ok(()) => app::Cell::Info(format!("transcript written to {path}")),
        Err(error) => app::Cell::Error(format!("export failed: {error}")),
    });
}

fn scroll_panel(app: &App, delta: isize) {
    if let Some(panel) = &app.panel {
        panel.scroll(delta);
    }
}

/// How far a page key moves: what the panel last drew, so the step is the
/// screenful the user is actually looking at.
fn page(app: &App) -> isize {
    app.panel.as_ref().map_or(1, |panel| panel.page.get() as isize)
}
