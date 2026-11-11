//! A tiny terminal syntax highlighter for fenced code blocks: keywords,
//! strings, numbers, and line comments. Deliberately not a grammar engine —
//! it covers the common languages in ~zero binary cost, in the spirit of the
//! rest of the UI.

use super::text::Span;
use super::text::{Modifier, Style};

use super::theme::t;

/// Space-separated keyword list plus the line-comment marker.
struct Lang {
    keywords: &'static str,
    comment: &'static str,
}

fn lang_for(tag: &str) -> Lang {
    const RUST: &str = "as async await break const continue crate dyn else enum extern false fn for if \
        impl in let loop match mod move mut pub ref return self Self static struct super trait true type \
        unsafe use where while";
    const PYTHON: &str = "False None True and as assert async await break class continue def del elif \
        else except finally for from global if import in is lambda not or pass raise return try while \
        with yield";
    const JS: &str = "async await break case catch class const continue default do else enum export \
        extends false finally for from function if implements import in instanceof interface let new null \
        of readonly return static switch this throw true try type typeof undefined var while";
    const GO: &str = "break case chan const continue default defer else fallthrough false for func go \
        goto if import interface map nil package range return select struct switch true type var";
    const SH: &str = "case do done echo elif else esac exit export fi for function if in local return \
        then while";
    const C: &str = "auto bool break case char class const continue default do double else enum extern \
        false float for if int long new nullptr public private return short signed sizeof static struct \
        switch template this true typedef union unsigned void while";
    match tag {
        "rust" | "rs" => Lang { keywords: RUST, comment: "//" },
        "python" | "py" => Lang { keywords: PYTHON, comment: "#" },
        "js" | "jsx" | "ts" | "tsx" | "javascript" | "typescript" | "json" => {
            Lang { keywords: JS, comment: "//" }
        }
        "go" | "golang" => Lang { keywords: GO, comment: "//" },
        "sh" | "bash" | "zsh" | "shell" => Lang { keywords: SH, comment: "#" },
        "c" | "cpp" | "h" | "hpp" | "java" | "kt" | "kotlin" | "swift" => Lang { keywords: C, comment: "//" },
        "toml" | "yaml" | "yml" | "ini" => Lang { keywords: "", comment: "#" },
        "sql" | "lua" => Lang { keywords: "", comment: "--" },
        _ => Lang { keywords: "", comment: "" },
    }
}

/// Language tag for a file path, judged by its extension.
pub fn lang_for_path(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "rs" => "rust",
        "py" => "python",
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "json" => "js",
        "go" => "go",
        "sh" | "bash" | "zsh" => "sh",
        "c" | "h" | "cpp" | "hpp" | "cc" | "java" | "kt" | "swift" => "c",
        "toml" | "ini" => "toml",
        "yaml" | "yml" => "yaml",
        "sql" => "sql",
        "lua" => "lua",
        _ => "",
    }
}

/// Highlight one line of fenced code. Unknown languages still get strings,
/// numbers, and (where the tag implies one) comments.
pub fn highlight(line: &str, tag: &str) -> Vec<Span> {
    let lang = lang_for(tag);
    let chars: Vec<char> = line.chars().collect();
    let comment: Vec<char> = lang.comment.chars().collect();
    let mut spans: Vec<Span> = Vec::new();
    let mut plain = String::new();
    let mut index = 0;

    let flush = |plain: &mut String, spans: &mut Vec<Span>| {
        if !plain.is_empty() {
            spans.push(Span::raw(std::mem::take(plain)));
        }
    };

    while index < chars.len() {
        let character = chars[index];
        if !comment.is_empty() && chars[index..].starts_with(&comment) {
            flush(&mut plain, &mut spans);
            spans.push(Span::styled(
                chars[index..].iter().collect::<String>(),
                Style::default().fg(t().muted).add_modifier(Modifier::ITALIC),
            ));
            return finish(spans, plain);
        }
        if character == '"' || character == '\'' || character == '`' {
            flush(&mut plain, &mut spans);
            let mut literal = String::from(character);
            index += 1;
            while index < chars.len() {
                literal.push(chars[index]);
                if chars[index] == '\\' && index + 1 < chars.len() {
                    literal.push(chars[index + 1]);
                    index += 2;
                    continue;
                }
                index += 1;
                if literal.ends_with(character) && literal.len() > 1 {
                    break;
                }
            }
            spans.push(Span::styled(literal, Style::default().fg(t().success)));
            continue;
        }
        if character.is_ascii_digit() && !plain.chars().next_back().is_some_and(is_ident) {
            flush(&mut plain, &mut spans);
            let mut number = String::new();
            while index < chars.len()
                && (chars[index].is_ascii_alphanumeric() || chars[index] == '.' || chars[index] == '_')
            {
                number.push(chars[index]);
                index += 1;
            }
            spans.push(Span::styled(number, Style::default().fg(t().warning)));
            continue;
        }
        if is_ident(character) {
            let mut word = String::new();
            while index < chars.len() && is_ident(chars[index]) {
                word.push(chars[index]);
                index += 1;
            }
            if lang.keywords.split_ascii_whitespace().any(|keyword| keyword == word) {
                flush(&mut plain, &mut spans);
                spans.push(Span::styled(word, Style::default().fg(t().keyword)));
            } else {
                plain.push_str(&word);
            }
            continue;
        }
        plain.push(character);
        index += 1;
    }
    finish(spans, plain)
}

fn finish(mut spans: Vec<Span>, plain: String) -> Vec<Span> {
    if !plain.is_empty() {
        spans.push(Span::raw(plain));
    }
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

fn is_ident(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Color;

    fn joined(spans: &[Span]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn highlights_keywords_strings_numbers_comments() {
        let spans = highlight(r#"let x = "hi"; // answer 42"#, "rust");
        assert_eq!(joined(&spans), r#"let x = "hi"; // answer 42"#);
        assert!(spans.iter().any(|s| s.content == "let" && s.style.fg == Some(Color::Magenta)));
        assert!(spans.iter().any(|s| s.content == r#""hi""# && s.style.fg == Some(Color::Green)));
        assert!(
            spans.iter().any(|s| s.content.starts_with("// answer") && s.style.fg == Some(Color::DarkGray))
        );

        let spans = highlight("count = value_2 + 37", "python");
        assert_eq!(joined(&spans), "count = value_2 + 37");
        assert!(spans.iter().any(|s| s.content == "37" && s.style.fg == Some(Color::Yellow)));
        assert!(!spans.iter().any(|s| s.content.contains("value_2") && s.style.fg.is_some())); // ident digits stay plain
    }
}
