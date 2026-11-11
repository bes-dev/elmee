//! Terminal color palette, overridable per user and per project:
//! `~/.elmee/theme.json` then `.elmee/theme.json` (the project wins) map role
//! names to colors — colour names ("cyan", "light blue"), "#rrggbb", or
//! 256-color indexes. Roles and where they appear: accent (prompt, selection,
//! inline code, whatever is working) · muted (secondary text) · border (the
//! input's rules, table rules) · warning (your own shell lines, numbers,
//! list marks) · success (ok marks, strings, added lines) ·
//! error · keyword (syntax keywords, peer badges) · agent (the dot a reply
//! hangs from; the terminal's own text colour by default) · user_bg (the
//! background behind what the person said; "reset" for none).

use std::{collections::HashMap, fs, path::Path, sync::OnceLock};

use super::text::Color;

pub struct Theme {
    pub accent: Color,
    pub muted: Color,
    pub border: Color,
    pub warning: Color,
    pub success: Color,
    pub error: Color,
    pub keyword: Color,
    pub agent: Color,
    pub user_bg: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            accent: Color::Cyan,
            muted: Color::DarkGray,
            border: Color::DarkGray,
            warning: Color::Yellow,
            success: Color::Green,
            error: Color::Red,
            keyword: Color::Magenta,
            agent: Color::Reset,
            user_bg: Color::DarkGray,
        }
    }
}

static THEME: OnceLock<Theme> = OnceLock::new();

/// Resolve and pin the palette; later calls keep the first resolution.
pub fn init(cwd: &Path) {
    let _ = THEME.set(load(cwd));
}

/// The active palette (defaults when `init` was never called, e.g. in tests).
pub fn t() -> &'static Theme {
    THEME.get_or_init(Theme::default)
}

fn load(cwd: &Path) -> Theme {
    let mut theme = Theme::default();
    let mut paths = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(std::path::PathBuf::from(home).join(".elmee/theme.json"));
    }
    paths.push(cwd.join(".elmee/theme.json")); // loaded last: the project wins
    for path in paths {
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&text) else {
            eprintln!("elmee: ignoring malformed theme {}", path.display());
            continue;
        };
        for (role, value) in map {
            let Ok(color) = value.parse::<Color>() else {
                eprintln!("elmee: theme {}: unknown color {value:?}", path.display());
                continue;
            };
            match role.as_str() {
                "accent" => theme.accent = color,
                "muted" => theme.muted = color,
                "border" => theme.border = color,
                "warning" => theme.warning = color,
                "success" => theme.success = color,
                "error" => theme.error = color,
                "keyword" => theme.keyword = color,
                "agent" => theme.agent = color,
                "user_bg" => theme.user_bg = color,
                other => eprintln!("elmee: theme {}: unknown role {other:?}", path.display()),
            }
        }
    }
    theme
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_role_overrides_in_all_color_forms() {
        let root = std::env::temp_dir().join(format!("elmee-theme-test-{}", std::process::id()));
        fs::create_dir_all(root.join(".elmee")).unwrap();
        fs::write(
            root.join(".elmee/theme.json"),
            r##"{"accent":"#ff8800","muted":"244","border":"light blue","bogus":"red","error":"nope"}"##,
        )
        .unwrap();
        let theme = load(&root);
        assert_eq!(theme.accent, Color::Rgb(255, 136, 0));
        assert_eq!(theme.muted, Color::Indexed(244));
        assert_eq!(theme.border, Color::LightBlue);
        assert_eq!(theme.error, Color::Red); // unparsable value keeps the default
        fs::remove_dir_all(root).unwrap();
    }
}
