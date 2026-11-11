//! A pi-style terminal renderer (see pi-mono's TuiMainScreen): the UI is a
//! document in the main screen. Finalized lines are printed once and become
//! ordinary terminal history; the live tail (streaming preview, input box,
//! status) is redrawn in place with relative cursor moves. No alternate
//! screen, no fixed viewport — the input always sits directly under the
//! content, and native scrollback just works.

use std::io::{self, Write};
use std::{borrow::Cow, fmt::Write as _};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::text::{Color, Line, Span};

/// The one animated glyph in the interface. Shared because both a running
/// tool and the status line show the same motion, and two spinners drifting
/// out of phase reads as two different things happening.
pub const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Fit a line to the terminal, saying when something was dropped.
///
/// Measured in display columns rather than characters: a wide glyph is one
/// character and two columns, so a row counted in characters overflows the
/// window. The ellipsis matters more than the character it costs — a line
/// silently chopped mid-word reads as a bug in whatever produced it.
pub(super) fn fit(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    let mut kept = String::new();
    let mut used = 0;
    for character in text.chars() {
        let columns = character.width().unwrap_or(0);
        if used + columns + 1 > width {
            break;
        }
        kept.push(character);
        used += columns;
    }
    kept.push('…');
    kept
}

/// Wrap a line to `width` columns at the spaces between words, keeping each
/// span's style across the break. A word wider than the whole line is the
/// one thing that breaks mid-word, because nothing else can be done with it.
///
/// Breaking wherever the column ran out — which is what this did — put the
/// second half of a word at the start of the next row, and a reply that
/// wrapped read as if the model had stuttered.
pub(super) fn wrap_line(line: Line, width: usize) -> Vec<Line> {
    let width = width.max(1);
    // Every piece of the line split at spaces, so a break can only happen
    // between words.
    let mut pieces: Vec<Span> = Vec::new();
    for span in line.spans {
        let text = sanitized(&span.content).into_owned();
        let mut word = String::new();
        for character in text.chars() {
            word.push(character);
            if character == ' ' {
                pieces.push(Span { content: std::mem::take(&mut word).into(), ..span.clone() });
            }
        }
        if !word.is_empty() {
            pieces.push(Span { content: word.into(), ..span.clone() });
        }
    }
    // A word is what stands between two spaces, however many styles it is
    // drawn in: the full stop after a piece of inline code belongs to it, and
    // a break there would start a row with a lone punctuation mark.
    let mut words: Vec<Vec<Span>> = vec![Vec::new()];
    for piece in pieces {
        let ends = piece.content.ends_with(' ');
        words.last_mut().expect("at least one word").push(piece);
        if ends {
            words.push(Vec::new());
        }
    }
    let mut rows: Vec<Line> = Vec::new();
    let mut current: Vec<Span> = Vec::new();
    let mut used = 0;
    for word in words.into_iter().filter(|word| !word.is_empty()) {
        // A trailing space may hang past the edge: it draws nothing there.
        let full: usize = word.iter().map(|piece| UnicodeWidthStr::width(piece.content.as_ref())).sum();
        let trailing =
            word.last().map_or(0, |piece| piece.content.len() - piece.content.trim_end_matches(' ').len());
        let visible_width = full - trailing;
        if used + visible_width > width && used > 0 {
            rows.push(finished_row(std::mem::take(&mut current)));
            used = 0;
        }
        if visible_width > width {
            // Too wide for any row: cut it by columns, the old way.
            for piece in &word {
                for character in piece.content.chars() {
                    let character_width = character.width().unwrap_or(0);
                    if used + character_width > width && used > 0 {
                        rows.push(finished_row(std::mem::take(&mut current)));
                        used = 0;
                    }
                    push_char(&mut current, piece, character);
                    used += character_width;
                }
            }
            continue;
        }
        for piece in word {
            // A row that follows a break starts at its word; the first row
            // keeps the indentation the line was written with.
            let content = if used == 0 && !rows.is_empty() {
                piece.content.trim_start_matches(' ')
            } else {
                &piece.content
            };
            if !content.is_empty() {
                used += UnicodeWidthStr::width(content);
                current.push(Span { content: content.to_owned().into(), ..piece.clone() });
            }
        }
    }
    rows.push(finished_row(current));
    rows
}

/// A row ends where its words do: the space that would have led to the next
/// word draws nothing at the edge and would count against the width.
fn finished_row(mut spans: Vec<Span>) -> Line {
    while let Some(last) = spans.last_mut() {
        let trimmed = last.content.trim_end_matches(' ').len();
        if trimmed == last.content.len() {
            break;
        }
        last.content.to_mut().truncate(trimmed);
        if last.content.is_empty() {
            spans.pop();
        }
    }
    Line::from(spans)
}

fn push_char(current: &mut Vec<Span>, like: &Span, character: char) {
    match current.last_mut() {
        Some(last) if last.style == like.style && last.link == like.link => {
            last.content.to_mut().push(character);
        }
        _ => current.push(Span { content: character.to_string().into(), ..like.clone() }),
    }
}

/// Wrap by columns alone, breaking wherever the row is full. The input box
/// uses this rather than word wrapping because the cursor has to be placed
/// on the row the character it follows landed on, and a rule that moves
/// words between rows as they are typed makes that a guess; a rule that fills
/// each row is arithmetic. `cursor_in` is that arithmetic, kept beside it.
pub(super) fn wrap_chars(line: Line, width: usize) -> Vec<Line> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut current: Vec<Span> = Vec::new();
    let mut used = 0;
    for span in line.spans {
        for character in sanitized(&span.content).chars() {
            let character_width = character.width().unwrap_or(0);
            if used + character_width > width && used > 0 {
                rows.push(Line::from(std::mem::take(&mut current)));
                used = 0;
            }
            push_char(&mut current, &span, character);
            used += character_width;
        }
    }
    rows.push(Line::from(current));
    rows
}

/// Where the cursor sits after `characters` characters of a line wrapped by
/// [`wrap_chars`]: the row, and the column within it.
pub(super) fn cursor_in(line: &str, characters: usize, width: usize) -> (usize, usize) {
    let width = width.max(1);
    let (mut row, mut column) = (0, 0);
    for character in sanitized(line).chars().take(characters) {
        let character_width = character.width().unwrap_or(0);
        if column + character_width > width && column > 0 {
            row += 1;
            column = 0;
        }
        column += character_width;
    }
    (row, column)
}

#[derive(Default)]
pub struct LiveArea {
    /// Row within the live area where the hardware cursor was left.
    cursor_row: u16,
    /// ANSI text of each currently drawn live row, for line-level diffing.
    rows: Vec<String>,
}

impl LiveArea {
    /// Force a full redraw on the next frame (e.g. after a terminal resize).
    ///
    /// The cursor has not moved, so where it is parked is still true.
    pub fn invalidate(&mut self) {
        self.rows.clear();
    }

    /// Forget the drawn area *and* where the cursor was, for a caller that has
    /// just homed it itself.
    ///
    /// `frame` opens by walking up `cursor_row` rows to reach the top of the
    /// live area. After a screen clear the cursor is already at the top, so a
    /// stale row count sends it up from there — which the terminal clamps at the
    /// first line, quietly leaving elmee's idea of the cursor and the terminal's
    /// disagreeing for every frame after.
    pub fn reset(&mut self) {
        self.rows.clear();
        self.cursor_row = 0;
    }

    /// Print `statics` permanently, then redraw the live tail below them and
    /// park the cursor at `cursor` (row, column) within the live lines.
    /// When nothing was printed and the height is unchanged, only rows whose
    /// content differs from the previous frame are rewritten — a spinner tick
    /// costs one line, not the whole area.
    ///
    /// Every live row is clamped to `width`. The relative cursor moves below
    /// are counted in `Line`s, so a row the terminal wraps would be two rows
    /// where the arithmetic expects one, and every later frame would land
    /// lower than the last — the live area marching down the screen, repeating
    /// itself. Clamping here makes "one line is one row" true by construction
    /// rather than something each caller has to remember.
    pub fn frame(
        &mut self,
        out: &mut impl Write,
        statics: &[Line],
        live: &[Line],
        cursor: (u16, u16),
        width: usize,
    ) -> io::Result<()> {
        let new: Vec<String> = live.iter().map(|line| clamp(line, width)).collect();
        let mut buffer = String::with_capacity(256);
        // One frame is one update: a terminal that knows synchronized output
        // shows it whole, and one that does not ignores the request.
        buffer.push_str("\x1b[?2026h\x1b[?25l\r");
        if self.cursor_row > 0 {
            let _ = write!(buffer, "\x1b[{}A", self.cursor_row);
        }
        // Same-height diffing only: growing or shrinking the area needs real
        // newlines to scroll at the screen bottom, so those take the full path.
        let row_after = if statics.is_empty() && new.len() == self.rows.len() && !new.is_empty() {
            let mut row = 0u16;
            for (index, line) in new.iter().enumerate() {
                if self.rows[index] == *line {
                    continue;
                }
                let target = index as u16;
                if target > row {
                    let _ = write!(buffer, "\x1b[{}B", target - row);
                }
                row = target;
                buffer.push_str("\x1b[2K");
                buffer.push_str(line);
                buffer.push('\r');
            }
            row
        } else {
            for line in statics {
                buffer.push_str("\x1b[2K");
                buffer.push_str(&ansi(line));
                buffer.push_str("\r\n");
            }
            for (index, line) in new.iter().enumerate() {
                buffer.push_str("\x1b[2K");
                buffer.push_str(line);
                if index + 1 < new.len() {
                    buffer.push_str("\r\n");
                }
            }
            buffer.push_str("\x1b[J"); // erase leftovers from a taller previous frame
            new.len().saturating_sub(1) as u16
        };
        let (row, column) = cursor;
        let row = row.min(new.len().saturating_sub(1) as u16);
        if row_after > row {
            let _ = write!(buffer, "\x1b[{}A", row_after - row);
        } else if row > row_after {
            let _ = write!(buffer, "\x1b[{}B", row - row_after);
        }
        buffer.push('\r');
        if column > 0 {
            let _ = write!(buffer, "\x1b[{column}C");
        }
        buffer.push_str("\x1b[?25h\x1b[?2026l");
        out.write_all(buffer.as_bytes())?;
        out.flush()?;
        self.cursor_row = row;
        self.rows = new;
        Ok(())
    }

    /// Leave the cursor on a fresh line below everything that was drawn.
    pub fn finish(&mut self, out: &mut impl Write) -> io::Result<()> {
        let down = (self.rows.len() as u16).saturating_sub(self.cursor_row + 1);
        if down > 0 {
            write!(out, "\x1b[{down}B")?;
        }
        write!(out, "\r\n\x1b[?25h")?;
        out.flush()
    }
}

/// Serialize one styled line to ANSI, cut to `width` display columns.
fn clamp(line: &Line, width: usize) -> String {
    if line.width() <= width {
        return ansi(line);
    }
    let mut kept = Vec::new();
    let mut room = width.saturating_sub(1); // the ellipsis needs a column
    for span in &line.spans {
        let span_width = span.width();
        if span_width <= room {
            room -= span_width;
            kept.push(span.clone());
            continue;
        }
        let mut content = String::new();
        for character in span.content.chars() {
            let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
            if character_width > room {
                break;
            }
            room -= character_width;
            content.push(character);
        }
        kept.push(Span { content: content.into(), ..span.clone() });
        break;
    }
    kept.push(Span::raw("…"));
    ansi(&Line::from(kept))
}

/// Serialize one styled line to ANSI. A span with a link is wrapped in OSC 8,
/// which a terminal that draws links makes clickable and every other one
/// ignores.
pub(super) fn ansi(line: &Line) -> String {
    let mut output = String::new();
    for span in &line.spans {
        let content = sanitized(&span.content);
        let mut codes: Vec<String> = span.style.add_modifier.codes().map(str::to_owned).collect();
        if let Some(code) = span.style.fg.and_then(|color| color_code(color, 30)) {
            codes.push(code);
        }
        if let Some(code) = span.style.bg.and_then(|color| color_code(color, 40)) {
            codes.push(code);
        }
        if let Some(link) = &span.link {
            let _ = write!(output, "\x1b]8;;{}\x1b\\", sanitized(link));
        }
        if codes.is_empty() {
            output.push_str(&content);
        } else {
            let _ = write!(output, "\x1b[{}m{}\x1b[0m", codes.join(";"), content);
        }
        if span.link.is_some() {
            output.push_str("\x1b]8;;\x1b\\");
        }
    }
    output
}

/// Terminal output is an active protocol. Never let model/tool/file content
/// inject OSC, CSI, clipboard, title, or cursor-control sequences into it.
pub(super) fn sanitized(text: &str) -> Cow<'_, str> {
    if !text.chars().any(|character| character == '\t' || character.is_control()) {
        return Cow::Borrowed(text);
    }
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\t' => output.push_str("    "),
            character if character.is_control() => {}
            character => output.push(character),
        }
    }
    Cow::Owned(output)
}

/// An SGR colour code: the named palette from `base` (30 for text, 40 for
/// background), plus 256-colour and true-colour values.
fn color_code(color: Color, base: u16) -> Option<String> {
    let named = match color {
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 60,
        Color::LightRed => 61,
        Color::LightGreen => 62,
        Color::LightYellow => 63,
        Color::LightBlue => 64,
        Color::LightMagenta => 65,
        Color::LightCyan => 66,
        Color::White => 67,
        Color::Indexed(index) => return Some(format!("{};5;{index}", base + 8)),
        Color::Rgb(red, green, blue) => return Some(format!("{};2;{red};{green};{blue}", base + 8)),
        Color::Reset => return None,
    };
    Some((base + named).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::{Modifier, Style};

    #[test]
    fn a_word_drawn_in_two_styles_is_still_one_word() {
        let line = Line::from(vec![
            Span::raw("see "),
            Span::styled("src/main.rs", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("."),
        ]);
        let rows: Vec<String> = wrap_line(line, 14).iter().map(Line::text).collect();
        assert_eq!(rows, ["see", "src/main.rs."], "the full stop stays with its word");
    }

    #[test]
    fn frame_prints_statics_once_and_redraws_live_in_place() {
        let mut area = LiveArea::default();
        let mut out = Vec::new();
        let statics = [Line::raw("history")];
        let live = [Line::raw("input"), Line::raw("status")];
        area.frame(&mut out, &statics, &live, (0, 2), 40).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("history\r\n"));
        assert!(text.contains("input"));
        assert!(text.ends_with("\x1b[1A\r\x1b[2C\x1b[?25h\x1b[?2026l")); // back up to the input row

        // Unchanged same-height frame: pure cursor bookkeeping, no rewrites.
        let mut second = Vec::new();
        area.frame(&mut second, &[], &live, (0, 2), 40).unwrap();
        let second = String::from_utf8(second).unwrap();
        assert!(!second.contains("\x1b[2K"));
        assert!(!second.contains("input"));

        // One changed row rewrites exactly that row.
        let changed = [Line::raw("input"), Line::raw("status ✱")];
        let mut third = Vec::new();
        area.frame(&mut third, &[], &changed, (0, 2), 40).unwrap();
        let third = String::from_utf8(third).unwrap();
        assert_eq!(third.matches("\x1b[2K").count(), 1);
        assert!(third.contains("status ✱") && !third.contains("input"));
    }

    #[test]
    fn a_live_row_never_exceeds_the_terminal_width() {
        // The bug this pins: a status line longer than the window wraps onto a
        // second row, the cursor arithmetic counts one, and the live area walks
        // down the screen leaving a copy of itself on every frame.
        let long = Line::from(vec![
            Span::styled("● ready ", Style::default().fg(Color::Green)),
            Span::raw("· @agent-22118 · 1 peers · /very/long/path/to/a/session/file.jsonl"),
        ]);
        let clamped = clamp(&long, 20);
        let visible: String = strip_ansi(&clamped);
        assert_eq!(UnicodeWidthStr::width(visible.as_str()), 20, "{visible:?}");
        assert!(visible.ends_with('…'));
        // A line that fits is untouched, ellipsis included.
        assert_eq!(clamp(&Line::raw("short"), 20), "short");
    }

    fn strip_ansi(text: &str) -> String {
        let mut output = String::new();
        let mut characters = text.chars();
        while let Some(character) = characters.next() {
            if character == '\x1b' {
                for escape in characters.by_ref() {
                    if escape == 'm' || escape == '\\' {
                        break;
                    }
                }
            } else {
                output.push(character);
            }
        }
        output
    }

    #[test]
    fn ansi_styles_only_what_is_styled_and_links_what_is_linked() {
        let line = Line::from(vec![
            Span::raw("plain"),
            Span::styled("hot", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        ]);
        assert_eq!(ansi(&line), "plain\x1b[1;31mhot\x1b[0m");
        let mut linked = Span::raw("src/a.rs");
        linked.link = Some("file:///work/src/a.rs".into());
        assert_eq!(ansi(&Line::from(linked)), "\x1b]8;;file:///work/src/a.rs\x1b\\src/a.rs\x1b]8;;\x1b\\");
        let shaded =
            Span { style: Style { bg: Some(Color::DarkGray), ..Style::default() }, ..Span::raw("you") };
        assert_eq!(ansi(&Line::from(shaded)), "\x1b[100myou\x1b[0m");
    }

    #[test]
    fn wrapping_breaks_between_words_and_cuts_only_what_cannot_fit() {
        let rows = wrap_line(Line::raw("the quick brown fox jumps"), 10);
        let texts: Vec<String> = rows.iter().map(Line::text).collect();
        assert_eq!(texts, ["the quick", "brown fox", "jumps"]);
        let indented = wrap_line(Line::raw("    let x = 1; // a nested line"), 20);
        assert_eq!(indented[0].text(), "    let x = 1; // a", "indentation is kept on the first row");
        // A word wider than the row is the one thing cut mid-word.
        let rows = wrap_line(Line::raw("supercalifragilistic ok"), 8);
        assert_eq!(rows.iter().map(Line::text).collect::<Vec<_>>(), ["supercal", "ifragili", "stic ok"]);
        // Styles survive the break, and so do wide glyphs' widths.
        let styled = Line::from(vec![
            Span::styled("bold words ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("plain"),
        ]);
        let rows = wrap_line(styled, 11);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(wrap_line(Line::raw("界界"), 3).len(), 2);
    }

    /// The input box fills rows and the cursor follows the same rule, so the
    /// two cannot disagree — which they did once the transcript's word wrap
    /// was applied to the box: the rows moved words, the cursor did not.
    #[test]
    fn the_input_box_and_its_cursor_share_one_rule() {
        let text = "the quick brown fox jumps over";
        let rows = wrap_chars(Line::raw(text), 10);
        assert_eq!(
            rows.iter().map(Line::text).collect::<Vec<_>>(),
            ["the quick ", "brown fox ", "jumps over"]
        );
        assert_eq!(cursor_in(text, 0, 10), (0, 0));
        assert_eq!(cursor_in(text, 10, 10), (0, 10), "after a full row the cursor sits at its end");
        assert_eq!(cursor_in(text, 11, 10), (1, 1));
        assert_eq!(cursor_in(text, text.chars().count(), 10), (2, 10));
        assert_eq!(cursor_in("界界界", 4, 3), (2, 2), "wide glyphs count their columns");
    }

    #[test]
    fn untrusted_text_cannot_emit_terminal_controls() {
        let output = ansi(&Line::raw("before\x1b]52;c;c2VjcmV0\x07after"));
        assert!(!output.contains('\x1b') && !output.contains('\x07'));
        assert!(output.contains("before") && output.contains("after"));
    }
}
