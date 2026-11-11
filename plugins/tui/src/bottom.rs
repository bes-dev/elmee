//! Everything the terminal user types into, and the panels that replace it.
//!
//! The input box, the command menu, reverse search, the question dialog and
//! the model picker all occupy the same place on screen and are mutually
//! exclusive, so they belong together: which one is showing is one decision,
//! made here.

use super::text::{Line, Span};
use super::text::{Modifier, Style};
use elmee_core::InteractionRequest;

use super::render::wrap_line;
use super::theme::t;

pub(super) fn dialog_view(
    dialog: &super::dialog::Dialog,
    width: usize,
    max_rows: usize,
) -> (Vec<Line>, (u16, u16)) {
    let accent = Style::default().fg(t().accent).add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(t().muted);
    let mut lines = match &dialog.request {
        InteractionRequest::Questions { questions, .. } => {
            let question = &questions[dialog.question];
            vec![
                Line::from(vec![
                    Span::styled(question.header.clone(), accent),
                    Span::styled(format!("  {}/{}", dialog.question + 1, questions.len()), muted),
                ]),
                Line::raw(question.question.clone()),
            ]
        }
    };
    if let Some(origin) = &dialog.origin {
        lines.insert(
            0,
            Line::from(Span::styled(
                format!("@{origin} requests input"),
                Style::default().fg(t().keyword).add_modifier(Modifier::BOLD),
            )),
        );
    }
    lines.push(Line::default());
    let option_start = lines.len();
    let count = dialog.option_count();
    for index in 0..count {
        let (label, description) = match &dialog.request {
            InteractionRequest::Questions { questions, .. } => {
                questions[dialog.question].options.get(index).map_or_else(
                    || ("Other…".into(), "Type a different answer".into()),
                    |option| (option.label.clone(), option.description.clone()),
                )
            }
        };
        let active = index == dialog.cursor;
        let checked = dialog.marked.get(index).copied().unwrap_or(false);
        lines.push(Line::from(vec![
            Span::styled(if active { "▸ " } else { "  " }, if active { accent } else { muted }),
            Span::styled(if checked { "[x] " } else { "" }, Style::default().fg(t().success)),
            Span::styled(label, if active { accent } else { Style::default() }),
            Span::styled(
                if description.is_empty() { String::new() } else { format!(" — {description}") },
                muted,
            ),
        ]));
    }
    lines.push(Line::default());
    let selected_row = 1 + lines[..option_start + dialog.cursor]
        .iter()
        .map(|line| wrap_line(line.clone(), width).len())
        .sum::<usize>();
    let input_column = if dialog.typing {
        lines.push(Line::from(vec![Span::styled("❯ ", accent), Span::raw(dialog.editor.text().to_owned())]));
        let (row, characters) = dialog.editor.cursor_position();
        let line = dialog.editor.text().split('\n').nth(row).unwrap_or_default();
        let before: String = line.chars().take(characters).collect();
        Some(unicode_width::UnicodeWidthStr::width(before.as_str()))
    } else {
        lines.push(Line::from(Span::styled(
            if dialog.current_question().is_some_and(|question| question.multi_select) {
                "↑↓ move · Space toggle · Enter submit · Esc cancel"
            } else {
                "↑↓ move · Enter select · Esc cancel"
            },
            muted,
        )));
        None
    };
    let lines: Vec<_> = lines.into_iter().flat_map(|line| wrap_line(line, width)).collect();
    let content_rows = lines.len();
    let mut rows = edged(lines, width, false);
    if rows.len() > max_rows {
        rows.truncate(max_rows);
    }
    let cursor = input_column.map_or((selected_row.min(rows.len().saturating_sub(1)) as u16, 0), |column| {
        let input_width = 2 + unicode_width::UnicodeWidthStr::width(dialog.editor.text());
        let input_rows = input_width.div_ceil(width).max(1);
        ((1 + content_rows - input_rows + (2 + column) / width) as u16, ((2 + column) % width) as u16)
    });
    (rows, cursor)
}
/// The input area without side walls: a rule above and below say where it
/// starts and ends, and each row is only what was typed — a terminal selection
/// over it copies text, not box art and padding to the terminal's width.
pub(super) fn edged(interior: Vec<Line>, width: usize, busy: bool) -> Vec<Line> {
    let style = Style::default().fg(if busy { t().accent } else { t().border });
    let rule = || Line::from(Span::styled("─".repeat(width), style));
    let mut rows = vec![rule()];
    rows.extend(interior);
    rows.push(rule());
    rows
}
