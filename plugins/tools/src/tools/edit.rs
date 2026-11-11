//! Exact, ordered text replacement for `apply_patch`.
//!
//! Position is expressed by the patch itself: an optional `@@` line and the
//! hunks before this one. There are deliberately no whitespace, punctuation,
//! language, or similarity guesses here.

/// Replace the first exact occurrence at or after `from` and return the byte
/// immediately after the replacement. That byte is where the next hunk starts
/// looking, so repeated source text is unambiguous without extra context.
pub(super) fn edit(
    content: &str,
    old: &str,
    new: &str,
    from: usize,
    require_unique: bool,
) -> Result<(String, usize), String> {
    let source = line_spans(content);
    let expected: Vec<&str> = old.split('\n').collect();
    let matches = source
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            if line.start < from || index + expected.len() > source.len() {
                return None;
            }
            source[index..index + expected.len()]
                .iter()
                .zip(&expected)
                .all(|(line, expected)| &content[line.start..line.body_end] == *expected)
                .then_some(index)
        })
        .collect::<Vec<_>>();
    let Some(&index) = matches.first() else {
        return Err(format!(
            "expected text was not found after the preceding hunk; it must match exactly{}",
            window(content, &source, from, expected.len())
        ));
    };
    if require_unique && matches.len() > 1 {
        return Err(format!(
            "expected text occurs {} times; add an @@ exact line before the intended occurrence",
            matches.len()
        ));
    }

    let matched = &source[index..index + expected.len()];
    let start = matched[0].start;
    let end = matched.last().expect("expected is non-empty").end;
    let ending = &content[matched[0].body_end..matched[0].end];
    let ending = if ending.is_empty() { "\n" } else { ending };
    let mut replacement = new.replace('\n', ending);
    if !replacement.is_empty()
        && matched.last().is_some_and(|line| line.end > line.body_end)
        && !replacement.ends_with(ending)
    {
        replacement.push_str(ending);
    }

    let mut result = String::with_capacity(content.len() - (end - start) + replacement.len());
    result.push_str(&content[..start]);
    result.push_str(&replacement);
    result.push_str(&content[end..]);
    Ok((result, start + replacement.len()))
}

/// Find one exact complete line and return the byte after its line ending.
/// The error is the number of matches, including zero.
pub(super) fn after_line(content: &str, line: &str, from: usize) -> Result<usize, usize> {
    let matches = line_spans(content)
        .into_iter()
        .filter(|candidate| candidate.start >= from && &content[candidate.start..candidate.body_end] == line)
        .map(|candidate| candidate.end)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [after] => Ok(*after),
        _ => Err(matches.len()),
    }
}

#[derive(Clone, Copy)]
struct Line {
    start: usize,
    body_end: usize,
    end: usize,
}

fn line_spans(content: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (newline, _) in content.match_indices('\n') {
        let body_end = newline - usize::from(newline > start && content.as_bytes()[newline - 1] == b'\r');
        lines.push(Line { start, body_end, end: newline + 1 });
        start = newline + 1;
    }
    if start < content.len() {
        lines.push(Line { start, body_end: content.len(), end: content.len() });
    }
    lines
}

fn window(content: &str, lines: &[Line], from: usize, wanted: usize) -> String {
    const MAX_LINES: usize = 8;
    let Some(first) = lines.iter().position(|line| line.start >= from) else {
        return "; the search position is at end of file".into();
    };
    let shown = wanted.clamp(1, MAX_LINES);
    let body = lines[first..]
        .iter()
        .take(shown)
        .enumerate()
        .map(|(offset, line)| format!("{:>6} | {}", first + offset + 1, &content[line.start..line.body_end]))
        .collect::<Vec<_>>()
        .join("\n");
    format!("; current text at the search position:\n{body}")
}
