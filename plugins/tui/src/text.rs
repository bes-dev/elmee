//! Styled text: a span is words with a style, a line is spans. This is the
//! whole of what the interface needs from a text model, and having it here is
//! what lets the frontend depend on the terminal alone — a widget toolkit was
//! being linked for these five types and nothing else.

use std::borrow::Cow;

use unicode_width::UnicodeWidthStr;

/// A colour a terminal understands: the named sixteen, an index, or true
/// colour. Parsed from the words a theme file uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Reset,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    White,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl std::str::FromStr for Color {
    type Err = ();

    fn from_str(text: &str) -> Result<Self, ()> {
        let name = text.trim().to_ascii_lowercase().replace(['_', '-'], " ");
        if let Some(hex) = name.strip_prefix('#')
            && hex.len() == 6
            && let Ok(value) = u32::from_str_radix(hex, 16)
        {
            return Ok(Self::Rgb((value >> 16) as u8, (value >> 8) as u8, value as u8));
        }
        if let Ok(index) = name.parse::<u8>() {
            return Ok(Self::Indexed(index));
        }
        Ok(match name.as_str() {
            "reset" => Self::Reset,
            "black" => Self::Black,
            "red" => Self::Red,
            "green" => Self::Green,
            "yellow" => Self::Yellow,
            "blue" => Self::Blue,
            "magenta" => Self::Magenta,
            "cyan" => Self::Cyan,
            "gray" | "grey" => Self::Gray,
            "dark gray" | "dark grey" | "darkgray" | "darkgrey" => Self::DarkGray,
            "light red" | "lightred" => Self::LightRed,
            "light green" | "lightgreen" => Self::LightGreen,
            "light yellow" | "lightyellow" => Self::LightYellow,
            "light blue" | "lightblue" => Self::LightBlue,
            "light magenta" | "lightmagenta" => Self::LightMagenta,
            "light cyan" | "lightcyan" => Self::LightCyan,
            "white" => Self::White,
            _ => return Err(()),
        })
    }
}

/// Text attributes, as a set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifier(u8);

impl Modifier {
    pub const BOLD: Self = Self(1);
    pub const DIM: Self = Self(2);
    pub const ITALIC: Self = Self(4);
    pub const UNDERLINED: Self = Self(8);
    pub const CROSSED_OUT: Self = Self(16);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The SGR codes that switch each attribute on.
    pub fn codes(self) -> impl Iterator<Item = &'static str> {
        [
            (Self::BOLD, "1"),
            (Self::DIM, "2"),
            (Self::ITALIC, "3"),
            (Self::UNDERLINED, "4"),
            (Self::CROSSED_OUT, "9"),
        ]
        .into_iter()
        .filter(move |(modifier, _)| self.contains(*modifier))
        .map(|(_, code)| code)
    }
}

impl std::ops::BitOr for Modifier {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub add_modifier: Modifier,
}

impl Style {
    pub fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }

    pub fn add_modifier(mut self, modifier: Modifier) -> Self {
        self.add_modifier = self.add_modifier | modifier;
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub content: Cow<'static, str>,
    pub style: Style,
    /// Where the text leads, for a terminal that draws links. Only ever set
    /// by this interface for paths of its own; page text never carries one.
    pub link: Option<String>,
}

impl Span {
    pub fn raw(content: impl Into<Cow<'static, str>>) -> Self {
        Self { content: content.into(), style: Style::default(), link: None }
    }

    pub fn styled(content: impl Into<Cow<'static, str>>, style: Style) -> Self {
        Self { content: content.into(), style, link: None }
    }

    pub fn width(&self) -> usize {
        UnicodeWidthStr::width(self.content.as_ref())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    pub fn raw(content: impl Into<Cow<'static, str>>) -> Self {
        Self { spans: vec![Span::raw(content)] }
    }

    pub fn width(&self) -> usize {
        self.spans.iter().map(Span::width).sum()
    }

    /// The words without their styles.
    #[cfg(test)]
    pub fn text(&self) -> String {
        self.spans.iter().map(|span| span.content.as_ref()).collect()
    }
}

impl From<Vec<Span>> for Line {
    fn from(spans: Vec<Span>) -> Self {
        Self { spans }
    }
}

impl From<Span> for Line {
    fn from(span: Span) -> Self {
        Self { spans: vec![span] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_parse_from_every_form_a_theme_writes_them_in() {
        assert_eq!("light blue".parse(), Ok(Color::LightBlue));
        assert_eq!("LightBlue".parse(), Ok(Color::LightBlue));
        assert_eq!("dark_gray".parse(), Ok(Color::DarkGray));
        assert_eq!("#ff8800".parse(), Ok(Color::Rgb(255, 136, 0)));
        assert_eq!("244".parse(), Ok(Color::Indexed(244)));
        assert_eq!("nope".parse::<Color>(), Err(()));
    }

    #[test]
    fn modifiers_combine_and_name_their_codes() {
        let both = Modifier::BOLD | Modifier::ITALIC;
        assert!(both.contains(Modifier::BOLD) && !both.contains(Modifier::DIM));
        assert_eq!(both.codes().collect::<Vec<_>>(), ["1", "3"]);
        assert_eq!(Line::from(vec![Span::raw("界"), Span::raw("a")]).width(), 3);
    }
}
