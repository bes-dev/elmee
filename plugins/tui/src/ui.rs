//! Assembling the terminal live area: its sections and cursor.
//!
//! Everything here is a decision about arrangement. What each section contains
//! belongs to `bottom`, `status` and `cells`; how rows become ANSI belongs to
//! `render`; how the rows are divided belongs to `layout`.

use super::text::{Line, Span};
use super::text::{Modifier, Style};

use super::app::{App, Cell};
use super::keymap;
use super::layout::{Section, stack};
use super::render::{cursor_in, wrap_chars};
use super::theme::t;
use super::{bottom, cells, status};

pub(super) use cells::{flushable, gap, hanging, rendered_cell, tool_body};

/// The rows below the finalized transcript, and the cursor's place in them.
pub fn live_view(app: &App, width: usize, max_rows: usize) -> (Vec<Line>, (u16, u16)) {
    let width = width.max(8);
    if let Some(dialog) = &app.dialog {
        return bottom::dialog_view(dialog, width, max_rows);
    }
    let text_width = width - 2; // the "❯ " prompt

    // Reverse search replaces the input box contents with the live match.
    if let Some((query, matches, selected)) = app.editor.search_matches(5) {
        let interior = vec![Line::from(vec![
            Span::styled("⌕ ", Style::default().fg(t().accent).add_modifier(Modifier::BOLD)),
            Span::styled(query.to_owned(), Style::default().fg(t().warning)),
            Span::styled(
                if matches.is_empty() { "  no match".to_owned() } else { String::new() },
                Style::default().fg(t().muted),
            ),
        ])];
        let mut rows: Vec<Line> = matches
            .iter()
            .enumerate()
            .map(|(index, entry)| search_row(entry, query, index == selected, width))
            .collect();
        rows.extend(bottom::edged(interior, width, app.busy));
        rows.push(Line::from(Span::styled(
            format!(" {} ", keymap::hints(keymap::mode(app))),
            Style::default().fg(t().muted),
        )));
        let cursor = (rows.len().saturating_sub(2) as u16, 2);
        return (rows, cursor);
    }

    let (interior, hidden_rows) = input_interior(app, text_width);
    let box_height = interior.len() + 2; // one rule above, one below
    let stacked = stack(
        vec![
            Section::flexible(|room| preview(app, width, room)),
            // Air between the conversation and the place it is typed into.
            Section::fixed(vec![Line::default()]),
            Section::fixed(menu_lines(app)),
            Section::fixed(bottom::edged(interior, width, app.busy)),
            Section::fixed(status::footer(app, width)),
        ],
        max_rows,
    );
    let input_top = stacked.offsets[3];
    let cursor = cursor_at(app, text_width, input_top, hidden_rows);
    debug_assert!(box_height >= 3);
    (stacked.rows, cursor)
}

/// The input box's contents: the prompt on the first row, aligned
/// continuations after it, scrolled so the cursor's own row stays visible.
fn input_interior(app: &App, text_width: usize) -> (Vec<Line>, usize) {
    let mut interior: Vec<Line> = Vec::new();
    for (index, logical) in app.editor.text().split('\n').enumerate() {
        for (wrapped_index, wrapped) in
            wrap_chars(highlight(app, logical, index == 0), text_width).into_iter().enumerate()
        {
            let prefix = if index == 0 && wrapped_index == 0 {
                Span::styled("❯ ", Style::default().fg(t().accent).add_modifier(Modifier::BOLD))
            } else {
                Span::raw("  ")
            };
            let mut spans = vec![prefix];
            spans.extend(wrapped.spans);
            interior.push(Line::from(spans));
        }
    }
    let shown = interior.len().clamp(1, 6);
    let hidden_rows = interior.len() - shown;
    (interior.into_iter().skip(hidden_rows).collect(), hidden_rows)
}

/// What the line says, coloured by what it means rather than by what it looks
/// like.
///
/// A leading `/` that names a command that exists reads as a command; one that
/// does not is wrong now rather than after Enter, which is the whole reason to
/// colour it. `!` is its own thing because it is: the person's own terminal,
/// and nothing the model will see. A mention is only a mention once the path
/// behind it is real — the same rule Codex uses, which is to say something when
/// it has acquired a meaning and not merely a shape.
fn highlight(app: &App, line: &str, first: bool) -> Line {
    let plain = |text: &str| Span::raw(text.to_owned());
    if first && let Some(rest) = line.strip_prefix('!') {
        return Line::from(vec![
            Span::styled("!".to_owned(), Style::default().fg(t().warning).add_modifier(Modifier::BOLD)),
            Span::styled(rest.to_owned(), Style::default().fg(t().warning)),
        ]);
    }
    if first && line.starts_with('/') {
        let end = line.find(char::is_whitespace).unwrap_or(line.len());
        let (name, rest) = line.split_at(end);
        let known = app.commands.iter().any(|command| command.name == name[1..]);
        let style =
            Style::default().fg(if known { t().accent } else { t().error }).add_modifier(Modifier::BOLD);
        return Line::from(vec![Span::styled(name.to_owned(), style), plain(rest)]);
    }
    // Mentions anywhere: what the agent will actually be handed a path for.
    let mut spans = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find('@') {
        let end = rest[at..].find(char::is_whitespace).map_or(rest.len(), |offset| at + offset);
        let path = &rest[at + 1..end];
        spans.push(plain(&rest[..at]));
        let real = !path.is_empty() && app.known_file(path);
        spans.push(if real {
            Span::styled(rest[at..end].to_owned(), Style::default().fg(t().accent))
        } else {
            plain(&rest[at..end])
        });
        rest = &rest[end..];
    }
    spans.push(plain(rest));
    Line::from(spans)
}

/// Completion menu for the "/command" or "@file" being typed; the window
/// slides so the selection stays visible.
fn menu_lines(app: &App) -> Vec<Line> {
    let matches = app.menu_items();
    if matches.is_empty() {
        return Vec::new();
    }
    let selected = app.menu_index % matches.len();
    // Which key takes a row, said where the rows are. Enter used to take one,
    // and a person who had learnt that had learnt something that now runs the
    // line instead — so the menu has to say what it answers to.
    let hint = if app.highlighted_command().is_some() {
        "Enter runs the highlighted command · Tab completes · ↑↓ choose"
    } else if app.choosing_argument() {
        "Enter chooses · ↑↓ move"
    } else {
        keymap::hints(keymap::mode(app))
    };
    let mut rows = vec![Line::from(Span::styled(format!(" {hint}"), Style::default().fg(t().muted)))];
    rows.extend(matches.iter().enumerate().skip(selected.saturating_sub(4)).take(8).map(|(index, item)| {
        let (marker, style) = if index == selected {
            ("▸", Style::default().fg(t().accent).add_modifier(Modifier::BOLD))
        } else {
            (" ", Style::default().fg(t().muted))
        };
        // The typed part in bold, so a row says why it is in the list —
        // which is the whole question when eight of them share a prefix.
        let matched = item.matched.min(item.value.len());
        let padding = 13usize.saturating_sub(item.value.chars().count() + 1);
        Line::from(vec![
            Span::styled(marker.to_owned(), style),
            Span::styled(item.value[..matched].to_owned(), style.add_modifier(Modifier::BOLD)),
            Span::styled(format!("{}{:padding$}", &item.value[matched..], "", padding = padding), style),
            Span::styled(format!(" {}", item.description), Style::default().fg(t().muted)),
        ])
    }));
    rows
}

/// What sits above the box: the model picker, a report, the Ctrl+O overlay,
/// or the tail
/// of the exchange in progress. Built back to front so a long streaming reply
/// renders only its visible end — the whole text is rendered once, when the
/// cell is finalized.
fn preview(app: &App, width: usize, budget: usize) -> Vec<Line> {
    if budget == 0 {
        return Vec::new();
    }
    if let Some(picker) = &app.picker {
        return cells::picker_lines(picker, budget);
    }
    if let Some(panel) = &app.panel {
        return super::pager::lines(panel, width, budget);
    }
    let overlay = app.expanded_view.and_then(|index| match app.cells.get(index) {
        Some(Cell::Tool { name, arguments, output: Some(output), ok }) => {
            Some((name.clone(), cells::tool_body(name, arguments, output, *ok)))
        }
        _ => None,
    });
    if let Some((name, (body, language))) = overlay {
        let lines = body.lines().count();
        let hint = format!("{lines} {} · Ctrl+O to close", if lines == 1 { "line" } else { "lines" });
        return cells::overlay_lines(&name, &body, language, &hint, width, budget);
    }
    let mut collected: Vec<Line> = Vec::new();
    let first = app.flushed.min(app.cells.len());
    'cells: for (index, cell) in app.cells.iter().enumerate().skip(first).rev() {
        let (lines, continued) = match cell {
            Cell::Assistant { text, flushed, .. } => {
                (cells::streaming_tail(text, *flushed, budget + 4), *flushed > 0)
            }
            _ => (cells::cell_lines(cell, app.spinner, &app.cwd, width), false),
        };
        let mut rows_for_cell: Vec<Line> = Vec::new();
        // A reply already partly printed above carries on without a break.
        if !continued && !lines.is_empty() && cells::gap(index.checked_sub(1).map(|at| &app.cells[at]), cell)
        {
            rows_for_cell.push(Line::default());
        }
        for line in lines {
            rows_for_cell.extend(cells::hanging(line, width));
        }
        for row in rows_for_cell.into_iter().rev() {
            collected.push(row);
            if collected.len() >= budget {
                break 'cells;
            }
        }
    }
    collected.into_iter().rev().collect()
}

/// Where the hardware cursor goes: the row the caret is on inside the box,
/// and the column the text starts at.
fn cursor_at(app: &App, text_width: usize, input_top: usize, hidden_rows: usize) -> (u16, u16) {
    let (cursor_row, characters) = app.editor.cursor_position();
    let mut visual_row = 0;
    for (index, logical) in app.editor.text().split('\n').enumerate() {
        if index == cursor_row {
            let (row, column) = cursor_in(logical, characters, text_width);
            visual_row += row;
            // "❯ " puts the text at column 2; the cursor lands exactly on it.
            return ((input_top + 1 + visual_row.saturating_sub(hidden_rows)) as u16, (2 + column) as u16);
        }
        visual_row += cursor_in(logical, logical.chars().count(), text_width).0 + 1;
    }
    ((input_top + 1) as u16, 2)
}

/// One candidate, with the part that matched pointed at.
fn search_row(entry: &str, query: &str, selected: bool, width: usize) -> Line {
    let (marker, rest) = if selected {
        ("▸ ", Style::default().fg(t().accent))
    } else {
        ("  ", Style::default().fg(t().muted))
    };
    let flat = entry.replace('\n', " ⏎ ");
    let at = flat.find(query).filter(|_| !query.is_empty());
    let mut spans = vec![Span::styled(marker.to_owned(), rest)];
    match at {
        Some(at) => {
            spans.push(Span::styled(flat[..at].to_owned(), rest));
            spans.push(Span::styled(
                flat[at..at + query.len()].to_owned(),
                Style::default().fg(t().warning).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(flat[at + query.len()..].to_owned(), rest));
        }
        None => spans.push(Span::styled(flat, rest)),
    }
    let mut line = Line::from(spans);
    while line.width() > width {
        line.spans.last_mut().map(|span| span.content.to_mut().pop());
        if line.spans.last().is_some_and(|span| span.content.is_empty()) {
            line.spans.pop();
        }
        if line.spans.is_empty() {
            break;
        }
    }
    line
}

#[cfg(test)]
mod dialog_tests {

    use super::*;

    #[test]
    fn a_successful_patch_shows_itself_instead_of_the_status_line() {
        let arguments =
            serde_json::json!({"patch": "*** Begin Patch\n*** Update File: a.rs\n-let x = 1;\n+let x = 2;\n*** End Patch"})
                .to_string();
        let (body, language) =
            tool_body("apply_patch", &arguments, "Applied patch to 1 file(s):\nM a.rs", true);
        assert!(body.contains("-let x = 1;\n+let x = 2;"), "{body:?}");
        assert!(language.is_empty());
        // A failed patch keeps the error, which is what the user must read.
        let (body, _) = tool_body("apply_patch", &arguments, "Error: no match", false);
        assert_eq!(body, "Error: no match");
    }

    #[test]
    fn background_panel_stays_within_terminal_width() {
        let mut app = App::new("m".into(), "s".into(), Vec::new(), std::env::temp_dir(), false);
        app.agents.push(elmee_host::Participant {
            name: "very-long-background-agent-name".into(),
            status: "running".into(),
            model: String::new(),
            task: Some(elmee_host::TaskView {
                id: "1".into(),
                name: "very-long-background-agent-name".into(),
                status: "running".into(),
                detail: "inspecting a very long and detailed operation in the repository".into(),
                started_at: 0,
                finished_at: None,
                model: String::new(),
                input_tokens: 0,
                output_tokens: 0,
                session: None,
            }),
        });
        let (rows, _) = live_view(&app, 28, 20);
        assert!(rows.iter().all(|row| row.width() <= 28));

        // A name cut to exactly the column width used to leave no gap, and the
        // detail began against it: `architecture-audparent process ended`.
        let text = |line: &Line| line.spans.iter().map(|span| span.content.to_string()).collect::<String>();
        let task = rows.iter().map(text).find(|row| row.contains('…')).expect("the task row");
        assert!(task.contains("… · "), "a cut name must not run into the detail: {task:?}");
    }
}
