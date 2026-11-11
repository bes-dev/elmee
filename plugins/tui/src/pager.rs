//! A terminal report, shown above the input and scrolled in place.
//!
//! Reports outgrew a fixed strip the moment one of them was the command list:
//! a fixed height can only cut, and a cut list is not a list. The alternate
//! screen is the usual answer and the wrong one here — this harness draws as a
//! flowing document in the main screen (see `render`), and a report that seizes
//! the terminal takes the session's own log with it on the way out.
//!
//! So the report stays in the live area, which repaints itself in place and
//! never writes a byte to the scrollback. It takes every row the layout can
//! spare and scrolls inside them, which is the only way to be both complete and
//! silent. Where the view starts is settled here rather than where the key was
//! pressed, because only drawing knows how tall the panel got — a key can ask
//! to move, and this decides what that meant.

use super::text::{Line, Span};
use super::text::{Modifier, Style};

use super::app::Panel;
use super::keymap::{Mode, hints};
use super::render::wrap_line;
use super::theme::t;

/// Rows the frame spends on itself: the title and the footer.
const CHROME: usize = 2;

pub(super) fn lines(panel: &Panel, width: usize, budget: usize) -> Vec<Line> {
    let body: Vec<Line> = panel
        .body
        .lines()
        // Wrapped inside the gutter, so a long line stays in the report
        // instead of its second half standing at the left edge like new text.
        .flat_map(|line| wrap_line(Line::raw(line.to_owned()), width.saturating_sub(4).max(1)))
        .map(|row| {
            let mut spans = vec![super::cells::gutter()];
            spans.extend(row.spans);
            Line::from(spans)
        })
        .collect();

    let page = budget.saturating_sub(CHROME).max(1);
    let top = panel.offset.get().min(body.len().saturating_sub(page));
    panel.offset.set(top);
    panel.page.set(page);

    let mut rows = vec![Line::from(vec![
        Span::styled("⌄ ".to_owned(), Style::default().fg(t().accent)),
        Span::styled(panel.title.clone(), Style::default().add_modifier(Modifier::BOLD)),
    ])];
    rows.extend(body.iter().skip(top).take(page).cloned());
    // The keys come from the keymap, like every other hint in the UI, so the
    // footer cannot promise one the table no longer resolves.
    // A report that fits has nowhere to scroll to, so it offers only the way
    // out.
    let footer = if body.len() > page {
        format!("lines {}–{} of {} · {}", top + 1, top + page, body.len(), hints(Mode::Panel))
    } else {
        "Esc close".to_owned()
    };
    rows.push(Line::from(Span::styled(
        format!("  {footer}"),
        Style::default().fg(t().muted).add_modifier(Modifier::ITALIC),
    )));
    // A section given room must fit in it. On a six-row terminal the budget is
    // smaller than the frame, and returning the frame anyway pushed the input
    // box off the bottom of the screen — the layout hands out room, it does not
    // enforce it. What goes first is what can be missed: the hint, then the
    // title, and the report itself last.
    rows.truncate(budget);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The failure this replaces: arrows kept moving after the last line, so the
    /// text walked off the top and the frame appeared to slide away. The view is
    /// clamped to the last full page, so the end is an end.
    #[test]
    fn scrolling_past_the_end_stops_on_the_last_page() {
        let body = (1..=60).map(|line| format!("line {line}")).collect::<Vec<_>>().join("\n");
        let panel = Panel::new("commands", body);

        panel.offset.set(9_000);
        let rows = lines(&panel, 40, 20);
        assert_eq!(panel.offset.get(), 60 - panel.page.get(), "clamped to the last full page");
        assert_eq!(rows.len(), panel.page.get() + CHROME, "and fills the room it was given");

        let shown = flatten(&rows);
        assert!(shown.contains("line 60"), "the last line is on screen: {shown:?}");
        assert!(shown.contains("of 60"), "and the position is stated: {shown:?}");
    }

    /// Short enough to fit says nothing about position, because a position
    /// implies there is somewhere else to go.
    #[test]
    fn a_report_that_fits_takes_only_the_rows_it_needs() {
        let panel = Panel::new("session", "one\ntwo");
        let rows = lines(&panel, 40, 20);
        assert_eq!(rows.len(), 2 + CHROME, "no padding out to the budget");
        assert!(!flatten(&rows).contains("lines 1"), "and no position to chase");
    }

    fn flatten(rows: &[Line]) -> String {
        rows.iter().flat_map(|row| row.spans.iter()).map(|span| span.content.as_ref()).collect()
    }
}
