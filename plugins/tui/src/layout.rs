//! How the terminal live area divides its rows.
//!
//! The rule the whole file exists to enforce: a section's height is *measured*
//! after it is built, never predicted before. The previous arrangement added
//! the pieces up by hand —
//!
//! ```text
//! let budget = max_rows - (shown + 2 + 1 + 1 + menu.len() + tasks.len());
//! ```
//!
//! — so every new section had to be remembered in an expression far away from
//! itself, and the magic constants stood for borders and padding nobody could
//! name any more. Here each section reports its own size, and the one flexible
//! section is handed exactly what is left. Adding a section cannot silently
//! starve another, because nothing predicts anything.

use super::text::Line;

pub enum Section<'a> {
    Fixed(Vec<Line>),
    /// Built last, from the rows the fixed sections did not take. There is at
    /// most one: two claimants on the same remainder would need a policy, and
    /// a policy here is what the arithmetic above already was.
    Flexible(Box<dyn FnOnce(usize) -> Vec<Line> + 'a>),
}

impl<'a> Section<'a> {
    pub fn fixed(rows: Vec<Line>) -> Self {
        Self::Fixed(rows)
    }

    pub fn flexible(build: impl FnOnce(usize) -> Vec<Line> + 'a) -> Self {
        Self::Flexible(Box::new(build))
    }
}

/// The assembled rows, plus where each section starts within them — the cursor
/// belongs to one particular section and has to be placed relative to it.
pub struct Stacked {
    pub rows: Vec<Line>,
    pub offsets: Vec<usize>,
}

pub fn stack(sections: Vec<Section<'_>>, max_rows: usize) -> Stacked {
    let fixed: usize = sections
        .iter()
        .map(|section| match section {
            Section::Fixed(rows) => rows.len(),
            Section::Flexible(_) => 0,
        })
        .sum();
    let mut room = max_rows.saturating_sub(fixed);
    let mut rows = Vec::with_capacity(max_rows);
    let mut offsets = Vec::with_capacity(sections.len());
    for section in sections {
        offsets.push(rows.len());
        match section {
            Section::Fixed(fixed) => rows.extend(fixed),
            Section::Flexible(build) => {
                let built = build(room);
                room = room.saturating_sub(built.len());
                rows.extend(built);
            }
        }
    }
    Stacked { rows, offsets }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(count: usize) -> Vec<Line> {
        (0..count).map(|index| Line::raw(index.to_string())).collect()
    }

    #[test]
    fn the_flexible_section_gets_exactly_what_the_others_left() {
        let stacked =
            stack(vec![Section::flexible(rows), Section::fixed(rows(3)), Section::fixed(rows(2))], 10);
        assert_eq!(stacked.rows.len(), 10);
        assert_eq!(stacked.offsets, vec![0, 5, 8], "each section knows where it starts");
    }

    #[test]
    fn fixed_sections_that_already_overflow_leave_the_flexible_one_nothing() {
        // Squeezing the input box out of view to make room for a preview would
        // be the wrong trade: the row you type into is the one row that must
        // survive a short window.
        let stacked = stack(vec![Section::flexible(rows), Section::fixed(rows(9))], 4);
        assert_eq!(stacked.rows.len(), 9);
        assert_eq!(stacked.offsets, vec![0, 0]);
    }
}
