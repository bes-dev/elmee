//! Durable notes: what the agent keeps about the person, and what it keeps
//! about this project.
//!
//! Two stores, because they answer different questions and go stale at
//! different rates. What someone prefers outlives any repository. What turned
//! out to be true about *this* code — which refactor was tried and failed,
//! where the API surprises you — is worth nothing in the next project and
//! everything in this one, and a repository does not record it: the code says
//! what it is, not what was learnt getting there.
//!
//! The project store is an index plus bodies, which is the shape skills
//! already use here. Only `MEMORY.md` rides every request, under a ceiling
//! small enough that it can; a line naming a file is opened with `read` when
//! its subject comes up. That way depth costs no machinery, and the hot part
//! cannot grow until it crowds out the conversation.
//!
//! It lives at `.elmee/memory/` inside the project, beside the run state and
//! the project's own commands, and `.gitignore` treats it as machine state by
//! default. A team that wants to share what a repository has taught can unignore
//! it; that is their call and not this module's.
//!
//! Every entry carries where it came from. A line the person said and a line
//! the agent inferred off a fetched web page are worth different things, and
//! once both are prose in one file nothing can tell them apart again — which is
//! how a page gets to write the agent's instructions. The tag is asserted by
//! the writer and so is not proof; what makes it worth recording is the
//! consequence attached to it elsewhere: something sourced from outside may be
//! shown to the agent, but must never become a rule that gates its work.

use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

use crate::{Error, Result};

/// Which store a note belongs in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// About the person: preferences, constraints, how they want to work.
    User,
    /// About this repository: what it turned out to be, and what it taught.
    Project,
}

/// Where a note came from. See the module note on why this is recorded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// The person said it.
    User,
    /// The agent concluded it from its own work. The default, because a tool
    /// call is the agent speaking unless it says otherwise.
    Agent,
    /// It came from outside — a fetched page, an MCP server, the output of
    /// something that ran. Never a rule; only ever something to show.
    External,
}

impl Scope {
    pub const NAMES: [&'static str; 2] = ["user", "project"];

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "user" | "global" => Some(Self::User),
            "project" | "repo" => Some(Self::Project),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }

    /// Roughly a thousand tokens for the person and five hundred for the
    /// project, on every request, forever. The ceiling is the point: it forces
    /// bulk into files that load on demand instead of letting the always-on
    /// part grow.
    pub fn limit(self) -> usize {
        match self {
            Self::User => 4096,
            Self::Project => 2048,
        }
    }

    fn path(self, cwd: &Path) -> Option<PathBuf> {
        match self {
            Self::User => std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".elmee/MEMORY.md")),
            Self::Project => Some(directory(cwd).join("MEMORY.md")),
        }
    }
}

impl Source {
    pub const NAMES: [&'static str; 3] = ["user", "agent", "external"];

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "user" | "person" => Some(Self::User),
            "agent" | "self" => Some(Self::Agent),
            "external" | "outside" => Some(Self::External),
            _ => None,
        }
    }

    pub fn tag(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::External => "external",
        }
    }
}

/// What is asked of a compaction beyond its summary.
///
/// Here rather than beside the compaction prompt because what makes a note
/// worth keeping is this module's question, not compaction's: compaction only
/// happens to be the moment with the whole conversation still in hand.
pub const ASK: &str = "After the summary, if this taught something durable about the project \
    itself — a convention nobody wrote down, an approach tried and abandoned and why, a place a \
    dependency behaves unexpectedly — add a <notes> block of at most three lines each starting \
    with '-'. Write what helps on a different task months from now, not what happened here: 'the \
    tests need DATABASE_URL set', not 'we fixed the failing test'. Omit the block when the answer \
    is nothing, which is usual.";

/// Split a compaction reply into the handoff summary and the durable notes it
/// offered.
///
/// The two are asked for in one request because the expensive part — sending
/// the whole conversation — is already being paid for. They must not be stored
/// as one thing, though: a handoff summary is about this piece of work and is
/// thrown away with it, while a note is meant to outlive the session that
/// found it.
pub fn split(reply: &str) -> (&str, &str) {
    match reply.split_once("<notes>") {
        Some((summary, rest)) => {
            (summary.trim_end(), rest.split_once("</notes>").map_or(rest, |(notes, _)| notes))
        }
        None => (reply, ""),
    }
}

/// Keep what a compaction found worth keeping.
///
/// Bounded hard, because this runs unattended in the middle of a turn with
/// nobody to ask: three notes is more than a stretch of work usually teaches,
/// and a store at its ceiling is a signal to consolidate rather than licence to
/// grow. A note already recorded is not recorded twice — the same compaction
/// runs again on a session that keeps going, and without this the store fills
/// with restatements of one fact.
///
/// Written as `agent`, never as `user`. What the model concluded from a
/// conversation is the model's conclusion, whoever it was talking to.
pub fn keep(cwd: &Path, notes: &str) -> usize {
    let mut kept = 0;
    for note in notes.lines().filter_map(|line| line.trim().strip_prefix('-')) {
        // The bound is on what is kept, not on what is looked at: a line too
        // short to be a note used to spend one of the three places.
        let note = note.trim();
        if kept == 3 {
            break;
        }
        if note.len() < 8 || load(Scope::Project, cwd).contains(note) {
            continue;
        }
        kept += usize::from(apply(Scope::Project, cwd, Source::Agent, note, None).is_ok());
    }
    kept
}

/// Which store and which origin a tool call named.
///
/// Here rather than at the dispatch, which is a registry of one line per tool:
/// what a name means belongs with the type that has the names. A name the model
/// invented is refused rather than quietly read as the default — `"source": "the
/// user told me"` must not become an agent note that later passes for one.
pub fn named(scope: Option<&str>, source: Option<&str>) -> Result<(Scope, Source)> {
    let unknown = |what: &str, given: &str| Error::Tool(format!("unknown memory {what} {given:?}"));
    Ok((
        scope.map_or(Ok(Scope::Project), |name| Scope::parse(name).ok_or_else(|| unknown("scope", name)))?,
        source
            .map_or(Ok(Source::Agent), |name| Source::parse(name).ok_or_else(|| unknown("source", name)))?,
    ))
}

/// Where a project's notes and their bodies live.
pub fn directory(cwd: &Path) -> PathBuf {
    cwd.join(".elmee/memory")
}

pub fn load(scope: Scope, cwd: &Path) -> String {
    scope.path(cwd).and_then(|path| fs::read_to_string(path).ok()).unwrap_or_default()
}

/// Apply one edit. `old` is a unique substring to replace or remove; without
/// it the text is appended.
///
/// The guard is against a failure other agents have shipped: a model that
/// decides the store is full when it is not, and rewrites it down to a
/// fraction of itself. A wholesale cull is refused unless the file really is
/// near its ceiling, and the previous contents are kept beside it either way.
///
/// Counted in entries rather than in characters, which is the unit both a
/// person and the model think in. Characters could not tell "consolidate forty
/// notes into three" from "delete the longer of two", and on the project index
/// — small on purpose — they blocked the second, which is an ordinary edit.
pub fn apply(scope: Scope, cwd: &Path, source: Source, text: &str, old: Option<&str>) -> Result<String> {
    let path = scope.path(cwd).ok_or_else(|| Error::Config("no HOME to store memory in".into()))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _lock = lock_beside(&path)?;
    let limit = scope.limit();
    let current = fs::read_to_string(&path).unwrap_or_default();
    // A removal is the one edit that must not be stamped: there is nothing
    // left to say where it came from.
    let entry = match text.trim().is_empty() {
        true => String::new(),
        false => format!("{} [{} {}]", text.trim_end(), source.tag(), today()),
    };
    let updated = match old {
        None if current.is_empty() => entry,
        None => format!("{current}\n{entry}"),
        Some(old) => match current.matches(old).count() {
            1 => current.replace(old, &entry),
            0 => return Err(Error::Tool(format!("no memory matches {old:?}"))),
            found => return Err(Error::Tool(format!("{old:?} matches {found} entries; be more specific"))),
        },
    };
    // Removing a note leaves the blank line it used to be on. Left in, they
    // accumulate: the store is written by replacement, so every removal costs
    // a line of the ceiling forever.
    let updated = updated.lines().filter(|line| !line.trim().is_empty()).collect::<Vec<_>>().join("\n");

    let entries = |text: &str| text.lines().filter(|line| !line.trim().is_empty()).count();
    let dropped = entries(&current).saturating_sub(entries(&updated));
    if dropped > 2 && current.len() * 5 < limit * 4 {
        return Err(Error::Tool(format!(
            "that edit would drop {dropped} notes from a store that is only {}% full — consolidate \
             only when it is actually near the {limit}-character limit",
            current.len() * 100 / limit
        )));
    }
    if updated.len() > limit {
        return Err(Error::Tool(format!(
            "memory would exceed {limit} characters. This store is an index: move a long note into \
             its own file under {} and leave one line here naming it. Current contents:\n{current}",
            directory(cwd).display()
        )));
    }
    if !current.is_empty() {
        let _ = fs::write(path.with_extension("md.bak"), &current);
    }
    elmee_plugin_atomic::write(&path, updated.as_bytes(), Default::default())?;
    Ok(format!("Memory updated ({}/{limit} characters)", updated.len()))
}

/// A memory edit is a read-modify-write and two agents can start one at once.
/// Both stores hold this sibling lock across the read and the replacement.
pub(crate) fn lock_beside(path: &Path) -> Result<File> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("md.lock"))?;
    lock.lock()?;
    Ok(lock)
}

fn today() -> String {
    let seconds = elmee_core::now();
    let days = (seconds / 86_400) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[derive(Clone)]
pub struct MemoryPlugin {
    cwd: PathBuf,
    definitions: Vec<serde_json::Value>,
}

impl MemoryPlugin {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            definitions: vec![serde_json::json!({
                "type": "function",
                "function": {
                    "name": "memory",
                    "description": "Remember something durable. Use project memory for repository facts and user memory for preferences that outlive a repository. Keep entries short; external content must retain external provenance.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "text": {"type":"string","description":"What to record, or the replacement text"},
                            "old_text": {"type":"string","description":"Unique existing text to replace; empty text removes it"},
                            "scope": {"type":"string","enum":["project","user"],"description":"Defaults to project"},
                            "source": {"type":"string","enum":["user","agent","external"],"description":"Where the fact came from; defaults to agent"}
                        },
                        "required": ["text"]
                    }
                }
            })],
        }
    }
}

impl elmee_core::Handler for MemoryPlugin {
    fn definitions(&self) -> &[serde_json::Value] {
        &self.definitions
    }

    fn execute<'a>(
        &'a mut self,
        call: &'a elmee_core::message::ToolCall,
    ) -> elmee_core::PluginFuture<'a, elmee_core::ToolOutcome> {
        #[derive(serde::Deserialize)]
        struct Args {
            text: String,
            old_text: Option<String>,
            scope: Option<String>,
            source: Option<String>,
        }
        Box::pin(async move {
            let result = serde_json::from_str::<Args>(&call.function.arguments)
                .map_err(Error::from)
                .and_then(|args| {
                    let (scope, source) = named(args.scope.as_deref(), args.source.as_deref())?;
                    apply(scope, &self.cwd, source, &args.text, args.old_text.as_deref())
                });
            match result {
                Ok(output) => elmee_core::ToolOutcome::new(output, true),
                Err(error) => elmee_core::ToolOutcome::new(format!("Error: {error}"), false),
            }
        })
    }
}

impl elmee_core::Middleware for MemoryPlugin {
    fn on<'a>(
        &'a mut self,
        event: elmee_core::MiddlewareEvent<'a>,
    ) -> elmee_core::PluginFuture<'a, Result<Vec<elmee_core::MiddlewareEffect>>> {
        Box::pin(async move {
            let effects = match event {
                elmee_core::MiddlewareEvent::BeforeCompaction { .. } => {
                    vec![elmee_core::MiddlewareEffect::AddContext(ASK.to_owned())]
                }
                elmee_core::MiddlewareEvent::Compacting { summary } => {
                    let (text, notes) = split(summary);
                    let kept = keep(&self.cwd, notes);
                    vec![elmee_core::MiddlewareEffect::ReplaceText { text: text.to_owned(), notes: kept }]
                }
                _ => Vec::new(),
            };
            Ok(effects)
        })
    }
}

impl elmee_core::Plugin for MemoryPlugin {
    fn id(&self) -> &'static str {
        "memory"
    }

    fn declare(self, plugins: &mut elmee_core::Registrar) -> Result<()> {
        plugins.provide_handler(self.clone())?;
        plugins.provide_middleware(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("elmee-memory-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    /// Where a note came from has to survive being written down, or the rule
    /// that a fetched page may not write the agent's instructions has nothing
    /// to stand on later.
    #[test]
    fn a_note_records_where_it_came_from() {
        let cwd = scratch("source");
        apply(Scope::Project, &cwd, Source::User, "prefer ripgrep here", None).unwrap();
        apply(Scope::Project, &cwd, Source::External, "the docs claim v2 is required", None).unwrap();
        let stored = load(Scope::Project, &cwd);

        assert!(stored.contains("prefer ripgrep here [user "), "{stored}");
        assert!(stored.contains("[external "), "{stored}");
        assert_eq!(stored.lines().count(), 2);
        // And it is in the project, not in HOME: the two stores are separate
        // files and one must not answer for the other.
        assert!(directory(&cwd).join("MEMORY.md").is_file());
        assert!(load(Scope::User, &cwd) != stored || stored.is_empty());

        // Removal takes the stamp with it rather than leaving a dated husk.
        apply(Scope::Project, &cwd, Source::Agent, "", Some("prefer ripgrep here")).unwrap();
        let stored = load(Scope::Project, &cwd);
        assert!(!stored.contains("ripgrep"), "{stored}");
        assert!(stored.contains("[external "), "the other note is untouched: {stored}");
        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn the_memory_tool_writes_its_own_store_with_provenance() {
        use elmee_core::Handler;

        let cwd = scratch("tool");
        let mut plugin = MemoryPlugin::new(&cwd);
        let call = |id: &str, args: &str| elmee_core::message::ToolCall::new(id, "memory", args.to_owned());
        let written =
            plugin.execute(&call("0", r#"{"text":"the loop budget is strict","source":"user"}"#)).await;
        assert!(written.ok, "{}", written.output);
        let stored = load(Scope::Project, &cwd);
        assert!(stored.contains("the loop budget is strict [user "), "{stored}");

        let invented = plugin.execute(&call("1", r#"{"text":"x","source":"the user told me"}"#)).await;
        assert!(!invented.ok && invented.output.contains("unknown memory source"), "{}", invented.output);
        let _ = fs::remove_dir_all(cwd);
    }

    /// The project store is an index, and the ceiling is what keeps it one.
    /// A refusal that does not say where the body should go instead is a
    /// refusal the model can only answer by deleting something.
    #[test]
    fn the_index_ceiling_points_at_the_way_out() {
        let cwd = scratch("ceiling");
        let long = "x".repeat(Scope::Project.limit() + 1);
        let error = apply(Scope::Project, &cwd, Source::Agent, &long, None).unwrap_err().to_string();
        assert!(error.contains("index"), "{error}");
        assert!(error.contains(".elmee/memory"), "{error}");
        let _ = fs::remove_dir_all(cwd);
    }

    /// The failure this reproduces: the model announces "memory is full" at a
    /// fraction of capacity and replaces everything with one line. The edit
    /// that must survive it is the ordinary one — taking out a single note.
    /// Compaction is where a session's knowledge dies, and what is taken out
    /// of it first has to be bounded: it runs mid-turn with nobody to ask.
    #[test]
    fn what_compaction_saves_is_bounded_deduplicated_and_refusable() {
        let cwd = scratch("compact");
        let reply = "a handoff summary\n<notes>\n- the tests need DATABASE_URL set\n- no\n\
                     - the build script regenerates src/gen\n- a fourth thing worth knowing\n\
                     - a fifth thing worth knowing\n</notes>";
        let (summary, notes) = split(reply);
        assert_eq!(summary, "a handoff summary");
        assert!(notes.contains("DATABASE_URL"));

        // At most three, and a line too short to be a note is not one.
        assert_eq!(keep(&cwd, notes), 3);
        let stored = load(Scope::Project, &cwd);
        assert!(stored.contains("DATABASE_URL") && !stored.contains("- no"), "{stored}");
        // Compaction runs again on a session that keeps going. The same notes
        // must not pile up — only the one that did not fit last time is new,
        // and after that there is nothing left to take.
        assert_eq!(keep(&cwd, notes), 1);
        assert_eq!(keep(&cwd, notes), 0);

        // A reply with no block at all is all summary.
        assert_eq!(split("just a summary"), ("just a summary", ""));
        let _ = fs::remove_dir_all(cwd);
    }

    #[test]
    fn a_wholesale_cull_of_a_nearly_empty_store_is_refused() {
        let cwd = scratch("shrink");
        for index in 0..6 {
            apply(Scope::Project, &cwd, Source::Agent, &format!("note {index}"), None).unwrap();
        }
        let all = load(Scope::Project, &cwd);
        let error = apply(Scope::Project, &cwd, Source::Agent, "tiny", Some(&all)).unwrap_err();
        assert!(error.to_string().contains("would drop 5 notes"), "{error}");

        // ... while removing one of them is not a cull, and used to be refused
        // on a store this small because the guard counted characters.
        apply(Scope::Project, &cwd, Source::Agent, "", Some("note 3")).unwrap();
        assert!(!load(Scope::Project, &cwd).contains("note 3"));
        let _ = fs::remove_dir_all(cwd);
    }
}
