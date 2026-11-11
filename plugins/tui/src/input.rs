//! A terminal multi-line editor with emacs-style keys and prompt history.
//! Text is a plain `String` with `\n` separators; the cursor is a byte
//! offset kept on `char` boundaries.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::KeyCode;

/// Enough for months of ordinary use without letting a prompt containing a
/// pasted file turn the per-user history into an unbounded database.
const HISTORY_LIMIT: usize = 1_000;
const LOCK_STALE_AFTER: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Editor {
    text: String,
    cursor: usize,
    history: Vec<String>,
    history_pos: Option<usize>,
    /// What had been typed when history was entered.
    ///
    /// Walking history from a blank line means walking everything, which for
    /// anyone who has used the tool for a week is scrolling past a hundred
    /// prompts to reach one. Typing the first few characters first and then
    /// pressing Up narrows it to prompts that began that way — what a shell
    /// does, and the difference between history being usable and being there.
    history_prefix: String,
    stash: String,
    /// Active reverse history search: the query being typed, and how many
    /// matches back from the newest the cursor has stepped.
    search: Option<(String, usize)>,
    history_path: Option<PathBuf>,
    history_warning: Option<String>,
}

impl Editor {
    /// The main prompt editor shares one durable history across workspaces and
    /// sessions. Dialog editors deliberately keep using `default()` so answers
    /// to questions are never retained accidentally.
    pub fn with_global_history() -> Self {
        let Some(home) = std::env::var_os("HOME") else { return Self::default() };
        Self::with_history_path(PathBuf::from(home).join(".elmee/history.jsonl"))
    }

    fn with_history_path(path: PathBuf) -> Self {
        let mut editor = Self { history_path: Some(path.clone()), ..Self::default() };
        match load_history(&path) {
            Ok(history) => editor.history = history,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => editor.history_warning = Some(format!("cannot load input history: {error}")),
        }
        editor
    }

    /// Persistence failures do not eat the submitted prompt, but the TUI must
    /// make them visible instead of quietly promising history it did not save.
    pub fn take_history_warning(&mut self) -> Option<String> {
        self.history_warning.take()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// The query and matched entry while a reverse search is running.
    pub fn search_state(&self) -> Option<(&str, &str)> {
        let (query, back) = self.search.as_ref()?;
        Some((query, self.search_match(query, *back).unwrap_or_default()))
    }

    /// The query, the matches around the selected one, and which of them it is.
    ///
    /// One match at a time answers "is there one" but not "which one do I
    /// want", which is the question actually being asked — so the candidates
    /// are shown together and the part that matched is pointed at.
    pub fn search_matches(&self, window: usize) -> Option<(&str, Vec<&str>, usize)> {
        let (query, back) = self.search.as_ref()?;
        // Each prompt once, at its most recent use: the same line sent five
        // times is one thing to find, not five rows of it.
        let mut seen = std::collections::HashSet::new();
        let all: Vec<&str> = self
            .history
            .iter()
            .rev()
            .filter(|entry| entry.contains(query.as_str()) && seen.insert(entry.as_str()))
            .map(String::as_str)
            .collect();
        if all.is_empty() {
            return Some((query, Vec::new(), 0));
        }
        let selected = (*back).min(all.len() - 1);
        let first = selected.saturating_sub(window / 2).min(all.len().saturating_sub(window));
        let shown = all[first..(first + window).min(all.len())].to_vec();
        Some((query, shown, selected - first))
    }

    /// Ctrl+R steps backwards through matches; this steps forwards again.
    pub fn search_back(&mut self) {
        if let Some((query, back)) = self.search.take() {
            self.search = Some((query, back.saturating_sub(1)));
        }
    }

    /// Ctrl+R: open the search, or step to the next older match.
    pub fn search_step(&mut self) {
        let Some((query, back)) = self.search.take() else {
            self.search = Some((String::new(), 0));
            return;
        };
        let stepped = if self.search_match(&query, back + 1).is_some() { back + 1 } else { back };
        self.search = Some((query, stepped));
    }

    /// Accept the highlighted match into the editor, leaving search mode.
    pub fn search_accept(&mut self) {
        if let Some((query, back)) = self.search.take()
            && let Some(found) = self.search_match(&query, back)
        {
            let found = found.to_owned();
            self.replace(&found);
        }
    }

    pub fn search_cancel(&mut self) {
        self.search = None;
    }

    /// History entries in `range` that begin the way the line did, oldest
    /// first. An empty prefix matches everything, which is the old behaviour
    /// and the right one for someone who pressed Up on a blank line.
    fn matching(&self, from: usize, to: usize) -> impl DoubleEndedIterator<Item = usize> {
        let prefix = self.history_prefix.clone();
        let to = to.min(self.history.len());
        (from.min(to)..to).filter(move |index| self.history[*index].starts_with(&prefix))
    }

    /// The `back`-th most recent history entry containing `query`.
    fn search_match(&self, query: &str, back: usize) -> Option<&str> {
        self.history.iter().rev().filter(|entry| entry.contains(query)).nth(back).map(String::as_str)
    }

    /// The editing keys shared by every text field (main input and dialog
    /// free-text): movement, deletion, emacs kills, words as a unit, and plain
    /// insertion. Returns false for keys the caller owns (Enter, Esc, history,
    /// menus).
    pub fn handle_key(&mut self, code: KeyCode, control: bool, alt: bool) -> bool {
        // While searching, typing refines the query instead of the text.
        if let Some((query, back)) = &mut self.search {
            match code {
                KeyCode::Char(character) if !control => {
                    query.push(character);
                    *back = 0;
                }
                KeyCode::Backspace => {
                    query.pop();
                    *back = 0;
                }
                _ => return false,
            }
            return true;
        }
        // A word is what Alt and Ctrl make of an arrow, and what Alt makes of
        // Backspace — the bindings every shell and editor share.
        if (alt || control) && matches!(code, KeyCode::Left | KeyCode::Right | KeyCode::Backspace)
            || alt && matches!(code, KeyCode::Char('b' | 'f'))
        {
            match code {
                KeyCode::Left | KeyCode::Char('b') => self.word_left(),
                KeyCode::Right | KeyCode::Char('f') => self.word_right(),
                _ => self.kill_word(),
            }
            return true;
        }
        match (control, code) {
            (true, KeyCode::Char('a')) | (_, KeyCode::Home) => self.home(),
            (true, KeyCode::Char('e')) | (_, KeyCode::End) => self.end(),
            (true, KeyCode::Char('u')) => self.kill_to_start(),
            (true, KeyCode::Char('k')) => self.kill_to_end(),
            (true, KeyCode::Char('w')) => self.kill_word(),
            (false, KeyCode::Char(character)) if !alt => self.insert(character),
            (_, KeyCode::Left) => self.left(),
            (_, KeyCode::Right) => self.right(),
            (_, KeyCode::Backspace) => self.backspace(),
            (_, KeyCode::Delete) => self.delete(),
            _ => return false,
        }
        true
    }

    /// Everything on the cursor's own line up to the cursor.
    ///
    /// What completion is about: the word being typed, which is on one line and
    /// ends where the cursor is. Reading the whole text instead is why
    /// completion used to switch itself off the moment a prompt had two lines.
    pub fn typing(&self) -> &str {
        &self.text[self.line_start()..self.cursor]
    }

    /// Replace the last `len` bytes before the cursor.
    pub fn splice_before_cursor(&mut self, len: usize, text: &str) {
        let start = self.cursor.saturating_sub(len);
        self.text.replace_range(start..self.cursor, text);
        self.cursor = start + text.len();
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// (row, column) of the cursor in character terms, for cursor placement.
    pub fn cursor_position(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        let row = before.matches('\n').count();
        let column =
            before.rfind('\n').map_or_else(|| before.chars().count(), |at| before[at + 1..].chars().count());
        (row, column)
    }

    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.history_pos = None;
        if !text.trim().is_empty() {
            if self.history.last() != Some(&text) {
                self.history.push(text.clone());
                self.trim_history();
            }
            if let Some(path) = &self.history_path {
                match persist_entry(path, &text) {
                    Ok(history) => self.history = history,
                    Err(error) => self.history_warning = Some(format!("cannot save input history: {error}")),
                }
            }
        }
        text
    }

    /// Replace the whole text (used by Tab command completion).
    pub fn replace(&mut self, text: &str) {
        self.text = text.to_owned();
        self.cursor = self.text.len();
        self.history_pos = None;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.history_pos = None;
    }

    pub fn insert(&mut self, character: char) {
        self.text.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    pub fn insert_str(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    pub fn backspace(&mut self) {
        if let Some(previous) = self.previous_boundary() {
            self.text.replace_range(previous..self.cursor, "");
            self.cursor = previous;
        }
    }

    pub fn delete(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.text.replace_range(self.cursor..next, "");
        }
    }

    pub fn left(&mut self) {
        if let Some(previous) = self.previous_boundary() {
            self.cursor = previous;
        }
    }

    pub fn right(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.cursor = next;
        }
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    pub fn kill_to_end(&mut self) {
        let end = self.line_end();
        self.text.replace_range(self.cursor..end, "");
    }

    pub fn kill_to_start(&mut self) {
        let start = self.line_start();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// To the start of this word, or of the one before if already there. A
    /// word is what an identifier is, underscores included.
    pub fn word_left(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end_matches(|character: char| !is_word(character));
        self.cursor = trimmed.rfind(|character: char| !is_word(character)).map_or(0, |at| at + 1);
    }

    /// To the end of this word, or of the next if already there.
    pub fn word_right(&mut self) {
        let after = &self.text[self.cursor..];
        let skipped = after.len() - after.trim_start_matches(|character: char| !is_word(character)).len();
        let word =
            after[skipped..].find(|character: char| !is_word(character)).unwrap_or(after.len() - skipped);
        self.cursor += skipped + word;
    }

    pub fn kill_word(&mut self) {
        let start = self.line_start();
        let before = &self.text[start..self.cursor];
        let trimmed = before.trim_end();
        let word_start =
            start + trimmed.rfind(|character: char| character.is_whitespace()).map_or(0, |at| at + 1);
        self.text.replace_range(word_start..self.cursor, "");
        self.cursor = word_start;
    }

    /// Move up a line; on the first line, park at its start, and only then
    /// recall older history.
    ///
    /// The middle step is what stops a half-written prompt being replaced by an
    /// old one on a keystroke meant to move within it — so it exists only where
    /// there are lines to move between. On a single line there is no repeat to
    /// make: Up is the history key there, and typing a prefix first and
    /// pressing it is the difference between history being usable and being
    /// there. It is skipped while history is already being walked too, because
    /// there the text is not the user's own.
    pub fn up(&mut self) {
        let (row, column) = self.cursor_position();
        if row > 0 {
            self.move_to(row - 1, column);
            return;
        }
        if column > 0 && self.text.contains('\n') && self.history_pos.is_none() {
            self.cursor = 0;
            return;
        }
        // Pick up prompts submitted by another Elmee process since this one
        // started. Refresh only when entering history, never while traversing
        // it, so the cursor cannot jump underneath the user.
        if self.history_pos.is_none()
            && let Some(path) = &self.history_path
            && let Ok(history) = load_history(path)
        {
            self.history = history;
        }
        if self.history_pos.is_none() {
            self.stash = self.text.clone();
            // A prefix nothing begins with would leave Up doing nothing, which
            // reads as broken. Narrow when there is something to narrow to,
            // and otherwise walk everything, as it always did.
            let typed = self.text.clone();
            let narrows = !typed.is_empty() && self.history.iter().any(|entry| entry.starts_with(&typed));
            self.history_prefix = if narrows { typed } else { String::new() };
        }
        let from = self.history_pos.unwrap_or(self.history.len());
        let Some(target) = self.matching(0, from).next_back() else {
            // Nothing older begins that way; the line stays as typed.
            return;
        };
        self.history_pos = Some(target);
        self.text = self.history[target].clone();
        self.cursor = self.text.len();
    }

    /// Move down a line; on the last line, park at its end, and only then
    /// recall newer history. The mirror of `up`, for the same reason.
    pub fn down(&mut self) {
        let (row, column) = self.cursor_position();
        if row < self.text.matches('\n').count() {
            self.move_to(row + 1, column);
            return;
        }
        if self.cursor < self.text.len() && self.text.contains('\n') && self.history_pos.is_none() {
            self.cursor = self.text.len();
            return;
        }
        let Some(position) = self.history_pos else { return };
        let next = self.matching(position + 1, self.history.len()).next();
        match next {
            Some(target) => {
                self.history_pos = Some(target);
                self.text = self.history[target].clone();
            }
            // Past the newest match is where the line was left.
            None => {
                self.history_pos = None;
                self.text = std::mem::take(&mut self.stash);
            }
        }
        self.cursor = self.text.len();
    }

    fn move_to(&mut self, row: usize, column: usize) {
        let line = self.text.split('\n').nth(row).unwrap_or_default();
        let line_offset = self.text.split_inclusive('\n').take(row).map(str::len).sum::<usize>();
        let column_offset: usize = line.chars().take(column).map(char::len_utf8).sum();
        self.cursor = line_offset + column_offset;
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |at| at + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..].find('\n').map_or(self.text.len(), |at| self.cursor + at)
    }

    fn previous_boundary(&self) -> Option<usize> {
        self.text[..self.cursor].char_indices().next_back().map(|(at, _)| at)
    }

    fn next_boundary(&self) -> Option<usize> {
        self.text[self.cursor..].chars().next().map(|character| self.cursor + character.len_utf8())
    }

    fn trim_history(&mut self) {
        let excess = self.history.len().saturating_sub(HISTORY_LIMIT);
        self.history.drain(..excess);
    }
}

fn is_word(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn load_history(path: &Path) -> io::Result<Vec<String>> {
    let contents = fs::read_to_string(path)?;
    let mut history: Vec<String> = contents
        .lines()
        // A torn or legacy line must not make all other history unavailable.
        .filter_map(|line| serde_json::from_str::<String>(line).ok())
        .filter(|entry| !entry.trim().is_empty())
        .collect();
    let excess = history.len().saturating_sub(HISTORY_LIMIT);
    history.drain(..excess);
    Ok(history)
}

fn persist_entry(path: &Path, entry: &str) -> io::Result<Vec<String>> {
    let parent = path.parent().ok_or_else(|| io::Error::other("history path has no parent"))?;
    fs::create_dir_all(parent)?;
    let _lock = HistoryLock::acquire(&path.with_extension("jsonl.lock"))?;

    let mut history = match load_history(path) {
        Ok(history) => history,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    if history.last().map(String::as_str) != Some(entry) {
        history.push(entry.to_owned());
    }
    let excess = history.len().saturating_sub(HISTORY_LIMIT);
    history.drain(..excess);

    // Owner-only: what a person typed at this prompt is as private as a shell
    // history, and the replacement must never widen that.
    let mut lines = Vec::new();
    for item in &history {
        serde_json::to_writer(&mut lines, item)?;
        lines.push(b'\n');
    }
    elmee_plugin_atomic::write(
        path,
        &lines,
        elmee_plugin_atomic::Options { mode: Some(0o600), ..Default::default() },
    )?;
    Ok(history)
}

struct HistoryLock(PathBuf);

impl HistoryLock {
    fn acquire(path: &Path) -> io::Result<Self> {
        for _ in 0..50 {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(path) {
                Ok(mut file) => {
                    if let Err(error) = writeln!(file, "{}", std::process::id()) {
                        drop(file);
                        let _ = fs::remove_file(path);
                        return Err(error);
                    }
                    return Ok(Self(path.to_owned()));
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > LOCK_STALE_AFTER);
                    if stale {
                        let _ = fs::remove_file(path);
                    } else {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(io::ErrorKind::WouldBlock, "input history is locked by another process"))
    }
}

impl Drop for HistoryLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn edits_multibyte_text_and_recalls_history() {
        let mut editor = Editor::default();
        for character in "héllo".chars() {
            editor.insert(character);
        }
        editor.backspace();
        editor.left();
        editor.left();
        editor.left();
        editor.delete(); // removes the multi-byte é without splitting it
        assert_eq!(editor.text(), "hll");
        assert_eq!(editor.take(), "hll");
        editor.up();
        assert_eq!(editor.text(), "hll");
        editor.down();
        assert_eq!(editor.text(), "");
    }

    /// What makes history usable: typing the start of a prompt and pressing
    /// Up walks only prompts that began that way, as a shell does. Walking a
    /// hundred entries to reach one is what it did before.
    #[test]
    fn up_walks_only_the_prompts_that_begin_the_way_the_line_does() {
        let mut editor = Editor::default();
        for entry in ["cargo test", "git status", "cargo build", "git log"] {
            editor.history.push(entry.to_owned());
        }
        editor.replace("cargo ");
        editor.up();
        assert_eq!(editor.text(), "cargo build", "the newest cargo prompt, not the newest prompt");
        editor.up();
        assert_eq!(editor.text(), "cargo test");
        editor.up();
        assert_eq!(editor.text(), "cargo test", "and no further: nothing older begins that way");
        editor.down();
        assert_eq!(editor.text(), "cargo build");
        editor.down();
        assert_eq!(editor.text(), "cargo ", "past the newest match is the line as it was typed");

        // A prefix nothing begins with walks everything rather than nothing,
        // because Up that does nothing reads as broken.
        editor.replace("zzz");
        editor.up();
        assert_eq!(editor.text(), "git log");
    }

    /// The search shows the candidates around the selected one, so the
    /// question it answers is "which one" and not merely "is there one".
    #[test]
    fn reverse_search_shows_the_candidates_around_the_selected_match() {
        let mut editor = Editor::default();
        for entry in ["cargo test", "cargo build --release", "git status", "cargo test -q"] {
            editor.history.push(entry.to_owned());
        }
        editor.search_step();
        for character in "cargo".chars() {
            editor.handle_key(KeyCode::Char(character), false, false);
        }
        let (query, matches, selected) = editor.search_matches(5).expect("a search");
        assert_eq!(query, "cargo");
        assert_eq!(matches, ["cargo test -q", "cargo build --release", "cargo test"]);
        assert_eq!(selected, 0, "newest first");
        editor.search_step();
        assert_eq!(editor.search_matches(5).unwrap().2, 1, "Up moves to the next older");
        editor.search_back();
        assert_eq!(editor.search_matches(5).unwrap().2, 0, "Down comes back");
    }

    #[test]
    fn reverse_search_walks_matches_and_accepts_one() {
        let mut editor = Editor::default();
        for prompt in ["fix the parser", "run the tests", "fix the renderer"] {
            editor.replace(prompt);
            editor.take();
        }
        editor.search_step(); // Ctrl+R opens the search
        editor.handle_key(KeyCode::Char('f'), false, false);
        editor.handle_key(KeyCode::Char('i'), false, false);
        assert_eq!(editor.search_state(), Some(("fi", "fix the renderer"))); // newest first
        editor.search_step();
        assert_eq!(editor.search_state(), Some(("fi", "fix the parser")));
        editor.search_step(); // no older match: the selection holds
        assert_eq!(editor.search_state(), Some(("fi", "fix the parser")));
        editor.search_accept();
        assert!(editor.search_state().is_none());
        assert_eq!(editor.text(), "fix the parser");
    }

    #[test]
    fn words_are_a_unit_for_the_keys_every_editor_shares() {
        let mut editor = Editor::default();
        editor.insert_str("fix the parser_module now");
        editor.handle_key(KeyCode::Left, false, true);
        assert_eq!(editor.cursor_position(), (0, 22), "Alt+Left lands at the start of the last word");
        editor.handle_key(KeyCode::Char('b'), false, true);
        assert_eq!(editor.cursor_position(), (0, 8), "and Alt+b at the one before, underscores included");
        editor.handle_key(KeyCode::Right, true, false);
        assert_eq!(editor.cursor_position(), (0, 21), "Ctrl+Right to the end of the word");
        editor.handle_key(KeyCode::Backspace, false, true);
        assert_eq!(editor.text(), "fix the  now", "Alt+Backspace takes the word");
    }

    #[test]
    fn navigates_between_lines() {
        let mut editor = Editor::default();
        editor.insert_str("first\nsecond");
        editor.home();
        assert_eq!(editor.cursor_position(), (1, 0));
        editor.up();
        assert_eq!(editor.cursor_position(), (0, 0));
        editor.end();
        assert_eq!(editor.cursor_position(), (0, 5));
    }

    /// Up and Down are the same key for two jobs, and the second must not be
    /// reachable by accident: a half-written prompt of several lines should not
    /// be replaced by an old one on a press meant to move inside it.
    #[test]
    fn up_walks_a_multi_line_draft_before_it_reaches_history() {
        let mut editor = Editor::default();
        editor.history.push("an older prompt".to_owned());
        editor.insert_str("first line\nsecond line\nthird line");
        assert_eq!(editor.cursor_position(), (2, 10));

        editor.up();
        assert_eq!(editor.cursor_position(), (1, 10), "up moves a line");
        editor.up();
        assert_eq!(editor.cursor_position(), (0, 10), "and another");
        editor.up();
        assert_eq!(editor.cursor_position(), (0, 0), "at the top it parks at the start");
        assert!(editor.text().starts_with("first line"), "with the draft still there");
        editor.up();
        assert_eq!(editor.text(), "an older prompt", "and only then is it history");

        // Down mirrors it, and while history is being walked neither parks:
        // the text is not the user's own, so a second press per entry is cost
        // with nothing bought.
        editor.down();
        assert_eq!(editor.text(), "first line\nsecond line\nthird line", "the draft comes back");
    }

    /// One line has no lines to move between, so Up is the history key — which
    /// is what makes typing a prefix and pressing it worth anything.
    #[test]
    fn on_a_single_line_up_is_still_the_history_key() {
        let mut editor = Editor::default();
        editor.history.push("cargo build".to_owned());
        editor.replace("cargo");
        editor.up();
        assert_eq!(editor.text(), "cargo build", "one press, not two");
    }

    #[test]
    fn history_survives_editors_and_restores_the_draft() {
        let root = std::env::temp_dir().join(format!(
            "elmee-input-history-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let path = root.join("history.jsonl");
        let mut first = Editor::with_history_path(path.clone());
        first.insert_str("first\nmultiline prompt");
        assert_eq!(first.take(), "first\nmultiline prompt");

        let mut second = Editor::with_history_path(path.clone());
        second.insert_str("unfinished draft");
        second.up();
        assert_eq!(second.text(), "first\nmultiline prompt");
        second.down();
        assert_eq!(second.text(), "unfinished draft");
        assert!(second.take_history_warning().is_none());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        fs::remove_dir_all(root).unwrap();
    }

    /// A rewrite replaces the file rather than editing it, so the owner-only
    /// mode has to be re-established every time and not just at creation.
    #[cfg(unix)]
    #[test]
    fn a_rewritten_history_file_stays_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("elmee-history-mode-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let path = root.join("history.jsonl");
        let mut editor = Editor::with_history_path(path.clone());
        editor.insert_str("first");
        editor.take();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        editor.insert_str("second");
        editor.take();
        assert_eq!(fs::read_to_string(&path).unwrap(), "\"first\"\n\"second\"\n");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_history_lines_are_ignored() {
        let root = std::env::temp_dir().join(format!("elmee-broken-history-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("history.jsonl");
        fs::write(&path, "not-json\n\"kept\"\n").unwrap();
        let mut editor = Editor::with_history_path(path);
        editor.up();
        assert_eq!(editor.text(), "kept");
        fs::remove_dir_all(root).unwrap();
    }
}
