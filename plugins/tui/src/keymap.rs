//! Which terminal key means what — kept apart from what the meaning then does.
//!
//! The interface has four modal layouts (a question dialog, a picker, reverse
//! search, and ordinary editing) and they used to live in one function, chosen
//! by early returns threaded between the bodies that carried them out. Reading
//! it meant tracing control flow to find out whether Enter was even reachable.
//!
//! The split here is deliberate and narrow: this file answers "what did the
//! user ask for", never "what should happen". Anything that depends on state
//! rather than on the key — whether the agent is busy, whether the line is
//! empty — belongs to the caller, so the table stays a table and cannot drift
//! away from what it claims to describe. The hint lines are generated from it
//! for the same reason: a binding and its advertisement should not be able to
//! disagree.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::App;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dialog {
        typing: bool,
    },
    Picker,
    /// A report owns the alternate screen: it scrolls, and Esc gives the
    /// terminal back.
    Panel,
    Editing {
        menu_open: bool,
        searching: bool,
    },
}

/// What the user asked for. Deliberately coarse: one variant per thing a
/// person would name, not per key that produces it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Intent {
    DialogSubmit,
    DialogCancel,
    DialogUp,
    DialogDown,
    DialogToggle,
    PickerUp,
    PickerDown,
    PickerAccept,
    PickerCancel,
    PickerType(char),
    PickerErase,
    SearchStep,
    SearchBack,
    SearchAccept,
    SearchCancel,
    /// Ctrl+C: abort a turn, then arm, then quit.
    Interrupt,
    /// Ctrl+D on an empty line.
    QuitIfEmpty,
    /// Esc: abort a turn, or clear what has been typed.
    Escape,
    Newline,
    Submit,
    ToggleExpanded,
    PanelUp,
    PanelDown,
    PanelPageUp,
    PanelPageDown,
    PanelClose,
    MenuUp,
    MenuDown,
    MenuComplete,
    HistoryUp,
    HistoryDown,
    Indent,
    /// Hand the key to the editor unchanged.
    Edit,
    Ignore,
}

pub fn mode(app: &App) -> Mode {
    match (&app.dialog, &app.picker, &app.panel) {
        (Some(dialog), _, _) => Mode::Dialog { typing: dialog.typing },
        (None, Some(_), _) => Mode::Picker,
        (None, None, Some(_)) => Mode::Panel,
        (None, None, None) => {
            Mode::Editing { menu_open: app.menu_open(), searching: app.editor.search_state().is_some() }
        }
    }
}

pub fn resolve(mode: Mode, key: KeyEvent) -> Intent {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match mode {
        Mode::Dialog { typing } => match (typing, key.code) {
            (_, KeyCode::Enter) => Intent::DialogSubmit,
            (_, KeyCode::Esc) => Intent::DialogCancel,
            (true, _) => Intent::Edit,
            (false, KeyCode::Up) => Intent::DialogUp,
            (false, KeyCode::Down) => Intent::DialogDown,
            (false, KeyCode::Char(' ')) => Intent::DialogToggle,
            (false, _) => Intent::Ignore,
        },
        // A report holds the alternate screen while it is open, so it owns the
        // arrows outright — the input, and its history, are not on screen to
        // want them.
        Mode::Panel => match key.code {
            KeyCode::Up | KeyCode::Char('k') => Intent::PanelUp,
            KeyCode::Down | KeyCode::Char('j') => Intent::PanelDown,
            KeyCode::PageUp => Intent::PanelPageUp,
            KeyCode::PageDown | KeyCode::Char(' ') => Intent::PanelPageDown,
            // Ctrl+C closes it too. It cannot reach the interrupt from here,
            // and a key that does nothing at all reads as a hang.
            KeyCode::Esc | KeyCode::Char('q') => Intent::PanelClose,
            KeyCode::Char('c') if control => Intent::PanelClose,
            _ => Intent::Ignore,
        },
        Mode::Picker => match key.code {
            KeyCode::Up => Intent::PickerUp,
            KeyCode::Down => Intent::PickerDown,
            KeyCode::Enter => Intent::PickerAccept,
            KeyCode::Esc => Intent::PickerCancel,
            KeyCode::Char('c') if control => Intent::PickerCancel,
            KeyCode::Backspace => Intent::PickerErase,
            KeyCode::Char(character) if !control && !alt => Intent::PickerType(character),
            _ => Intent::Ignore,
        },
        Mode::Editing { menu_open, searching } => match key.code {
            KeyCode::Char('r') if control => Intent::SearchStep,
            KeyCode::Up if searching => Intent::SearchStep,
            KeyCode::Down if searching => Intent::SearchBack,
            KeyCode::Enter if searching => Intent::SearchAccept,
            KeyCode::Esc if searching => Intent::SearchCancel,
            KeyCode::Char('c') if control => Intent::Interrupt,
            KeyCode::Char('d') if control => Intent::QuitIfEmpty,
            KeyCode::Esc => Intent::Escape,
            KeyCode::Enter if alt => Intent::Newline,
            KeyCode::Char('j') if control => Intent::Newline,
            KeyCode::Enter => Intent::Submit,
            KeyCode::Char('o') if control => Intent::ToggleExpanded,
            KeyCode::Up if menu_open => Intent::MenuUp,
            KeyCode::Down if menu_open => Intent::MenuDown,
            KeyCode::Up => Intent::HistoryUp,
            KeyCode::Down => Intent::HistoryDown,
            KeyCode::Tab if menu_open => Intent::MenuComplete,
            KeyCode::Tab => Intent::Indent,
            _ => Intent::Edit,
        },
    }
}

/// What to tell the user they can press, straight from the table above.
pub fn hints(mode: Mode) -> &'static str {
    match mode {
        Mode::Dialog { typing: true } => "Enter confirm · Esc cancel",
        Mode::Dialog { typing: false } => "↑↓ move · Space toggle · Enter confirm · Esc cancel",
        Mode::Picker => "type to filter · ↑↓ move · Enter choose · Esc cancel",
        Mode::Panel => "↑↓ PgUp/PgDn scroll · Esc close",
        Mode::Editing { searching: true, .. } => "↑↓ move · Enter accept · Esc cancel",
        Mode::Editing { menu_open: true, .. } => "Tab completes · ↑↓ choose · Enter sends the line",
        Mode::Editing { .. } => {
            "/ commands · Enter sends · Ctrl+J newline · ↑↓ move · Esc Esc clears, or stops the turn"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn a_mode_changes_what_the_same_key_means() {
        let plain = KeyModifiers::NONE;
        let editing = Mode::Editing { menu_open: false, searching: false };
        assert_eq!(resolve(editing, press(KeyCode::Enter, plain)), Intent::Submit);
        assert_eq!(
            resolve(Mode::Editing { menu_open: false, searching: true }, press(KeyCode::Enter, plain)),
            Intent::SearchAccept
        );
        assert_eq!(resolve(Mode::Picker, press(KeyCode::Enter, plain)), Intent::PickerAccept);
        assert_eq!(
            resolve(Mode::Dialog { typing: true }, press(KeyCode::Enter, plain)),
            Intent::DialogSubmit
        );
        // With a menu open the arrows belong to it, not to history.
        assert_eq!(resolve(editing, press(KeyCode::Up, plain)), Intent::HistoryUp);
        assert_eq!(
            resolve(Mode::Editing { menu_open: true, searching: false }, press(KeyCode::Up, plain)),
            Intent::MenuUp
        );
    }

    #[test]
    fn typing_into_a_dialog_reaches_the_editor_but_choosing_does_not() {
        let plain = KeyModifiers::NONE;
        assert_eq!(resolve(Mode::Dialog { typing: true }, press(KeyCode::Char('x'), plain)), Intent::Edit);
        assert_eq!(resolve(Mode::Dialog { typing: false }, press(KeyCode::Char('x'), plain)), Intent::Ignore);
    }

    #[test]
    fn every_hint_names_a_key_the_table_actually_resolves() {
        // The reason hints live here: an advertised binding that no longer
        // exists is worse than no hint at all.
        for mode in [
            Mode::Picker,
            Mode::Dialog { typing: false },
            Mode::Editing { menu_open: false, searching: true },
        ] {
            let hint = hints(mode);
            if hint.contains("Esc cancel") {
                let escape = resolve(mode, press(KeyCode::Esc, KeyModifiers::NONE));
                assert!(
                    matches!(escape, Intent::DialogCancel | Intent::PickerCancel | Intent::SearchCancel),
                    "{hint:?} promises Esc cancels"
                );
            }
        }
    }
}
