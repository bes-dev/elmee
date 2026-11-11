//! Terminal Markdown: what a reply is written in, drawn as a terminal can.
//!
//! Headings, paragraphs, bullet and numbered lists at any depth, quotes,
//! rules, tables, code fences with highlighting, and inline code, bold,
//! italic, strikethrough and links. Hand-written rather than a parser
//! dependency: the output is styled lines, and a full CommonMark tree would
//! be flattened straight back into them. Width-wrapping happens at render
//! time; a table is the one construct that lays itself out here, because its
//! columns have to agree across rows.

use super::text::{Line, Modifier, Span, Style};
use super::theme::t;

pub fn render(text: &str) -> Vec<Line> {
    render_from(text, None)
}

/// Render with an inherited fence state — used by the streaming preview when
/// its tail slice starts inside a code block whose opener scrolled away.
pub fn render_from(text: &str, initial_fence: Option<String>) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut fence = initial_fence;
    let raw_lines: Vec<&str> = text.lines().collect();
    let mut index = 0;
    while index < raw_lines.len() {
        let raw = raw_lines[index];
        index += 1;
        if let Some(tag) = raw.trim_start().strip_prefix("```") {
            // The fence itself is markup: what it encloses is drawn as code,
            // and that is the whole of what it had to say.
            fence = match fence {
                Some(_) => None,
                None => Some(fence_tag(tag)),
            };
            continue;
        }
        if let Some(tag) = &mut fence {
            // Untagged fence: guess the language from its first content line.
            if tag.is_empty() && !raw.trim().is_empty() {
                *tag = guess_language(raw).to_owned();
            }
            lines.push(Line::from(super::highlight::highlight(raw, tag)));
            continue;
        }
        if is_table_row(raw) && raw_lines.get(index).is_some_and(|next| is_table_rule(next)) {
            let end = (index + 1..raw_lines.len())
                .find(|at| !is_table_row(raw_lines[*at]))
                .unwrap_or(raw_lines.len());
            lines.extend(table(&raw_lines[index - 1..end]));
            index = end;
            continue;
        }
        lines.push(block(raw));
    }
    lines
}

/// One line that is not code and not part of a table.
fn block(raw: &str) -> Line {
    let trimmed = raw.trim_start();
    let indent = raw.len() - trimmed.len();
    if let Some(heading) = trimmed.strip_prefix('#') {
        let level = 1 + heading.chars().take_while(|character| *character == '#').count();
        let title = heading.trim_start_matches('#').trim_start().to_owned();
        let style = match level {
            1 => Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            2 => Style::default().add_modifier(Modifier::BOLD),
            _ => Style::default().fg(t().accent).add_modifier(Modifier::BOLD),
        };
        return Line::from(Span::styled(title, style));
    }
    if trimmed.len() >= 3
        && ["---", "***", "___"].iter().any(|rule| trimmed.chars().all(|c| rule.starts_with(c)))
    {
        return Line::from(Span::styled("─".repeat(40), Style::default().fg(t().border)));
    }
    if let Some(quoted) = trimmed.strip_prefix('>') {
        let mut spans = vec![Span::styled("▎ ".to_owned(), Style::default().fg(t().muted))];
        spans.extend(inline(quoted.trim_start()).into_iter().map(|mut span| {
            span.style = span.style.add_modifier(Modifier::ITALIC);
            span
        }));
        return Line::from(spans);
    }
    let item = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
        .map(|item| ("•".to_owned(), item));
    let numbered = trimmed.find(". ").and_then(|at| {
        let number = &trimmed[..at];
        (!number.is_empty() && number.chars().all(|c| c.is_ascii_digit()))
            .then(|| (format!("{number}."), &trimmed[at + 2..]))
    });
    if let Some((marker, rest)) = item.or(numbered) {
        let (mark, rest) = match rest.strip_prefix("[ ] ").map(|rest| ("☐", rest)).or_else(|| {
            rest.strip_prefix("[x] ").or_else(|| rest.strip_prefix("[X] ")).map(|rest| ("☑", rest))
        }) {
            Some((mark, rest)) => (mark.to_string(), rest),
            None => (marker, rest),
        };
        let mut spans = vec![
            Span::raw(" ".repeat(indent)),
            Span::styled(format!("{mark} "), Style::default().fg(t().warning)),
        ];
        spans.extend(inline(rest));
        return Line::from(spans);
    }
    Line::from(inline(raw))
}

pub(super) fn is_table_row(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') && trimmed.len() > 1
}

fn is_table_rule(line: &str) -> bool {
    is_table_row(line) && line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// A table with its columns lined up: the header bold, the rule drawn, every
/// cell padded to the widest in its column.
fn table(rows: &[&str]) -> Vec<Line> {
    let cells: Vec<Vec<&str>> = rows
        .iter()
        .filter(|row| !is_table_rule(row))
        .map(|row| row.trim().trim_matches('|').split('|').map(str::trim).collect())
        .collect();
    let columns = cells.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            cells.iter().map(|row| row.get(column).map_or(0, |cell| plain_width(cell))).max().unwrap_or(0)
        })
        .collect();
    let mut lines = Vec::new();
    for (index, row) in cells.iter().enumerate() {
        let mut spans = Vec::new();
        for (column, width) in widths.iter().enumerate() {
            let cell = row.get(column).copied().unwrap_or("");
            let mut cell_spans = inline(cell);
            if index == 0 {
                for span in &mut cell_spans {
                    span.style = span.style.add_modifier(Modifier::BOLD);
                }
            }
            spans.extend(cell_spans);
            spans.push(Span::raw(" ".repeat(width.saturating_sub(plain_width(cell)))));
            if column + 1 < columns {
                spans.push(Span::styled("  ".to_owned(), Style::default().fg(t().muted)));
            }
        }
        lines.push(Line::from(spans));
        if index == 0 {
            let rule = widths.iter().map(|width| "─".repeat(*width)).collect::<Vec<_>>().join("  ");
            lines.push(Line::from(Span::styled(rule, Style::default().fg(t().border))));
        }
    }
    lines
}

fn plain_width(text: &str) -> usize {
    Line::from(inline(text)).width()
}

/// Normalize a fence info string: "Rust,no_run ignore" → "rust".
pub fn fence_tag(info: &str) -> String {
    info.trim().split([' ', ',', ';']).next().unwrap_or_default().to_lowercase()
}

/// A cheap guess for untagged fences, judged by the first content line.
fn guess_language(line: &str) -> &'static str {
    let text = line.trim_start();
    if text.starts_with("#!") {
        "sh"
    } else if text.starts_with("#include") || text.starts_with("template<") {
        "c"
    } else if text.starts_with("fn ")
        || text.starts_with("pub ")
        || text.starts_with("let ")
        || text.starts_with("impl ")
        || text.contains("::")
    {
        "rust"
    } else if text.starts_with("def ") || text.starts_with("import ") || text.starts_with("from ") {
        "python"
    } else if text.starts_with("func ") || text.starts_with("package ") {
        "go"
    } else if text.starts_with("function") || text.starts_with("const ") || text.starts_with("export ") {
        "js"
    } else if text.starts_with("SELECT ") || text.starts_with("select ") {
        "sql"
    } else {
        ""
    }
}

/// The inline markers, in the order a scanner tries them at one position:
/// the marker, its closer, and the style it gives.
const MARKERS: &[(&str, &str)] =
    &[("`", "`"), ("**", "**"), ("__", "__"), ("~~", "~~"), ("*", "*"), ("_", "_")];

/// Split a line into styled spans for code, bold, italic, strikethrough and
/// links. A marker without a closer is plain text; a link shows its text with
/// the address dimmed after it, since a terminal cannot hide one in the other.
fn inline(text: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut rest = text;
    let mut plain = String::new();
    while !rest.is_empty() {
        if let Some((label, url, after)) = link_at(rest) {
            push_plain(&mut spans, std::mem::take(&mut plain));
            spans.push(Span::styled(
                label.to_owned(),
                Style::default().fg(t().accent).add_modifier(Modifier::UNDERLINED),
            ));
            spans.push(Span::styled(format!(" ({url})"), Style::default().fg(t().muted)));
            rest = after;
            continue;
        }
        let marked = MARKERS.iter().find_map(|(open, close)| {
            let body = rest.strip_prefix(open)?;
            let end = body.find(close).filter(|end| *end > 0)?;
            // `*` and `_` inside a word are not emphasis: snake_case stays.
            if (*open == "_" || *open == "*") && plain.chars().next_back().is_some_and(char::is_alphanumeric)
            {
                return None;
            }
            Some((*open, &body[..end], &body[end + close.len()..]))
        });
        match marked {
            Some((open, inner, after)) => {
                push_plain(&mut spans, std::mem::take(&mut plain));
                let style = match open {
                    "`" => Style::default().fg(t().accent),
                    "**" | "__" => Style::default().add_modifier(Modifier::BOLD),
                    "~~" => Style::default().add_modifier(Modifier::CROSSED_OUT),
                    _ => Style::default().add_modifier(Modifier::ITALIC),
                };
                if open == "`" {
                    spans.push(Span::styled(inner.to_owned(), style));
                } else {
                    spans.extend(inline(inner).into_iter().map(|mut span| {
                        span.style = Style {
                            fg: span.style.fg.or(style.fg),
                            bg: span.style.bg,
                            add_modifier: span.style.add_modifier | style.add_modifier,
                        };
                        span
                    }));
                }
                rest = after;
            }
            None => {
                let character = rest.chars().next().expect("non-empty");
                plain.push(character);
                rest = &rest[character.len_utf8()..];
            }
        }
    }
    push_plain(&mut spans, plain);
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

/// `[label](url)` at the start of `text`.
fn link_at(text: &str) -> Option<(&str, &str, &str)> {
    let body = text.strip_prefix('[')?;
    let close = body.find("](")?;
    let (label, after_label) = (&body[..close], &body[close + 2..]);
    let end = after_label.find(')')?;
    let url = &after_label[..end];
    (!label.is_empty() && !url.contains(' ')).then_some((label, url, &after_label[end + 1..]))
}

fn push_plain(spans: &mut Vec<Span>, text: String) {
    if !text.is_empty() {
        spans.push(Span::raw(text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line) -> String {
        line.text()
    }

    #[test]
    fn styles_code_bold_and_structure() {
        let lines = render("# Title\n- item with `code` and **bold**\n```rust\nlet x = 1;\n```");
        assert_eq!(lines.len(), 3, "the fences are not drawn");
        assert_eq!(lines[0].spans[0].content, "Title");
        assert_eq!(lines[1].spans[1].content, "• ");
        assert!(lines[1].spans.iter().any(|span| span.content == "code"));
        assert!(lines[1].spans.iter().any(|span| span.content == "bold"));
        assert_eq!(text(&lines[2]), "let x = 1;");
        assert!(lines[2].spans.iter().any(|span| span.content == "let")); // highlighted keyword
    }

    #[test]
    fn lists_quotes_rules_and_emphasis_are_drawn_as_themselves() {
        let lines = render(
            "1. first\n  2. nested\n- [x] done\n> quoted *softly*\n---\nsnake_case stays, ~~gone~~ and _this_",
        );
        assert_eq!(text(&lines[0]), "1. first");
        assert_eq!(text(&lines[1]), "  2. nested");
        assert_eq!(text(&lines[2]), "☑ done");
        assert!(text(&lines[3]).starts_with("▎ quoted softly"));
        assert!(
            lines[3]
                .spans
                .iter()
                .any(|span| span.content == "softly" && span.style.add_modifier.contains(Modifier::ITALIC))
        );
        assert!(text(&lines[4]).starts_with("────"));
        let last = &lines[5];
        assert_eq!(text(last), "snake_case stays, gone and this");
        assert!(last.spans.iter().any(|span| span.content == "gone" && span.style.add_modifier.contains(Modifier::CROSSED_OUT)));
        assert!(
            last.spans
                .iter()
                .any(|span| span.content == "this" && span.style.add_modifier.contains(Modifier::ITALIC))
        );
    }

    #[test]
    fn links_show_where_they_go_and_tables_line_up() {
        let lines = render("see [the docs](https://x.y/z) now\n| a | bb |\n|---|---|\n| ccc | d |");
        assert_eq!(text(&lines[0]), "see the docs (https://x.y/z) now");
        assert_eq!(text(&lines[1]), "a    bb");
        assert!(text(&lines[2]).starts_with("───  ──"));
        assert_eq!(text(&lines[3]), "ccc  d ");
        assert!(lines[1].spans[0].style.add_modifier.contains(Modifier::BOLD), "the header is bold");
    }
}
