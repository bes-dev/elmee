//! The terminal transcript: one finalized exchange at a time.
//!
//! A cell knows how to draw itself, so adding a kind of history entry is a
//! change in one place rather than a new arm in a shared match at the other
//! end of the file. Nothing here is aware of the live area or of layout — it
//! turns a cell into logical lines and stops.
//!
//! The two voices in a transcript are the two that must never be confused: a
//! prompt is set against a bar in the accent colour, and a reply hangs from a
//! dot, so a page of them reads as a conversation rather than a log.

use std::path::Path;

use super::app::{Cell, Picker, TOOL_PREVIEW_LINES};
use super::render::{SPINNER, wrap_line};
use super::text::{Line, Modifier, Span, Style};
use super::theme::t;

/// The gutter that marks quoted output, one place because it is one idea.
pub(super) fn gutter() -> Span {
    Span::styled("  │ ".to_owned(), Style::default().fg(t().muted))
}

/// The prompt's bar, on every one of its lines, on the prompt's background.
fn bar() -> Span {
    Span::styled("┃ ".to_owned(), user_style().fg(t().accent).add_modifier(Modifier::BOLD))
}

/// What the person said is set on a background of its own: a bar alone was
/// not enough to tell a prompt from a reply at a glance down a long log.
fn user_style() -> Style {
    Style { bg: (t().user_bg != super::text::Color::Reset).then_some(t().user_bg), ..Style::default() }
}

/// Wrap one finalized cell for permanent printing into the terminal history.
pub(super) fn rendered_cell(cell: &Cell, width: usize, cwd: &Path) -> Vec<Line> {
    let mut rows = Vec::new();
    for line in cell_lines(cell, 0, cwd, width) {
        rows.extend(hanging(line, width.max(1)));
    }
    rows
}

/// Whether a blank row separates a cell from the one before it.
///
/// A run of tool calls is one activity and reads as one block, a line each;
/// everything else is a change of voice and gets air. The gap belongs to the
/// cell that follows, because only then is it known what follows.
pub(super) fn gap(before: Option<&Cell>, cell: &Cell) -> bool {
    !matches!((before, cell), (None, _) | (Some(Cell::Tool { .. }), Cell::Tool { .. }))
}

/// Wrap a line so its continuation rows sit under its text, not under the
/// mark that opens it — a wrapped reply keeps its left edge.
pub(super) fn hanging(line: Line, width: usize) -> Vec<Line> {
    // The marks a transcript line opens with, and the reply's own two-column
    // continuation, which is a mark that happens to be blank.
    let indent = match line.spans.first().map(|span| span.content.as_ref()) {
        Some("● ") | Some("┃ ") | Some("✻ ") | Some("  ") | Some("  │ ") => {
            Some(line.spans[0].clone())
        }
        _ => None,
    };
    let Some(mark) = indent else { return wrap_line(line, width) };
    let rest = Line::from(line.spans[1..].to_vec());
    let continuation = Span::styled(" ".repeat(mark.width()), mark.style);
    // A bar and a gutter run down every row of what they mark; a dot does not.
    let continuation =
        if matches!(mark.content.as_ref(), "┃ " | "  │ ") { mark.clone() } else { continuation };
    // The prompt's rows are filled to the edge, so its background is a block
    // and not a ragged right margin.
    let filled = mark.content.as_ref() == "┃ ";
    wrap_line(rest, width.saturating_sub(mark.width()).max(1))
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let mut spans = vec![if index == 0 { mark.clone() } else { continuation.clone() }];
            spans.extend(row.spans);
            let line = Line::from(spans);
            if filled && line.width() < width {
                let mut line = line;
                line.spans.push(Span::styled(" ".repeat(width - line.width()), user_style()));
                return line;
            }
            line
        })
        .collect()
}

pub(super) fn cell_lines(cell: &Cell, spinner: usize, cwd: &Path, width: usize) -> Vec<Line> {
    match cell {
        Cell::User(text) => text
            .lines()
            .map(|line| Line::from(vec![bar(), Span::styled(line.to_owned(), user_style())]))
            .collect(),
        Cell::Assistant { text, .. } => reply(super::markdown::render(text)),
        Cell::Reasoning { text, done } => {
            // At full strength, and marked rather than dimmed. A run of tool
            // calls says what was done and never why; the reasoning is the only
            // place the why exists, and it used to be set in the same grey as a
            // tool's arguments — so a session read as a list of commands with
            // the thinking whispered between them.
            let mark = Style::default().fg(t().accent);
            let body = Style::default().add_modifier(Modifier::ITALIC);
            let thought = |line: &str| {
                Line::from(vec![Span::styled("✻ ".to_owned(), mark), Span::styled(line.to_owned(), body)])
            };
            let kept: Vec<&str> = match done {
                true => text.lines().filter(|line| !line.trim().is_empty()).collect(),
                // The tail while it streams, and the whole of it once it stops.
                false => text.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect(),
            };
            kept.into_iter().map(thought).collect()
        }
        Cell::Tool { name, arguments, output, ok } => {
            tool_lines(name, arguments, output.as_deref(), *ok, spinner, cwd, width)
        }
        // `$` rather than a tool's tick: nobody asked the model for this and
        // the model is never told about it.
        Cell::Shell { command, output, ok } => {
            let mut lines = vec![Line::from(vec![
                Span::styled("$ ".to_owned(), Style::default().fg(t().accent).add_modifier(Modifier::BOLD)),
                Span::raw(command.clone()),
            ])];
            let style = Style::default().fg(if *ok { t().muted } else { t().error });
            match output {
                None => lines
                    .push(Line::from(Span::styled(format!("  {}", SPINNER[spinner % SPINNER.len()]), style))),
                Some(output) => lines.extend(
                    output
                        .lines()
                        .map(|line| Line::from(vec![gutter(), Span::styled(line.to_owned(), style)])),
                ),
            }
            lines
        }
        Cell::Info(text) => text
            .lines()
            .map(|line| {
                Line::from(Span::styled(
                    line.to_owned(),
                    Style::default().fg(t().muted).add_modifier(Modifier::ITALIC),
                ))
            })
            .collect(),
        Cell::Error(text) => text
            .lines()
            .enumerate()
            .map(|(index, line)| {
                let prefix = if index == 0 { "✘ " } else { "  " };
                Line::from(Span::styled(format!("{prefix}{line}"), Style::default().fg(t().error)))
            })
            .collect(),
    }
}

/// A reply hangs from a dot on its first line, and the rest stands under it.
pub(super) fn reply(lines: Vec<Line>) -> Vec<Line> {
    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let mark = if index == 0 { "● " } else { "  " };
            let mut spans = vec![Span::styled(mark.to_owned(), Style::default().fg(t().agent))];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// A tool call as the transcript keeps it: one row saying what was done and
/// how it went, and under a failure the part of its output that says why.
///
/// One row because a call that worked is read once, on the way to the answer;
/// what it printed is a keypress away. A failure is the exception — one you
/// cannot see is one you cannot act on — and for a command the reason is at
/// the end of what it wrote, so that is the end shown.
fn tool_lines(
    name: &str,
    arguments: &str,
    output: Option<&str>,
    ok: bool,
    spinner: usize,
    cwd: &Path,
    width: usize,
) -> Vec<Line> {
    let exit = output.and_then(elmee_core::text::exit_status);
    let failed = output.is_some() && (!ok || exit.is_some_and(|code| code != 0));
    // Failed with nothing to show for it is a call whose turn was stopped.
    let stopped = failed && output == Some("");
    let (mark, color) = match output {
        None => (format!("{} ", SPINNER[spinner % SPINNER.len()]), t().accent),
        Some(_) if stopped => ("■ ".to_owned(), t().muted),
        Some(_) if failed => ("✘ ".to_owned(), t().error),
        Some(_) => ("✔ ".to_owned(), t().success),
    };
    let label = label(name);
    let (body, language) =
        output.map_or_else(Default::default, |output| tool_body(name, arguments, output, ok));
    let rows = body.lines().filter(|line| elmee_core::text::exit_status(line).is_none()).count();
    let note = match (output, exit) {
        (None, _) => String::new(),
        _ if stopped => "  stopped".to_owned(),
        (_, Some(code)) if code != 0 => format!("  exit {code}"),
        _ if failed || rows == 0 || matches!(name, "plan" | "ask_user" | "apply_patch") => String::new(),
        _ => format!("  {rows} {}", if rows == 1 { "line" } else { "lines" }),
    };
    let mut subject = subject(name, arguments, output, cwd);
    let taken = mark.chars().count() + label.chars().count() + 1 + note.chars().count();
    subject.content = super::render::fit(&subject.content, width.saturating_sub(taken).max(8)).into();
    let mut lines = vec![Line::from(vec![
        Span::styled(mark, Style::default().fg(color)),
        Span::styled(label, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        subject,
        Span::styled(note, Style::default().fg(t().muted)),
    ])];
    if !failed {
        // An edit is the one result worth reading where it happened: what
        // changed is the work, and a count of lines does not say what it was.
        if name == "apply_patch" && output.is_some() {
            let single = body.lines().filter(|line| line.starts_with("*** ")).count() == 1;
            let diff: Vec<&str> =
                body.lines().filter(|line| *line != "@@" && !(single && line.starts_with("*** "))).collect();
            lines.extend(diff.iter().take(DIFF_PREVIEW_LINES).map(|line| body_line(line, "")));
            if diff.len() > DIFF_PREVIEW_LINES {
                lines.push(more(diff.len() - DIFF_PREVIEW_LINES));
            }
        }
        return lines;
    }
    let mut said: Vec<&str> =
        body.lines().filter(|line| elmee_core::text::exit_status(line).is_none()).collect();
    while said.last().is_some_and(|line| line.trim().is_empty()) {
        said.pop();
    }
    let hidden = said.len().saturating_sub(TOOL_PREVIEW_LINES);
    let shown = if exit.is_some() { &said[hidden..] } else { &said[..said.len() - hidden] };
    lines.extend(shown.iter().map(|line| body_line(line, language)));
    if hidden > 0 {
        lines.push(more(hidden));
    }
    lines
}

/// How much of an edit is shown under its row before the rest is a keypress.
const DIFF_PREVIEW_LINES: usize = 8;

/// The row that says a preview stopped short, and how to see the rest.
fn more(hidden: usize) -> Line {
    Line::from(Span::styled(
        format!("  │ … {hidden} more · Ctrl+O"),
        Style::default().fg(t().muted).add_modifier(Modifier::ITALIC),
    ))
}

/// A tool's name as a person reads it: a server's tool by server and tool,
/// not by the separator the model's namespace needs.
fn label(name: &str) -> String {
    match name.strip_prefix("mcp__").and_then(|rest| rest.split_once("__")) {
        Some((server, tool)) => format!("{server}·{tool}"),
        None => name.to_owned(),
    }
}

/// What a tool was about, said the way a person would: the command, the path
/// — a link a terminal can open — the files a patch touches and by how much,
/// how far a plan has come, or what was answered.
fn subject(name: &str, arguments: &str, output: Option<&str>, cwd: &Path) -> Span {
    let muted = Style::default().fg(t().muted);
    let value: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
    let text = |key: &str| value.get(key).and_then(|field| field.as_str());
    match name {
        "read" | "view_image" | "upload" => text("path").map(|path| linked(path, cwd, muted)),
        "apply_patch" => text("patch").map(|patch| patched(patch, cwd, muted)),
        "plan" => value.get("steps").and_then(|steps| steps.as_array()).map(|steps| {
            let is = |step: &&serde_json::Value, status: &str| step["status"].as_str() == Some(status);
            let done = steps.iter().filter(|step| is(step, "done")).count();
            let doing = steps.iter().find(|step| is(step, "doing"));
            let doing = doing.and_then(|step| step.get("step")?.as_str()).unwrap_or_default();
            let now = if doing.is_empty() { String::new() } else { format!(" · {doing}") };
            Span::styled(format!("{done}/{}{now}", steps.len()), muted)
        }),
        "ask_user" => Some(Span::styled(answered(&value, output), muted)),
        _ => None,
    }
    // The project's own directory is where everything happens, so spelling
    // it out in every command only pushes the command itself off the row.
    .unwrap_or_else(|| {
        let here = cwd.to_string_lossy();
        let said = summarize_arguments(arguments);
        Span::styled(if here.len() > 1 { said.replace(here.as_ref(), ".") } else { said }, muted)
    })
}

/// A patch by the files it touches and the lines it adds and removes. One file
/// is named, and is a link; several are counted.
fn patched(patch: &str, cwd: &Path, style: Style) -> Span {
    let files: Vec<&str> = patch
        .lines()
        .filter_map(|line| {
            ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
                .iter()
                .find_map(|header| line.strip_prefix(header))
        })
        .collect();
    let count = |sign: char| patch.lines().filter(|line| line.starts_with(sign)).count();
    let change = format!(" +{} −{}", count('+'), count('-'));
    match files[..] {
        [file] => {
            let mut span = linked(file, cwd, style);
            span.content = format!("{file}{change}").into();
            span
        }
        _ => Span::styled(format!("{} files{change}", files.len()), style),
    }
}

/// A question by what was asked and what the person chose — the only place in
/// the transcript the choice is recorded.
fn answered(arguments: &serde_json::Value, output: Option<&str>) -> String {
    let headers: Vec<&str> = arguments
        .get("questions")
        .and_then(|questions| questions.as_array())
        .into_iter()
        .flatten()
        .filter_map(|question| question.get("header")?.as_str())
        .collect();
    let answers = output
        .and_then(|output| serde_json::from_str::<serde_json::Value>(output).ok())
        .and_then(|value| value.get("answers")?.as_object().cloned());
    match (answers, output) {
        (Some(answers), _) => headers
            .iter()
            .zip(answers.values())
            .map(|(header, answer)| {
                let chosen = match answer {
                    serde_json::Value::String(text) => text.clone(),
                    serde_json::Value::Array(items) => {
                        items.iter().filter_map(|item| item.as_str()).collect::<Vec<_>>().join(", ")
                    }
                    other => other.to_string(),
                };
                format!("{header} → {chosen}")
            })
            .collect::<Vec<_>>()
            .join(" · "),
        (None, Some(output)) => output.lines().next().unwrap_or_default().to_owned(),
        (None, None) => headers.join(" · "),
    }
}

/// A path as a link to the file it names, so a terminal that draws links
/// opens it on a click.
fn linked(path: &str, cwd: &Path, style: Style) -> Span {
    let mut span = Span::styled(path.to_owned(), style);
    let absolute = if Path::new(path).is_absolute() { Path::new(path).to_path_buf() } else { cwd.join(path) };
    span.link = Some(format!("file://{}", absolute.display()));
    span
}

/// One line of a tool's body: a diff line by its sign, code by its language.
fn body_line(line: &str, language: &'static str) -> Line {
    let mut spans = vec![gutter()];
    match (language.is_empty(), line.as_bytes().first()) {
        (true, _) if line.starts_with("*** ") => {
            spans.push(Span::styled(
                line.to_owned(),
                Style::default().fg(t().accent).add_modifier(Modifier::BOLD),
            ));
        }
        (true, _) if line.starts_with("@@") => {
            spans.push(Span::styled(line.to_owned(), Style::default().fg(t().muted)))
        }
        (true, Some(b'+')) => spans.push(Span::styled(line.to_owned(), Style::default().fg(t().success))),
        (true, Some(b'-')) => spans.push(Span::styled(line.to_owned(), Style::default().fg(t().error))),
        (true, _) => spans.push(Span::raw(line.to_owned())),
        (false, _) => spans.extend(super::highlight::highlight(line, language)),
    }
    Line::from(spans)
}

/// What a finished tool card shows, and the language to highlight it with. A
/// successful patch reports only which files it touched, so the patch is shown
/// instead — it is already a diff, and the -/+ colouring below is already
/// looking for one. Everything else shows its output.
pub(super) fn tool_body(name: &str, arguments: &str, output: &str, ok: bool) -> (String, &'static str) {
    match patch_body(name, arguments).filter(|_| ok) {
        Some(patch) => (patch, ""),
        None => (output.to_owned(), tool_language(name, arguments)),
    }
}

/// The patch without its envelope: the file headers and hunks are what a
/// person reads, and `*** Begin Patch` says nothing they did not know.
fn patch_body(name: &str, arguments: &str) -> Option<String> {
    (name == "apply_patch")
        .then(|| serde_json::from_str::<serde_json::Value>(arguments).ok())
        .flatten()
        .and_then(|value| value.get("patch")?.as_str().map(str::to_owned))
        .map(|patch| {
            patch
                .lines()
                .filter(|line| !matches!(*line, "*** Begin Patch" | "*** End Patch"))
                .collect::<Vec<_>>()
                .join("\n")
        })
}

/// Language for a tool's output: file reads are judged by their extension.
fn tool_language(name: &str, arguments: &str) -> &'static str {
    if name != "read" {
        return "";
    }
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| {
            value.get("path").and_then(|path| path.as_str()).map(super::highlight::lang_for_path)
        })
        .unwrap_or("")
}

/// Prefer the human-relevant argument (command/path/query) over raw JSON.
pub(super) fn summarize_arguments(arguments: &str) -> String {
    let text = serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| {
            ["command", "path", "query", "url", "patch", "id"]
                .iter()
                .find_map(|key| value.get(key).and_then(|field| field.as_str()).map(str::to_owned))
        })
        .unwrap_or_else(|| arguments.to_owned());
    // A patch says what it is on its first line, and it is always the same
    // line; the files it names come right after it and are the useful part.
    let flat = text.trim_start_matches("*** Begin Patch\n").replace('\n', " ⏎ ");
    if flat.chars().count() > 100 {
        let mut short: String = flat.chars().take(100).collect();
        short.push('…');
        short
    } else {
        flat
    }
}

/// Render the visible tail of a streaming reply, carrying the code-fence
/// state of the skipped prefix so highlighting survives when a block's
/// opening ``` has scrolled out of the preview.
pub(super) fn streaming_tail(text: &str, from: usize, keep: usize) -> Vec<Line> {
    let tail = tail_lines(&text[from..], keep);
    let lines = super::markdown::render_from(tail, fence_state(&text[..text.len() - tail.len()]));
    // The dot belongs to the reply's first line, which is only here while
    // nothing of the reply has been printed above yet.
    if from == 0 { reply(lines) } else { reply_continued(lines) }
}

fn reply_continued(lines: Vec<Line>) -> Vec<Line> {
    lines
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// The code fence a chunk of text starts inside, if it starts inside one.
///
/// Rendering a reply in pieces means each piece has to be told what the pieces
/// before it left open, or a code block loses its highlighting at every seam.
fn fence_state(prefix: &str) -> Option<String> {
    if prefix.matches("```").count().is_multiple_of(2) {
        return None;
    }
    let opener = prefix.rfind("```").map(|at| &prefix[at + 3..]).unwrap_or_default();
    Some(super::markdown::fence_tag(opener.lines().next().unwrap_or_default()))
}

/// The part of a reply that is finished enough to become history, and how far
/// into the text it reaches.
///
/// A line is the unit: a complete one will not change again, so it can be
/// printed above and forgotten. What is left is the line still being written,
/// which is all the live area has to redraw.
pub(super) fn flushable(text: &str, from: usize, whole: bool) -> (Vec<Line>, usize) {
    let end = if whole { text.len() } else { settled(text, from) };
    if end <= from {
        return (Vec::new(), from);
    }
    let lines = super::markdown::render_from(&text[from..end], fence_state(&text[..from]));
    // Nothing to draw yet — a fence that has only just opened — is nothing
    // printed, so the reply's first line is still to come and takes the dot.
    if lines.is_empty() {
        return (Vec::new(), from);
    }
    (if from == 0 { reply(lines) } else { reply_continued(lines) }, end)
}

/// How far the complete lines reach that will not be drawn differently later.
///
/// A table lays its columns out across all of its rows, so rows at the end of
/// what has arrived stay live until a line that is not one follows them.
fn settled(text: &str, from: usize) -> usize {
    let mut end = text[from..].rfind('\n').map_or(from, |at| from + at + 1);
    while end > from {
        let start = text[from..end - 1].rfind('\n').map_or(from, |at| from + at + 1);
        if !super::markdown::is_table_row(&text[start..end]) {
            break;
        }
        end = start;
    }
    end
}

/// The last `keep` raw lines of a text — the only part a preview can show.
fn tail_lines(text: &str, keep: usize) -> &str {
    let mut seen = 0;
    for (index, byte) in text.bytes().enumerate().rev() {
        if byte == b'\n' {
            seen += 1;
            if seen > keep {
                return &text[index + 1..];
            }
        }
    }
    text
}

/// A panel above the input: an expanded tool output, or a report the harness
/// was asked for. `hint` says how to dismiss it, because the key differs and a
/// panel that does not say how to leave it is a trap.
pub(super) fn overlay_lines(
    name: &str,
    body: &str,
    language: &'static str,
    hint: &str,
    width: usize,
    budget: usize,
) -> Vec<Line> {
    let total = body.lines().count();
    let mut rows = vec![Line::from(vec![
        Span::styled("⌄ ".to_owned(), Style::default().fg(t().accent)),
        Span::styled(label(name), Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(format!(" — {hint}"), Style::default().fg(t().muted)),
    ])];
    for line in body.lines() {
        rows.extend(wrap_line(body_line(line, language), width));
        if rows.len() >= budget {
            break;
        }
    }
    // Says what did not fit, rather than pointing at somewhere the rest is
    // supposed to be: the earlier note claimed "history", where a report never
    // goes. Nothing scrolls it — the screen is the screen.
    if rows.len() >= budget && budget > 1 {
        rows.truncate(budget - 1);
        let hidden = total.saturating_sub(budget - 2);
        rows.push(Line::from(Span::styled(
            format!("  │ … {hidden} more lines than this screen has room for"),
            Style::default().fg(t().muted).add_modifier(Modifier::ITALIC),
        )));
    }
    rows.push(Line::default());
    rows
}

/// A selector overlay (models / sessions / prompts / branches): the selection moves
/// with arrows, the active model is marked with a bullet.
pub(super) fn picker_lines(picker: &Picker, budget: usize) -> Vec<Line> {
    let muted = Style::default().fg(t().muted);
    let mut title = vec![
        Span::styled("⌄ ".to_owned(), Style::default().fg(t().accent)),
        Span::styled(picker.title.clone(), Style::default().add_modifier(Modifier::BOLD)),
    ];
    if !picker.filter.is_empty() {
        title.push(Span::styled(format!("  {}", picker.filter), Style::default().fg(t().accent)));
    }
    if picker.pending {
        title.push(Span::styled("  asking the endpoints…".to_owned(), muted.add_modifier(Modifier::ITALIC)));
    }
    let mut rows = vec![Line::from(title)];
    let shown = picker.shown();
    let window = budget.saturating_sub(3).max(1);
    for (index, (value, label)) in
        shown.iter().enumerate().skip(picker.index.saturating_sub(window - 1)).take(window)
    {
        let marker = if index == picker.index { "▸" } else { " " };
        let active = picker.active.as_deref() == Some(value.as_str());
        let bullet = if active { "● " } else { "  " };
        let style = if index == picker.index {
            Style::default().fg(t().accent).add_modifier(Modifier::BOLD)
        } else if active {
            Style::default().fg(t().success)
        } else {
            muted
        };
        rows.push(Line::from(Span::styled(format!("{marker}{bullet}{label}"), style)));
    }
    // Where you are in a list longer than the window, and — like every other
    // hint — the keys as the keymap resolves them.
    let position = match shown.len() {
        0 => "nothing matches · ".to_owned(),
        count if count > window => format!("{} of {count} · ", picker.index + 1),
        _ => String::new(),
    };
    rows.push(Line::from(Span::styled(
        format!("  {position}{}", super::keymap::hints(super::keymap::Mode::Picker)),
        muted.add_modifier(Modifier::ITALIC),
    )));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, arguments: serde_json::Value, output: Option<&str>, ok: bool) -> Vec<String> {
        let cell = Cell::Tool {
            name: name.into(),
            arguments: arguments.to_string(),
            output: output.map(str::to_owned),
            ok,
        };
        cell_lines(&cell, 0, Path::new("/project"), 80).iter().map(Line::text).collect()
    }

    /// A call is one row that says what was done and how it went, in a
    /// person's terms rather than as the JSON the model sent.
    #[test]
    fn a_tool_call_is_one_row_in_a_persons_terms() {
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-old\n+new\n+more\n*** End Patch";
        let edit = row("apply_patch", serde_json::json!({ "patch": patch }), Some("Applied"), true);
        assert_eq!(edit[0], "✔ apply_patch src/a.rs +2 −1");
        assert_eq!(edit[1..], ["  │ -old", "  │ +new", "  │ +more"], "and the edit itself is shown beneath");

        let steps = serde_json::json!({ "steps": [
            { "step": "read", "status": "done" }, { "step": "write", "status": "doing" }, { "step": "check" },
        ] });
        assert_eq!(row("plan", steps, Some("1/3 done"), true), ["✔ plan 1/3 · write"]);

        let question = serde_json::json!({ "questions": [{ "header": "Scope", "question": "How far?" }] });
        let answer = r#"{"answers":{"How far?":"This file"}}"#;
        assert_eq!(row("ask_user", question, Some(answer), true), ["✔ ask_user Scope → This file"]);

        let read = row("read", serde_json::json!({ "path": "src/a.rs" }), Some("one\ntwo"), true);
        assert_eq!(read, ["✔ read src/a.rs  2 lines"]);
        let here = row("exec", serde_json::json!({ "command": "cd /project/src && ls" }), None, true);
        assert_eq!(here, ["⠋ exec cd ./src && ls"], "the project's own path is not spelled out");
        let server = row("mcp__github__list_issues", serde_json::json!({}), Some("[]"), true);
        assert!(server[0].starts_with("✔ github·list_issues"), "{server:?}");
    }

    /// A command that exits non-zero ran, so the tool reports success — and
    /// used to be drawn with a tick. Its own exit status is what says how it
    /// went, and the end of what it wrote is where the reason is.
    #[test]
    fn a_command_that_failed_is_shown_as_failed_with_its_last_words() {
        let command = serde_json::json!({ "command": "cargo test" });
        let output = "one\ntwo\nthree\nfour\nfive\nerror: it broke\n\n[exit status: 101]";
        let failed = row("exec", command.clone(), Some(output), true);
        assert_eq!(failed[0], "✘ exec cargo test  exit 101");
        assert_eq!(failed.last().unwrap(), "  │ … 2 more · Ctrl+O");
        assert!(failed.contains(&"  │ error: it broke".to_owned()), "{failed:?}");
        assert!(!failed.iter().any(|line| line.contains("exit status")), "the marker is not output");

        let followed = "done\n[run 7 finished with exit status 0]";
        assert_eq!(
            row("write_run", serde_json::json!({ "id": "7" }), Some(followed), true),
            ["✔ write_run 7  1 line"]
        );
        assert_eq!(row("exec", command.clone(), None, true), ["⠋ exec cargo test"]);
        assert_eq!(row("exec", command, Some(""), false), ["■ exec cargo test  stopped"]);
    }

    /// A row is cut to the window with its verdict kept, never wrapped.
    #[test]
    fn a_long_call_is_cut_to_one_row_and_keeps_how_it_went() {
        let long = serde_json::json!({ "command": "x".repeat(200) });
        let lines = row("exec", long, Some("out\n[exit status: 2]"), true);
        assert!(lines[0].chars().count() <= 80, "{}", lines[0].chars().count());
        assert!(lines[0].ends_with("…  exit 2"), "{}", lines[0]);
    }

    /// A run of calls reads as one block; a change of voice gets air.
    #[test]
    fn only_a_change_of_voice_is_set_apart() {
        let tool = || Cell::Tool { name: "read".into(), arguments: "{}".into(), output: None, ok: true };
        let said = Cell::Assistant { text: "hi".into(), done: true, flushed: 0 };
        assert!(!gap(Some(&tool()), &tool()));
        assert!(gap(Some(&said), &tool()));
        assert!(gap(Some(&tool()), &said));
        assert!(!gap(None, &said), "nothing above the first thing printed");
    }
}
