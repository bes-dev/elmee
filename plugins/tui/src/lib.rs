#![forbid(unsafe_code)]

//! The interactive terminal frontend, rendered pi-style as a flowing document in
//! the main screen (see `render`): no alternate screen, no reserved rows —
//! the input hugs the transcript and finished lines become ordinary terminal
//! history. A dedicated OS thread feeds crossterm events into the async
//! loop; the agent runs in a local task behind channels (`worker`).

mod act;
mod app;
mod bottom;
mod cells;
mod commands;
mod dialog;
mod highlight;
mod input;
mod keymap;
mod layout;
mod markdown;
mod pager;
mod render;
mod status;
mod text;
mod theme;
mod ui;
mod worker;

use std::io::Write;
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use elmee_core::Result;
use text::Line;
use tokio::sync::mpsc;

use app::App;
use render::LiveArea;
use worker::{Cmd, Handle, Out};

use elmee_host::{Driver, RuntimeEvent};

/// What the interface shows about the session it was handed, decided by the
/// application that composed it.
pub struct Options {
    pub model: String,
    pub cwd: std::path::PathBuf,
    /// Whether the conversation is being written to a file.
    pub persistent_session: bool,
    /// The compaction threshold, so the status line can say how full the
    /// context is; `None` when the agent never compacts.
    pub compact_chars: Option<usize>,
}

pub async fn run(
    driver: Driver,
    options: &Options,
    runtime_events: mpsc::UnboundedReceiver<RuntimeEvent>,
) -> Result<i32> {
    theme::init(&options.cwd);
    let session =
        driver.agent.session_path().map_or_else(|| "ephemeral".to_owned(), |path| path.display().to_string());
    let mut app = App::new(
        options.model.clone(),
        session,
        commands::load(&options.cwd, driver.agent.plugin_commands()),
        options.cwd.clone(),
        options.persistent_session,
    );
    // A conversation being continued is shown, not merely loaded.
    app.cells.extend(app::transcript(driver.agent.messages()));
    app.context_chars = driver.agent.context_chars();
    app.compact_chars = options.compact_chars;
    let worker = worker::spawn(driver, runtime_events);

    enable_raw_mode()?;
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = std::io::stdout().write_all(b"\x1b[?25h\r\n");
        hook(info);
    }));
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);

    let mut area = LiveArea::default();
    let result = event_loop(&mut area, &mut app, worker).await;

    // Print whatever was still streaming so nothing is lost on exit.
    app.cells.iter_mut().for_each(|cell| {
        if let app::Cell::Assistant { done, .. } | app::Cell::Reasoning { done, .. } = cell {
            *done = true;
        }
    });
    let mut out = std::io::stdout();
    let statics = take_finalized(&mut app, true, terminal_size().0);
    let _ = area.frame(&mut out, &statics, &[Line::default()], (0, 0), terminal_size().0);
    let _ = area.finish(&mut out);
    let _ = execute!(out, DisableBracketedPaste);
    Attention::from_env().title(&mut out, "done");
    let _ = disable_raw_mode();
    result.map(|()| 0)
}

/// Collect the lines of newly finished cells; they are printed once and then
/// live only in the terminal's own history.
fn take_finalized(app: &mut App, force: bool, width: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    while app.flushed < app.cells.len() {
        // A reply is the one cell that becomes history while it is still being
        // written: its finished lines go up as they arrive, so a long answer
        // can be read from the top instead of scrolling past in the live area
        // until the model stops.
        // A streaming cell that is no longer the last one is finished by
        // construction: nothing can append to it. Believing `done` alone was
        // the bug — the model interleaves reasoning between two answers, the
        // first answer was never marked, and the flush pointer stopped there
        // for the rest of the session. Everything behind it stayed out of the
        // scrollback and lived only in the height-bounded live area, which is
        // what "the output goes fragmentary and things get lost" was.
        let last = app.flushed + 1 == app.cells.len();
        let gap = ui::gap(app.flushed.checked_sub(1).map(|at| &app.cells[at]), &app.cells[app.flushed]);
        if let app::Cell::Assistant { text, done, flushed } = &mut app.cells[app.flushed] {
            let whole = *done || force || !last;
            let (ready, reached) = ui::flushable(text, *flushed, whole);
            if gap && *flushed == 0 && !ready.is_empty() {
                lines.push(Line::default());
            }
            // Wrapped here, not by the terminal: a row the terminal wraps is
            // two rows where the live area's arithmetic counted one, and
            // every frame after it landed a line off, leaving the tail of a
            // long line standing where the next one should have been.
            lines.extend(ready.into_iter().flat_map(|line| ui::hanging(line, width)));
            *flushed = reached;
            if !whole {
                break;
            }
        } else {
            let cell = &app.cells[app.flushed];
            if !force && !cell.finalized() {
                break;
            }
            if gap {
                lines.push(Line::default());
            }
            lines.extend(ui::rendered_cell(cell, width, &app.cwd));
        }
        app.flushed += 1;
    }
    lines
}

fn terminal_size() -> (usize, usize) {
    crossterm::terminal::size()
        .map_or((80, 24), |(columns, rows)| (columns.max(20) as usize, rows.max(6) as usize))
}

async fn event_loop(area: &mut LiveArea, app: &mut App, mut worker: Handle) -> Result<()> {
    let (term_tx, mut term_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            if term_tx.send(event).is_err() {
                break;
            }
        }
    });
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut out = std::io::stdout().lock();
    let attention = Attention::from_env();
    let mut was_busy = false;
    let mut turn_started = Instant::now();
    attention.title(&mut out, "ready");
    // Coalesce redraws: bursts of stream deltas mark the UI dirty, and frames
    // go out at most once per interval instead of once per event batch.
    const FRAME_INTERVAL: Duration = Duration::from_millis(16);
    let mut dirty = true;
    let mut last_frame = Instant::now() - FRAME_INTERVAL;
    let (mut width, mut rows) = terminal_size();

    loop {
        if std::mem::take(&mut app.clear_screen) {
            // An overlay drawn over a screen that no longer exists is the one
            // thing a clear must not leave behind.
            app.panel = None;
            app.expanded_view = None;
            // A cell that is done but has not reached the scrollback yet would
            // otherwise print straight back onto the screen just cleared. Only
            // finished ones are dropped: a reply still being written keeps every
            // line it has, because those are what the live area is showing.
            while app.flushed < app.cells.len() && app.cells[app.flushed].finalized() {
                app.flushed += 1;
            }
            // Scrolled away rather than erased. `\x1b[3J` drops the scrollback,
            // and someone running an unbounded history wants to scroll back
            // through it — deleting what they kept on purpose is not ours to do.
            // A screenful of newlines pushes the conversation up into that
            // history and leaves a blank screen, which is the part we own.
            let _ = write!(out, "\x1b[r\x1b[0m{}\x1b[H", "\n".repeat(rows));
            let _ = out.flush();
            // Both the drawn rows and the cursor are gone, and the sequence
            // above homed it — so the live area must forget where it parked.
            area.reset();
            app.cells.append(&mut app.after_clear);
            dirty = true;
        }
        if dirty && last_frame.elapsed() >= FRAME_INTERVAL {
            let statics = take_finalized(app, false, width);
            let (live, cursor) = ui::live_view(app, width, rows.saturating_sub(1));
            area.frame(&mut out, &statics, &live, cursor, width)?;
            dirty = false;
            last_frame = Instant::now();
        }
        tokio::select! {
            event = term_rx.recv() => {
                let Some(event) = event else { break };
                for event in std::iter::once(event).chain(std::iter::from_fn(|| term_rx.try_recv().ok())) {
                    if let Event::Resize(new_width, new_rows) = event {
                        (width, rows) = (new_width.max(20) as usize, new_rows.max(6) as usize);
                        area.invalidate();
                    } else {
                        handle_terminal_event(app, &worker, event);
                    }
                }
                dirty = true;
            }
            message = worker.out.recv() => {
                let Some(message) = message else { break };
                dispatch(app, &worker, message);
                while let Ok(message) = worker.out.try_recv() {
                    dispatch(app, &worker, message);
                }
                if app.busy != was_busy {
                    was_busy = app.busy;
                    if app.busy {
                        turn_started = Instant::now();
                        attention.title(&mut out, "thinking…");
                    } else {
                        attention.title(&mut out, "ready");
                        attention.ring(&mut out, turn_started.elapsed());
                    }
                }
                dirty = true;
            }
            _ = tick.tick(), if app.busy || app.has_running_tasks() => {
                app.spinner = app.spinner.wrapping_add(1);
                dirty = true;
            }
            // Dirty but throttled: wake exactly when the frame window opens.
            _ = tokio::time::sleep(FRAME_INTERVAL.saturating_sub(last_frame.elapsed())), if dirty => {}
        }
        if app.should_quit {
            break;
        }
    }
    Ok(())
}

/// What the terminal is told beyond the frame: its title, and a bell when a
/// long turn ends, for the person who went to do something else meanwhile.
/// Both are what a terminal does with them — a tab title, a dock bounce, a
/// desktop notification — and nothing where it does nothing.
struct Attention {
    title: bool,
    /// A turn shorter than this ends without a sound; `None` never rings.
    ring_after: Option<Duration>,
}

impl Attention {
    fn from_env() -> Self {
        let off =
            |name: &str| std::env::var(name).is_ok_and(|value| matches!(value.trim(), "0" | "false" | "off"));
        let seconds = std::env::var("ELMEE_TUI_NOTIFY_SECS")
            .ok()
            .and_then(|value| value.trim().parse::<f64>().ok())
            .unwrap_or(15.0);
        Self {
            title: !off("ELMEE_TUI_TITLE"),
            ring_after: (seconds > 0.0).then(|| Duration::from_secs_f64(seconds)),
        }
    }

    fn title(&self, out: &mut impl Write, state: &str) {
        if self.title {
            let _ = write!(out, "\x1b]2;elmee — {state}\x07");
        }
    }

    /// The bell, and the two notification sequences terminals answer to:
    /// OSC 9 (iTerm2, ConEmu, Ghostty) and OSC 777 (WezTerm, foot, rxvt).
    fn ring(&self, out: &mut impl Write, took: Duration) {
        if self.ring_after.is_some_and(|after| took >= after) {
            let _ = write!(
                out,
                "\x07\x1b]9;elmee: finished after {}\x07\x1b]777;notify;elmee;finished after {}\x07",
                elmee_core::text::how_long_for(took.as_secs()),
                elmee_core::text::how_long_for(took.as_secs())
            );
            let _ = out.flush();
        }
    }
}

/// Pipe text into the platform clipboard tool (pbcopy / xclip / wl-copy).
fn copy_to_clipboard(app: &mut App, text: &str) {
    for tool in [&["pbcopy"][..], &["xclip", "-selection", "clipboard"], &["wl-copy"]] {
        let spawned = std::process::Command::new(tool[0])
            .args(&tool[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Ok(mut child) = spawned {
            let ok = child
                .stdin
                .take()
                .and_then(|mut stdin| Write::write_all(&mut stdin, text.as_bytes()).ok())
                .is_some()
                && child.wait().map(|status| status.success()).unwrap_or(false);
            if ok {
                app.cells.push(app::Cell::Info("copied the last reply to the clipboard".to_owned()));
                return;
            }
        }
    }
    app.cells.push(app::Cell::Error("no clipboard tool found (pbcopy/xclip/wl-copy)".to_owned()));
}

fn dispatch(app: &mut App, worker: &Handle, out: Out) {
    // An expanded command prompt is sent the way a typed one would have been
    // at the moment it is ready: to the worker when idle, as steering when a
    // turn is already running.
    if let Out::Expanded(prompt) = out {
        if app.busy {
            app.cells.push(app::Cell::Info("steering — the agent sees this at its next step".to_owned()));
            worker.steer.steer(&prompt);
        } else {
            let _ = worker.cmd.send(Cmd::Prompt(prompt));
        }
        return;
    }
    if let Some(queued) = app.apply(out) {
        let _ = worker.cmd.send(Cmd::Prompt(queued));
    }
}

fn handle_terminal_event(app: &mut App, worker: &Handle, event: Event) {
    let text_before = app.editor.text().to_owned();
    match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => {
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            // A repeat of the same key is what confirms it, so neither of the
            // two-press keys disarms itself; everything else disarms both.
            if !(control && key.code == KeyCode::Char('c')) {
                app.ctrl_c_armed = false;
            }
            if key.code != KeyCode::Esc {
                app.escape_armed = false;
            }
            let mode = keymap::mode(app);
            act::carry_out(app, worker, mode, keymap::resolve(mode, key), key, control);
        }
        Event::Paste(text) => {
            if let Some(dialog) = &mut app.dialog
                && dialog.typing
            {
                dialog.editor.insert_str(&text);
            } else {
                app.editor.insert_str(&text);
            }
        }
        _ => {}
    }

    if app.editor.text() != text_before {
        app.menu_index = 0; // editing restarts the selection
        if app.editor.text().contains('@') && app.wants_files() {
            // One listing per session, started on the first mention and
            // answered whenever it is ready.
            let (report, cwd) = (worker.report.clone(), app.cwd.clone());
            tokio::task::spawn_local(async move {
                let listing = tokio::process::Command::new("rg")
                    .args(["--files", "--color", "never"])
                    .current_dir(&cwd)
                    .output()
                    .await;
                let text = listing
                    .map(|listing| String::from_utf8_lossy(&listing.stdout).into_owned())
                    .unwrap_or_default();
                let files = text.lines().take(20_000).map(str::to_owned).collect();
                let _ = report.send(Out::Files(files));
            });
        }
    }
}
