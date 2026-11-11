//! The terminal footer: who else is here, what this session started, and what the
//! agent is doing right now.
//!
//! These lines are what a person glances at rather than reads, so each answers
//! one question and gives up gracefully when the window is narrow.

use super::text::{Line, Span};
use super::text::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::app::App;
use super::render::{SPINNER, fit};
use super::theme::t;

/// Everything below the input box, in the order it is read.
pub(super) fn footer(app: &App, width: usize) -> Vec<Line> {
    let mut rows = plan_lines(app, width);
    rows.extend(agent_lines(app, width));
    rows.push(status_line(app, width));
    rows
}

/// The status line, narrowed to fit rather than cut blind.
///
/// Clamping alone would keep whatever happens to come first and lose the rest,
/// so the parts are dropped in order of how little they are worth once the
/// window is tight: the session path, then the model, then the identity.
pub(super) fn status_line(app: &App, width: usize) -> Line {
    let state = if app.busy {
        let elapsed = app.started_at.map_or(0, |at| at.elapsed().as_secs());
        // What it is waiting on, not only that it is waiting: a model that is
        // thinking and a test suite that is running are different waits.
        let doing = app.cells.iter().rev().find_map(|cell| match cell {
            super::app::Cell::Tool { name, output: None, .. } => Some(name.as_str()),
            _ => None,
        });
        // Stopping takes two presses, and the first one is answered here:
        // a key that changes nothing on screen reads as a key that was lost.
        let stop = if app.escape_armed { "Esc again to stop" } else { "Esc Esc stops" };
        Span::styled(
            format!(
                " {} {} {elapsed}s · {stop} ",
                SPINNER[app.spinner % SPINNER.len()],
                doing.unwrap_or("thinking")
            ),
            {
                let style = Style::default().fg(t().accent);
                if app.escape_armed { style.add_modifier(Modifier::BOLD) } else { style }
            },
        )
    } else if app.escape_armed {
        Span::styled(" Esc again to go back to an earlier prompt ", Style::default().fg(t().accent))
    } else if app.ctrl_c_armed {
        Span::styled(" Ctrl+C again to quit ", Style::default().fg(t().error))
    } else {
        Span::styled(" ● ready ", Style::default().fg(t().success))
    };
    let queued =
        if app.queued.is_empty() { String::new() } else { format!("· {} queued ", app.queued.len()) };
    // A count of zero says nothing and costs the width the model needs.
    let company =
        if app.agents.is_empty() { String::new() } else { format!("· {} agents ", app.agents.len()) };
    // A name matters once there is someone to be told apart from; an agent
    // alone in its project under the name everyone starts with has said nothing.
    let alone = app.agents.is_empty() && app.identity == "agent";
    let identity = if app.identity.is_empty() || alone {
        String::new()
    } else {
        format!("· @{} {company}", app.identity)
    };
    // A raw character count means nothing without the window it fills; with a
    // declared window, show how full the context actually is.
    let context = match app.compact_chars {
        // Rounded up: a context that has anything in it is not at zero.
        Some(limit) if limit > 0 => format!("{}% ctx", (app.context_chars * 100).div_ceil(limit)),
        _ => format!("{} ctx", kchars(app.context_chars)),
    };
    let fixed = state.width();
    // Widest first; each fallback gives up the next-least-useful part.
    // Which model answered is the fact a narrow window keeps longest after the
    // state chip: it is what a person has just changed and what makes an answer
    // readable, while the name this agent answers to changes once a session.
    let tails = [
        format!("{identity}· {context} · {} · {} {queued}", app.model, app.session),
        format!("{identity}· {context} · {} {queued}", app.model),
        format!("· {context} · {} {queued}", app.model),
        format!("· {context} "),
    ];
    // Past the narrowest fallback there is nothing left to give up, so what
    // remains is cut rather than allowed to run off the window: the state chip
    // is the one part always worth keeping, and it is fitted too.
    if fixed >= width {
        return Line::from(Span::styled(fit(&state.content, width), state.style));
    }
    let tail = tails
        .iter()
        .find(|tail| fixed + UnicodeWidthStr::width(tail.as_str()) <= width)
        .map_or_else(|| fit(tails.last().expect("a non-empty fallback list"), width - fixed), Clone::clone);
    Line::from(vec![state, Span::styled(tail, Style::default().fg(t().muted))])
}
/// The steps the agent said it is working through.
///
/// Shown nearest the input because it answers the question a person actually
/// has while waiting — not "is it busy", which the spinner already says, but
/// "is it doing the thing I wanted".
///
/// A finished step stays, struck through: a list that deletes what it did
/// loses the shape of the work, and the reader has to remember what the
/// remaining steps are the remainder *of*. The footer still cannot grow
/// without bound, so when a plan outgrows the budget the steps dropped are the
/// earliest finished ones — the only ones whose disappearance costs nothing
/// the count in the header does not already say.
pub(super) fn plan_lines(app: &App, width: usize) -> Vec<Line> {
    if app.plan.is_empty() {
        return Vec::new();
    }
    use elmee_core::plan::Status;
    const SHOWN: usize = 6;
    let done = app.plan.iter().filter(|step| !step.open()).count();
    let mut lines = vec![Line::from(Span::styled(
        format!("  plan · {done}/{}", app.plan.len()),
        Style::default().fg(t().muted),
    ))];
    let mut first = 0;
    while app.plan.len() - first > SHOWN && !app.plan[first].open() {
        first += 1;
    }
    lines.extend(app.plan[first..].iter().take(SHOWN).map(|step| {
        let (mark, style) = match step.status {
            Status::Done => ("✓", Style::default().fg(t().muted).add_modifier(Modifier::CROSSED_OUT)),
            Status::Doing => ("▸", Style::default().fg(t().accent)),
            Status::Pending => ("▫", Style::default().fg(t().muted)),
        };
        Line::from(Span::styled(fit(&format!(" {mark} {}", step.step), width), style))
    }));
    lines
}

/// Who is working in this project, mine and otherwise, in one list.
///
/// Grouped by whether the work is still going rather than by where the name
/// came from — the question while delegating is "who, and are they busy", and
/// a running child answers it identically whether this session started it or a
/// person did. It used to be two lists built from two sources, so a child
/// appeared in both.
fn agent_lines(app: &App, width: usize) -> Vec<Line> {
    // Only what is still going. A finished task has already said so in the
    // transcript, and leaving it here would mean the footer grows all session
    // and the thing you are waiting on sinks below the things you are not.
    let working: Vec<_> = app
        .agents
        .iter()
        .filter(|agent| agent.task.as_ref().is_none_or(|task| task.finished_at.is_none()))
        .collect();
    let hidden = working.len().saturating_sub(5);
    let mut lines = Vec::new();
    let mut section = None;
    for agent in working.into_iter().take(5) {
        if section.is_none() {
            section = Some("active");
            lines.push(Line::from(Span::styled("  active".to_owned(), Style::default().fg(t().muted))));
        }
        let (icon, color) = match agent.status.as_str() {
            "completed" => ('✓', t().success),
            "failed" => ('✘', t().error),
            "stopped" => ('■', t().muted),
            "waiting" => ('?', t().keyword),
            // Someone else's agent is not this session's spinner to turn.
            _ if agent.task.is_none() => ('@', t().accent),
            _ => (SPINNER[app.spinner % SPINNER.len()], t().accent),
        };
        // An owned agent is described by the work it was given and how long it
        // has had; a peer by what it published about itself.
        let detail = match &agent.task {
            Some(task) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                // A finished task's detail already carries what it cost,
                // duration included — appending an age printed the same span
                // twice on one line.
                let taken = elmee_core::text::how_long_for(now.saturating_sub(task.started_at));
                format!("{} · {taken}", task.detail)
            }
            None => format!("{} · {}", agent.status, agent.model),
        };
        let label_width = (width / 3).clamp(6, 16);
        // A name cut to exactly the column width leaves no gap, and the detail
        // begins mid-word against it: `architecture-audparent process ended`.
        // The ellipsis costs a character of the name and is worth it, because
        // it also says the name was cut rather than being that.
        let label = fit(&agent.name, label_width);
        let head = format!(" {icon} {label:<label_width$} · ");
        let tail = fit(&detail, width.saturating_sub(UnicodeWidthStr::width(head.as_str())));
        lines.push(Line::from(vec![
            Span::styled(head, Style::default().fg(color)),
            Span::styled(tail, Style::default().fg(t().muted)),
        ]));
    }
    if hidden > 0 {
        let more = fit(&format!("   +{hidden} more · /agents"), width);
        lines.push(Line::from(Span::styled(more, Style::default().fg(t().muted))));
    }
    lines
}
fn kchars(value: usize) -> String {
    if value < 1000 { format!("{value}") } else { format!("{:.1}k", value as f64 / 1000.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A narrow window used to drop the model before the agent's own name, so
    /// switching models in an eighty-column terminal changed nothing visible.
    #[test]
    fn a_narrow_window_keeps_the_model_after_it_has_given_up_the_identity() {
        let mut app = App::new(
            "lmstudio/qwen3.8-27b-mlx".into(),
            "session".into(),
            crate::commands::builtin(),
            std::env::temp_dir(),
            false,
        );
        app.identity = "agent-1w742r6lt136t".into();
        let rendered = |width| {
            status_line(&app, width).spans.iter().map(|span| span.content.to_string()).collect::<String>()
        };
        assert!(rendered(120).contains("lmstudio/qwen3.8-27b-mlx"));
        // The window that reported this: dropping a count of nobody is what
        // buys the model its room back at eighty columns.
        let reported = rendered(78);
        assert!(reported.contains("lmstudio/qwen3.8-27b-mlx"), "{reported}");
        assert!(!reported.contains("0 agents"), "a count of nobody costs the width the model needs");
        // Narrower still, the name this agent answers to goes before the model
        // does: one changes once a session, the other is what was just chosen.
        let narrow = rendered(60);
        assert!(narrow.contains("lmstudio/qwen3.8-27b-mlx"), "{narrow}");
        assert!(!narrow.contains("agent-1w742r6lt136t"), "{narrow}");
    }
}
