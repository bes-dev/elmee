//! Tests for the terminal application module beside this one.
//!
//! A child module, so it still reaches the private items it is testing and
//! is still compiled out of a release build — the only thing it stops doing is
//! competing with the code for the reader.

use super::*;

fn step(text: &str, status: elmee_core::plan::Status) -> elmee_core::plan::Step {
    elmee_core::plan::Step { step: text.into(), status, proof: None, proven: false }
}

#[test]
fn streams_text_into_one_cell_and_tracks_tools() {
    let mut app =
        App::new("m".into(), "s".into(), super::super::commands::builtin(), std::env::temp_dir(), false);
    app.apply(Out::Started);
    app.apply(Out::Event(AgentEvent::Text { text: "Hel".into() }));
    app.apply(Out::Event(AgentEvent::Text { text: "lo".into() }));
    app.apply(Out::Event(AgentEvent::ToolStart {
        id: "1".into(),
        name: "exec".into(),
        arguments: "{}".into(),
    }));
    app.apply(Out::Event(AgentEvent::ToolEnd {
        id: "1".into(),
        name: "read".into(),
        ok: true,
        duration_ms: 1,
        output: "ok".into(),
    }));
    app.apply(Out::Done { error: None, aborted: false });
    assert!(matches!(&app.cells[1], Cell::Assistant { text, done: true, .. } if text == "Hello"));
    assert!(matches!(&app.cells[2], Cell::Tool { output: Some(output), ok: true, .. } if output == "ok"));
}

#[test]
fn menu_navigates_wraps_and_completes_selected() {
    fn selected(app: &App) -> Option<String> {
        let items = app.menu_items();
        (!items.is_empty()).then(|| items[app.menu_index % items.len()].value.clone())
    }
    let mut app =
        App::new("m".into(), "s".into(), super::super::commands::builtin(), std::env::temp_dir(), false);
    for character in "/c".chars() {
        app.editor.insert(character);
    }
    // In declaration order: clear, compact, copy.
    assert_eq!(app.menu_items().len(), 3);
    app.menu_move(1);
    assert_eq!(selected(&app).as_deref(), Some("/compact"));
    app.menu_move(-2); // wraps to the end
    assert_eq!(selected(&app).as_deref(), Some("/copy"));
    app.menu_move(1); // wraps forward to the start
    assert_eq!(selected(&app).as_deref(), Some("/clear"));
    app.menu_move(-1);
    app.complete_menu();
    assert_eq!(app.editor.text().trim_end(), "/copy");
}

#[test]
fn at_mentions_complete_against_project_files() {
    let mut app =
        App::new("m".into(), "s".into(), super::super::commands::builtin(), std::env::temp_dir(), false);
    app.files = vec!["src/main.rs".into(), "src/lib.rs".into(), "README.md".into()];
    app.editor.insert_str("look at @main");
    let offered: Vec<String> = app.menu_items().into_iter().map(|item| item.value).collect();
    assert_eq!(offered, vec!["@src/main.rs".to_owned()]);
    app.complete_menu();
    assert_eq!(app.editor.text(), "look at @src/main.rs ");
    assert!(app.menu_items().is_empty()); // the completed mention closes the menu
}

#[test]
fn overlay_toggles_open_and_closed() {
    let mut app =
        App::new("m".into(), "s".into(), super::super::commands::builtin(), std::env::temp_dir(), false);
    app.apply(Out::Started);
    app.apply(Out::Event(AgentEvent::ToolStart {
        id: "1".into(),
        name: "read".into(),
        arguments: "{}".into(),
    }));
    let long = (0..10).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
    app.apply(Out::Event(AgentEvent::ToolEnd {
        id: "1".into(),
        name: "read".into(),
        ok: true,
        duration_ms: 1,
        output: long,
    }));

    app.toggle_expanded();
    assert!(app.expanded_view.is_some());
    app.toggle_expanded();
    assert!(app.expanded_view.is_none()); // same hotkey closes it

    app.toggle_expanded();
    app.apply(Out::Started);
    assert!(app.expanded_view.is_none()); // a new turn dismisses the overlay
}

fn busy_app() -> App {
    let mut app =
        App::new("m".into(), "s".into(), super::super::commands::builtin(), std::env::temp_dir(), false);
    app.apply(Out::Started);
    app
}

/// A plan that deletes what it finished loses the shape of the work: the
/// reader is left with a remainder and no memory of what it is the remainder
/// of. Finished steps stay, struck through, and only give way when a long plan
/// runs out of footer.
#[test]
fn a_finished_step_stays_in_the_plan_struck_through() {
    use crate::text::Modifier;
    use elmee_core::plan::Status;
    let mut app = App::new("m".into(), "s".into(), Vec::new(), std::env::temp_dir(), false);
    app.plan = vec![step("read the parser", Status::Done), step("rewrite it", Status::Doing)];

    let lines = super::super::status::plan_lines(&app, 90);
    let struck: Vec<&str> = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .filter(|span| span.style.add_modifier.contains(Modifier::CROSSED_OUT))
        .map(|span| span.content.as_ref())
        .collect();
    assert_eq!(struck.len(), 1, "exactly the finished step is struck through");
    assert!(struck[0].contains("read the parser"), "{struck:?}");
    let all: String =
        lines.iter().flat_map(|line| line.spans.iter().map(|span| span.content.to_string())).collect();
    assert!(all.contains("plan · 1/2") && all.contains("rewrite it"), "{all}");

    // A plan longer than the footer gives up its oldest finished steps first,
    // never a step that is still open.
    app.plan = (0..9)
        .map(|index| step(&format!("step {index}"), if index < 6 { Status::Done } else { Status::Pending }))
        .collect();
    let shown: String = super::super::status::plan_lines(&app, 90)
        .iter()
        .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
        .collect();
    assert!(!shown.contains("step 0") && !shown.contains("step 2"), "the earliest done ones went: {shown}");
    assert!(shown.contains("step 5") && shown.contains("step 8"), "everything open stayed: {shown}");
    assert!(shown.contains("plan · 6/9"), "and the count still says what left: {shown}");
}

/// Built by rendering real frames and reading them: the narrow cases are
/// where an interface stops being one, and every defect this catches was
/// found that way — a plan chopped mid-word, a model name chopped
/// mid-word, and a task's age printed as ten digits of raw seconds.
#[test]
fn nothing_is_cut_without_saying_so_however_narrow_the_window() {
    use elmee_host::{Participant, TaskView};
    let mut app = App::new(
        "codex/gpt-5.4-mini".into(),
        "~/.elmee/sessions/x.jsonl".into(),
        super::super::commands::builtin(),
        std::env::temp_dir(),
        true,
    );
    app.identity = "agent-1".into();
    app.busy = true;
    app.plan = vec![
        step("read the parser", elmee_core::plan::Status::Done),
        step("rewrite the tokenizer to handle nested fences", elmee_core::plan::Status::Doing),
    ];
    app.agents = vec![
        Participant {
            name: "architecture-audit".into(),
            status: "running".into(),
            model: "codex/gpt-5.4-mini".into(),
            task: Some(TaskView {
                id: "t".into(),
                name: "architecture-audit".into(),
                status: "running".into(),
                detail: "reading a file with a very long name indeed".into(),
                started_at: 0,
                finished_at: Some(5_400),
                model: String::new(),
                input_tokens: 0,
                output_tokens: 0,
                session: None,
            }),
        },
        Participant {
            name: "reviewer".into(),
            status: "idle".into(),
            model: "anthropic/claude-sonnet-4-6".into(),
            task: None,
        },
    ];
    app.cells.push(Cell::User("why is the tokenizer dropping fences?".into()));

    for width in [100, 60, 34, 20] {
        let (rows, cursor) = super::super::ui::live_view(&app, width, 18);
        for row in &rows {
            assert!(row.width() <= width, "row overflows at width {width}: {row:?}");
        }
        assert!((cursor.0 as usize) < rows.len(), "the cursor must land on a row at width {width}");
        let text: Vec<String> =
            rows.iter().map(|row| row.spans.iter().map(|span| span.content.to_string()).collect()).collect();
        // Whatever was cut says so, at every width.
        for row in text.iter().filter(|row| row.contains('▫') || row.contains('⠋')) {
            let whole = row.ends_with("nested fences") || row.ends_with("1h30m") || row.contains("ctx");
            assert!(whole || row.ends_with('…'), "cut without saying so at {width}: {row:?}");
        }
        // Hours read as hours, never as four digits of seconds.
        assert!(
            !text.iter().any(|row| row.contains("5400")),
            "an age must be readable at width {width}: {text:?}"
        );
    }
}

#[test]
fn a_second_escape_on_an_empty_prompt_takes_back_the_last_message() {
    let mut app =
        App::new("m".into(), "s".into(), super::super::commands::builtin(), std::env::temp_dir(), false);
    // With something typed, Esc clears the line and arms nothing.
    app.editor.replace("half a thought");
    assert!(!app.escape_armed);

    // Reasoning folds once it stops arriving, so the answer is not pushed
    // off the screen by the working that produced it.
    app.apply(Out::Event(AgentEvent::Reasoning { text: "weighing two options".into() }));
    assert!(matches!(app.cells.last(), Some(Cell::Reasoning { done: false, .. })));
    app.apply(Out::Event(AgentEvent::Text { text: "the answer".into() }));
    // The answer beginning is what ends the thinking: a reasoning cell left
    // open would redraw its tail in the live area for the rest of the
    // session and never become scrollback.
    let reasoning = app.cells.iter().find(|cell| matches!(cell, Cell::Reasoning { .. }));
    assert!(matches!(reasoning, Some(Cell::Reasoning { done: true, .. })), "{:?}", app.cells.len());
    app.apply(Out::Done { error: None, aborted: false });
    // And it is the model working, not what it produced, so it stays out
    // of an exported transcript.
    assert!(!app.transcript_markdown().contains("weighing two options"));
}

/// The property that keeps a long session usable: a frame costs what is
/// *visible*, not what has happened. Asserted as a ratio rather than a
/// deadline, because a deadline measures the machine — this one failed in
/// CI at a threshold that passed here, which is how a timing test teaches
/// people to ignore the build.
/// A long answer must be readable from the top while it is still being
/// written. It used to sit whole in the live area until the model stopped,
/// which redrew it as a scrolling tail — so the only way to read it was to
/// wait for it to end.
#[test]
fn a_reply_becomes_history_line_by_line_while_it_is_still_arriving() {
    let mut app = App::new("m".into(), "s".into(), Vec::new(), std::env::temp_dir(), false);
    app.apply(Out::Event(AgentEvent::Text { text: "first line\nsecond line\nhalf a th".into() }));

    let reply = app.cells.iter().find(|c| matches!(c, Cell::Assistant { .. })).expect("a reply");
    assert!(matches!(reply, Cell::Assistant { flushed: 0, .. }), "nothing printed yet");
    let (ready, reached) = super::super::ui::flushable("first line\nsecond line\nhalf a th", 0, false);
    assert_eq!(ready.len(), 2, "both finished lines are ready, the unfinished one is not");
    assert_eq!(reached, "first line\nsecond line\n".len());

    // What stays live is only the line still being written.
    let live = super::super::cells::streaming_tail("first line\nsecond line\nhalf a th", reached, 8);
    let text: String = live.iter().flat_map(|l| l.spans.iter().map(|s| s.content.to_string())).collect();
    assert_eq!(text.trim(), "half a th");

    // A fence opened before the seam keeps its language after it.
    let fenced = "```rust\nfn main() {\nlet x = 1";
    let (opened, seam) = super::super::ui::flushable(fenced, 0, false);
    assert_eq!(opened.len(), 1, "the line under the fence is finished, and the fence is not drawn");
    let after = super::super::cells::streaming_tail(fenced, seam, 8);
    let tail: String = after.iter().flat_map(|l| l.spans.iter().map(|s| s.content.to_string())).collect();
    assert_eq!(tail.trim(), "let x = 1", "and the unfinished line is still live past the seam");

    // A table is laid out across its rows, so none of them is printed while
    // more may follow.
    let table = "before\n| a | b |\n|---|---|\n| c | d |\n";
    let (ready, reached) = super::super::ui::flushable(table, 0, false);
    assert_eq!((ready.len(), reached), (1, "before\n".len()), "only the line above the table is history");
    let (ready, _) = super::super::ui::flushable(&format!("{table}after\n"), reached, false);
    let drawn: Vec<String> = ready.iter().map(|line| line.text()).collect();
    assert!(drawn[0].contains("a  b") && !drawn[0].contains('|'), "the table arrives whole: {drawn:?}");
}

#[test]
fn a_frame_costs_the_visible_tail_and_not_the_session() {
    let conversation = |exchanges: usize| {
        let mut app = App::new("m".into(), "s".into(), Vec::new(), std::env::temp_dir(), false);
        for i in 0..exchanges {
            app.cells.push(Cell::User(format!("question {i}")));
            app.cells.push(Cell::Assistant { text: "answer ".repeat(200), done: true, flushed: 0 });
            app.cells.push(Cell::Tool {
                name: "read".into(),
                arguments: "{}".into(),
                output: Some("line\n".repeat(400)),
                ok: true,
            });
        }
        app.cells.push(Cell::Reasoning { text: "weighing options\n".repeat(500), done: false });
        app.cells.push(Cell::Assistant { text: "x".repeat(180_000), done: false, flushed: 0 });
        app
    };
    let cost = |app: &App| {
        let start = std::time::Instant::now();
        for _ in 0..50 {
            let _ = super::super::ui::live_view(app, 120, 30);
        }
        start.elapsed() / 50
    };
    let (short, long) = (conversation(1), conversation(200));
    // Warm both, so neither pays for a first allocation the other does not.
    let _ = (cost(&short), cost(&long));
    let (short_frame, long_frame) = (cost(&short), cost(&long));
    println!("frame: {short_frame:?} at 5 cells, {long_frame:?} at {}", long.cells.len());
    assert!(
        long_frame < short_frame * 4,
        "cost grew with the session: {short_frame:?} → {long_frame:?} over {} cells",
        long.cells.len()
    );
}

/// The footer answers what is happening. A finished task says so once, in the
/// transcript, and leaves — otherwise the footer fills up over a session and
/// the thing being waited on sinks below the things that are not.
#[test]
fn a_finished_task_says_so_in_the_transcript_and_leaves_the_footer() {
    use elmee_host::{Participant, TaskView};
    let task = |name: &str, status: &str, detail: &str, finished: Option<u64>| Participant {
        name: name.into(),
        status: status.into(),
        model: String::new(),
        task: Some(TaskView {
            id: name.into(),
            name: name.into(),
            status: status.into(),
            detail: detail.into(),
            started_at: 0,
            finished_at: finished,
            model: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            session: None,
        }),
    };
    let mut app = App::new("m".into(), "s".into(), Vec::new(), std::env::temp_dir(), false);
    let running = vec![task("news1", "running", "read", None), task("news2", "running", "read", None)];
    app.apply(Out::Runtime { name: "me".into(), agents: running.clone() });
    let before = app.cells.len();

    // One finishes: a line appears, and the footer keeps only the other.
    let mut next = running.clone();
    next[0] = task("news1", "completed", "146740 in · 4134 out · 1m44s", Some(104));
    app.apply(Out::Runtime { name: "me".into(), agents: next.clone() });
    assert_eq!(app.cells.len(), before + 1, "exactly one line, for the one that ended");
    let said = match app.cells.last() {
        Some(Cell::Info(text)) => text.clone(),
        other => panic!("a completion is not an error: {other:?}", other = other.is_some()),
    };
    assert!(said.contains("✓ news1") && said.contains("1m44s"), "{said}");
    assert_eq!(said.matches("1m44s").count(), 1, "the duration is printed once");

    // The same snapshot arriving again says nothing further.
    app.apply(Out::Runtime { name: "me".into(), agents: next.clone() });
    assert_eq!(app.cells.len(), before + 1, "a snapshot is not an event");

    let footer: String = super::super::status::footer(&app, 90)
        .iter()
        .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
        .collect();
    assert!(footer.contains("news2"), "what is still going stays: {footer}");
    assert!(!footer.contains("news1"), "what ended has left: {footer}");

    // A failure is said differently, and also leaves.
    next[1] = task("news2", "failed", "watchdog", Some(200));
    app.apply(Out::Runtime { name: "me".into(), agents: next });
    assert!(matches!(app.cells.last(), Some(Cell::Error(text)) if text.contains("✘ news2")));
}

#[test]
fn typing_during_a_turn_steers_it_and_slash_next_waits_for_the_one_after() {
    let route = super::super::commands::route;
    let mut app = busy_app();

    // Enter reaches the agent mid-turn: talking to one that is working is
    // the same act as talking to one that is not.
    let steered = route(&mut app, "actually use the other parser");
    assert!(matches!(&steered, Action::Steer(text) if text == "actually use the other parser"));
    assert!(app.queued.is_empty(), "steering must not also queue");

    // And the other intent stays reachable without a second key.
    assert!(matches!(route(&mut app, "/next then update the README"), Action::None));
    let released = app.apply(Out::Done { error: None, aborted: false });
    assert_eq!(released.as_deref(), Some("then update the README"));
}

#[test]
fn a_turn_that_ends_badly_hands_back_what_was_waiting_behind_it() {
    let mut app = busy_app();
    super::super::commands::route(&mut app, "/next run the tests");
    app.editor.replace("half a thought");

    app.apply(Out::Done { error: None, aborted: true });
    assert!(app.queued.is_empty());
    // Not sent, because the turn it was queued behind never finished — but
    // the person typed it, so it is theirs to send, edit, or drop.
    assert_eq!(app.editor.text(), "run the tests\nhalf a thought");
}

/// A report answers a question the user put to the harness, not to the agent,
/// so it is not spliced into the agent's output where it would stay for the rest
/// of the session. It is drawn in the live area, which repaints in place: a
/// report that is scrolled, opened and closed leaves the terminal's own log
/// exactly as it found it, which the alternate screen did not.
#[test]
fn a_report_is_drawn_in_place_and_stays_out_of_the_transcript() {
    let mut app =
        App::new("m".into(), "s".into(), super::super::commands::builtin(), std::env::temp_dir(), false);
    let transcript = app.cells.len();
    super::super::take_finalized(&mut app, false, 60); // the banner, already printed

    let body = (1..=60).map(|line| format!("line {line}")).collect::<Vec<_>>().join("\n");
    app.apply(Out::Panel { title: "commands".into(), body });
    assert_eq!(app.cells.len(), transcript, "the transcript is left alone");

    // Nothing new to finalize means nothing printed: a report never becomes
    // terminal history, however long it is or however far it is scrolled.
    assert!(super::super::take_finalized(&mut app, false, 60).is_empty());

    let (rows, _) = super::super::ui::live_view(&app, 60, 20);
    let shown: String =
        rows.iter().flat_map(|row| row.spans.iter()).map(|span| span.content.as_ref()).collect();
    assert!(shown.contains("line 1"), "the report is on screen: {shown:?}");
    assert!(shown.contains("Esc close"), "and says how to leave: {shown:?}");

    // The arrows belong to the report while it is open; there is nowhere else
    // on screen for them to go.
    let key = |code| crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
    let mode = super::super::keymap::mode(&app);
    use super::super::keymap::{Intent, resolve};
    assert_eq!(resolve(mode, key(crossterm::event::KeyCode::Down)), Intent::PanelDown);
    assert_eq!(resolve(mode, key(crossterm::event::KeyCode::Esc)), Intent::PanelClose);
}
/// Every window the harness can actually be given, with the biggest report it
/// has open. A section handed room must fit in it: the panel did not, and on a
/// six-row terminal it pushed the input box off the screen. Terminal rows are
/// clamped to six and the live view gets one less, so five is the smallest
/// window that exists. Below that nothing can fit at all: the input box owns
/// three rows and the status line a fourth, and those are not negotiable.
#[test]
fn the_interface_fits_the_smallest_window_it_can_be_given() {
    let mut app =
        App::new("m".into(), "s".into(), super::super::commands::builtin(), std::env::temp_dir(), false);
    let body = (1..=60).map(|line| format!("line {line}")).collect::<Vec<_>>().join("\n");
    app.apply(Out::Panel { title: "commands".into(), body });
    for rows in 5..=12 {
        let (drawn, cursor) = super::super::ui::live_view(&app, 40, rows);
        assert!(drawn.len() <= rows, "rows={rows}: drew {} lines into {rows}", drawn.len());
        assert!((cursor.0 as usize) < rows.max(1), "rows={rows}: the cursor left the window");
    }
}

/// Everything the model says reaches the terminal's own history, whatever
/// order it says it in.
///
/// The failure: only the newest cell was ever marked finished, so reasoning
/// arriving between two answers left the first answer open forever. The flush
/// pointer stops at an open cell, so that answer, the reasoning, the second
/// answer and every cell for the rest of the session stayed out of the
/// scrollback and lived only in the live area — which is bounded by the screen,
/// so the older of them simply scrolled away. That is what "the output goes
/// fragmentary and things get lost" was, and it got worse the longer a session
/// ran.
#[test]
fn nothing_the_model_says_is_left_out_of_the_scrollback() {
    let mut app =
        App::new("m".into(), "s".into(), super::super::commands::builtin(), std::env::temp_dir(), false);
    super::super::take_finalized(&mut app, false, 80); // the banner, already printed
    app.apply(Out::Started);
    app.apply(Out::Event(AgentEvent::Text { text: "first answer\n".into() }));
    app.apply(Out::Event(AgentEvent::Reasoning { text: "weighing two options\n".into() }));
    app.apply(Out::Event(AgentEvent::Text { text: "second answer\n".into() }));
    app.apply(Out::Event(AgentEvent::ToolStart {
        id: "1".into(),
        name: "exec".into(),
        arguments: "{}".into(),
    }));
    app.apply(Out::Event(AgentEvent::ToolEnd {
        id: "1".into(),
        name: "exec".into(),
        ok: true,
        duration_ms: 1,
        output: "ok".into(),
    }));
    app.apply(Out::Event(AgentEvent::Text { text: "third answer\n".into() }));
    app.apply(Out::Done { error: None, aborted: false });

    let printed: String = super::super::take_finalized(&mut app, false, 80)
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect();
    for said in ["first answer", "second answer", "third answer"] {
        assert!(printed.contains(said), "{said} never reached the scrollback: {printed:?}");
    }
    // Reasoning is what a run of tool calls does not say, and a count of its
    // words is the one summary that carries none of it.
    assert!(printed.contains("weighing two options"), "the thinking was folded away: {printed:?}");
    assert_eq!(app.flushed, app.cells.len(), "and the flush pointer is not stuck behind anything");
}

/// The whole promise of the notation: it runs, it prints, and the model is
/// never told. A `/` command may hand the agent something; this one cannot,
/// which is why it is worth a separate character.
#[test]
fn a_bang_command_runs_here_and_tells_the_model_nothing() {
    let mut app =
        App::new("m".into(), "s".into(), super::super::commands::builtin(), std::env::temp_dir(), false);
    let before = app.cells.len();

    let action = super::super::commands::route(&mut app, "! echo hello");
    assert!(matches!(action, Action::Shell(command) if command == "echo hello"));
    // Nothing that reaches the model: no prompt, no steer, and no user cell,
    // which is what a transcript is made of.
    assert!(!app.cells[before..].iter().any(|cell| matches!(cell, Cell::User(_))));
    assert!(matches!(app.cells.last(), Some(Cell::Shell { output: None, .. })));
    assert!(!app.transcript_markdown().contains("echo hello"), "nor an exported transcript");

    // The working directory is the project boundary every tool is bound to, so
    // moving it is refused rather than appearing to work.
    super::super::commands::route(&mut app, "! cd /tmp");
    assert!(matches!(app.cells.last(), Some(Cell::Error(message)) if message.contains("fixed")));
}

/// Completion reads the cursor's own line, so it works in a prompt that has
/// more than one — it used to switch itself off entirely the moment a prompt
/// grew a second line, which is exactly when a person is most likely to be
/// naming files.
#[test]
fn completion_works_on_the_line_the_cursor_is_on() {
    let mut app =
        App::new("m".into(), "s".into(), super::super::commands::builtin(), std::env::temp_dir(), false);
    app.files = vec!["src/main.rs".into(), "src/lib.rs".into()];
    app.editor.insert_str("first line\nlook at @mai");
    let offered: Vec<String> = app.menu_items().into_iter().map(|item| item.value).collect();
    assert_eq!(offered, vec!["@src/main.rs".to_owned()], "a mention on the second line still completes");
    app.complete_menu();
    assert_eq!(app.editor.text(), "first line\nlook at @src/main.rs ");

    // A command on a later line is a command too.
    app.editor.replace("notes\n/mod");
    let offered: Vec<String> = app.menu_items().into_iter().map(|item| item.value).collect();
    assert!(offered.iter().any(|value| value.starts_with("/model")), "{offered:?}");
}

/// The typed part is bold, so a row says why it is in the list.
#[test]
fn a_row_says_which_part_of_it_was_typed() {
    let mut app =
        App::new("m".into(), "s".into(), super::super::commands::builtin(), std::env::temp_dir(), false);
    app.editor.replace("/comp");
    let compact = app.menu_items().into_iter().find(|item| item.value.starts_with("/compact")).unwrap();
    assert_eq!(&compact.value[..compact.matched], "/comp");
}

/// Enter runs the line, and the menu does not get a say in it.
///
/// It used to: while anything matched, Enter meant "take the highlighted
/// completion", so a line ending in a complete word could not be sent at all —
/// Enter put another copy in, the new word reopened the menu, and the next
/// Enter put in another. Completing is Tab's, and a word already typed out in
/// full is not offered at all, so the menu is not sitting over a finished line
/// holding the arrow keys.
#[test]
fn enter_sends_the_line_even_with_the_menu_open() {
    use super::super::keymap::{Intent, Mode, mode, resolve};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app =
        App::new("m".into(), "s".into(), super::super::commands::builtin(), std::env::temp_dir(), false);
    app.editor.replace("/compact");
    let offered: Vec<String> = app.menu_items().into_iter().map(|item| item.value).collect();
    assert!(offered.is_empty(), "nothing is left to complete: {offered:?}");
    assert!(matches!(mode(&app), Mode::Editing { menu_open: false, .. }));

    // And where there *is* something to complete, the two keys are still apart.
    app.editor.replace("/comp");
    assert!(!app.menu_items().is_empty(), "half a word is worth a menu");
    let press = |code| resolve(mode(&app), KeyEvent::new(code, KeyModifiers::NONE));
    assert_eq!(press(KeyCode::Enter), Intent::Submit);
    assert_eq!(press(KeyCode::Tab), Intent::MenuComplete);
}

/// A gateway serves hundreds of models: typing narrows the list, the choice is
/// made among what is left, and a filter that leaves nothing chooses nothing.
#[test]
fn a_picker_narrows_as_you_type_and_chooses_among_what_is_left() {
    let mut app = App::new("m".into(), "s".into(), Vec::new(), std::env::temp_dir(), false);
    let models =
        ["openai/gpt", "local/qwen-27b", "local/qwen-7b"].map(|model| (model.to_owned(), model.to_owned()));
    app.picker = Some(Picker::new("model", models.to_vec(), PickerKind::Model, Some("local/qwen-7b".into())));
    assert_eq!(app.picker_selected().unwrap().1, "local/qwen-7b", "it opens on the one in force");

    for character in "QWEN".chars() {
        app.picker.as_mut().unwrap().narrow(Some(character));
    }
    assert_eq!(app.picker.as_ref().unwrap().shown().len(), 2, "case does not matter");
    app.picker_move(1);
    assert_eq!(app.picker_selected().unwrap().1, "local/qwen-7b");

    app.picker.as_mut().unwrap().narrow(Some('!'));
    assert!(app.picker_selected().is_none());
    app.picker_move(1);
    app.picker.as_mut().unwrap().narrow(None);
    assert_eq!(app.picker_selected().unwrap().1, "local/qwen-27b", "erasing brings the list back");
}

/// Enter on the start of a command's name means the command the menu has
/// highlighted — `/us` used to answer "unknown command: /us" with `/usage`
/// highlighted right above it, and so did choosing a row with the arrows.
/// Anything typed past the name is the person's own line and is sent as it is.
#[test]
fn enter_on_half_a_command_name_means_the_highlighted_command() {
    let mut app =
        App::new("m".into(), "s".into(), super::super::commands::builtin(), std::env::temp_dir(), false);
    let chosen = |app: &mut App, typed: &str, down: isize| {
        app.editor.replace(typed);
        app.menu_index = 0;
        for _ in 0..down {
            app.menu_move(1);
        }
        app.highlighted_command().map(|command| command.name)
    };
    assert_eq!(chosen(&mut app, "/us", 0).as_deref(), Some("usage"));
    assert_eq!(chosen(&mut app, "/c", 1).as_deref(), Some("compact"), "the arrows choose");
    assert_eq!(chosen(&mut app, "/", 1).as_deref(), Some("init"));
    assert_eq!(chosen(&mut app, "/compact", 0), None, "typed in full is sent as typed");
    assert_eq!(chosen(&mut app, "/model fake/m1", 0), None, "an argument is the person's own line");
    assert_eq!(chosen(&mut app, "/zz", 0), None, "nothing highlighted, nothing chosen");
    assert_eq!(chosen(&mut app, "plain words", 0), None);

    // A command that wants an argument is completed and waits for it.
    app.editor.replace("/ne");
    assert!(app.highlighted_command().is_some_and(|command| command.hint.is_some()));
}

/// A conversation that is opened, or moved within, is put on screen as it
/// stands: what was asked, what was answered, what each call returned.
#[test]
fn a_conversation_moved_to_is_shown_as_it_stands() {
    use elmee_core::message::{Message, ToolCall};
    let mut calling = Message::assistant_text("looking");
    calling.tool_calls = vec![ToolCall::new("1", "read", "{}"), ToolCall::new("2", "exec", "{}")];
    let messages = vec![
        Message::system("prompt"),
        Message::user("fix it"),
        Message::background("e", "<external_event>ping</external_event>"),
        calling,
        Message::tool("1", "fn main() {}"),
        Message::tool("2", "Error: no such command"),
        Message::assistant_text("done"),
    ];
    let cells = transcript(&messages);
    assert!(matches!(&cells[0], Cell::User(text) if text == "fix it"), "only what a person typed is theirs");
    assert!(matches!(&cells[2], Cell::Tool { name, output: Some(output), ok: true, .. }
        if name == "read" && output == "fn main() {}"));
    assert!(matches!(&cells[3], Cell::Tool { name, ok: false, .. } if name == "exec"));
    assert!(matches!(cells.last(), Some(Cell::Assistant { text, done: true, .. }) if text == "done"));
    assert!(cells.iter().all(Cell::finalized), "nothing replayed is left open");

    // A long conversation shows where it stands, not everything it ever was.
    let long: Vec<Message> = (0..40).map(|turn| Message::user(format!("prompt {turn}"))).collect();
    let cells = transcript(&long);
    assert!(matches!(&cells[0], Cell::Info(note) if note.contains("28 earlier messages")));
    assert_eq!(cells.len(), 1 + SHOWN_PROMPTS);

    let mut app = App::new("m".into(), "s".into(), Vec::new(), std::env::temp_dir(), true);
    app.plan = serde_json::from_str(r#"[{"step":"old"}]"#).unwrap();
    app.apply(Out::Moved { session: None, messages, draft: Some("fix it".into()), note: "went back".into() });
    assert!(app.clear_screen && app.plan.is_empty(), "the screen and the plan belonged to where it stood");
    assert_eq!(app.editor.text(), "fix it");
    assert!(matches!(app.after_clear.last(), Some(Cell::Info(note)) if note == "went back"));
}

/// The second question of a rewind is asked only where the files were kept.
#[test]
fn going_back_offers_the_files_only_where_they_were_recorded() {
    use elmee_core::session::Prompt;
    let mut app = App::new("m".into(), "s".into(), Vec::new(), std::env::temp_dir(), true);
    app.apply(Out::Prompts(vec![
        Prompt { id: 2, text: "first\nsecond line".into(), checkpoint: Some("abc".into()) },
        Prompt { id: 9, text: "latest".into(), checkpoint: None },
    ]));
    let picker = app.picker.as_ref().expect("a list of prompts");
    assert_eq!(picker.items, [("9".to_owned(), "latest".to_owned()), ("2".to_owned(), "first".to_owned())]);
    assert_eq!(app.recorded, [2]);
}
