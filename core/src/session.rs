use std::{collections::HashMap, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    Error, Result,
    message::{Message, Role},
};

pub const SESSION_SCHEMA: u32 = 1;

const fn session_schema() -> u32 {
    SESSION_SCHEMA
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    #[serde(default = "session_schema")]
    pub schema: u32,
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub compacted: bool,
    /// On a compaction entry: how many entries before it stay verbatim. A
    /// summary loses the exact wording of the most recent turns, which is
    /// precisely what the next turn needs.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tail: usize,
    pub message: Message,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

pub struct Session {
    store: Box<dyn SessionStore>,
    entries: Vec<Entry>,
    active: Option<u64>,
    messages: Vec<Message>,
    /// The state of the working tree recorded before a prompt's turn wrote
    /// anything, by the id of that prompt.
    checkpoints: HashMap<u64, String>,
    /// Entries up to this id show their tool output shortened. Old output is
    /// most of a long context and the least of what it means: the call that
    /// produced it is still there to be made again.
    elided: u64,
    title: Option<String>,
}

/// What is left of an old tool result: enough to see what it was.
const ELIDED_CHARS: usize = 400;
/// What a picture is counted as. Its bytes say nothing about its cost; a
/// model charges on the order of a thousand tokens for one.
const IMAGE_CHARS: usize = 6_000;

/// How much of the context one message takes — including thinking blocks,
/// which are replayed and can be large, so leaving them out would delay
/// making room exactly when the model is reasoning hardest.
fn weight(message: &Message) -> usize {
    message.content.as_ref().map_or(0, String::len)
        + message.tool_calls.iter().map(|call| call.function.arguments.len()).sum::<usize>()
        + message.thinking.iter().map(|block| block.to_string().len()).sum::<usize>()
        + message.images.len() * IMAGE_CHARS
}

/// An entry as the model is shown it.
fn shown(entry: &Entry, elided: u64) -> Message {
    let mut message = entry.message.clone();
    if entry.id > elided {
        return message;
    }
    if matches!(message.role, Role::Tool)
        && let Some(content) = &mut message.content
        && content.len() > 2 * ELIDED_CHARS
    {
        *content = crate::text::middle(content, ELIDED_CHARS);
    }
    if !message.images.is_empty() {
        let note = format!("\n[{} picture(s) shown earlier are no longer attached]", message.images.len());
        message.content.get_or_insert_default().push_str(&note);
        message.images.clear();
    }
    message
}

/// Something a person asked on the active branch, as a place to go back to.
#[derive(Debug, Clone, PartialEq)]
pub struct Prompt {
    pub id: u64,
    pub text: String,
    /// What the working tree was before this prompt's turn changed it, where
    /// that was recorded.
    pub checkpoint: Option<String>,
}

/// One line of conversation in the tree, named by where it ends.
#[derive(Debug, Clone, PartialEq)]
pub struct Branch {
    pub leaf: u64,
    /// How many things were asked along it.
    pub prompts: usize,
    /// The last thing asked on it, which is what tells two branches apart.
    pub last: String,
    /// Whether this is where the conversation stands now.
    pub active: bool,
}

/// One line of a session's journal: something said, the active branch moving
/// without anything being said (`/clear`, `/rewind`, `/tree`), or the state of
/// the working tree recorded before a prompt's turn. All live in the one
/// journal, in order, so replaying it is the whole of opening a session and
/// there is no second file to disagree with the first.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Record {
    Entry(Entry),
    Head {
        // Required, null included: a line that merely lacks it is not this.
        #[serde(deserialize_with = "Option::deserialize")]
        head: Option<u64>,
    },
    Checkpoint {
        checkpoint: String,
        entry: u64,
    },
    /// Tool output in entries up to this one is shown shortened from now on.
    Elide {
        elide: u64,
    },
    /// What a person named the conversation, in place of its first prompt.
    Title {
        title: String,
    },
}

/// Persistence boundary for a conversation tree.
///
/// Core owns the records and their invariants; plugins decide where and how
/// they are loaded and appended. A store is deliberately append-only because
/// branching and compaction preserve history instead of rewriting it.
pub trait SessionStore: Send {
    fn path(&self) -> Option<&Path> {
        None
    }

    fn load(&mut self) -> Result<Vec<Record>>;

    fn append(&mut self, record: &Record) -> Result<()>;
}

struct MemorySessionStore;

impl SessionStore for MemorySessionStore {
    fn load(&mut self) -> Result<Vec<Record>> {
        Ok(Vec::new())
    }

    fn append(&mut self, _: &Record) -> Result<()> {
        Ok(())
    }
}

impl Session {
    pub fn memory() -> Self {
        Self::with_store(MemorySessionStore).expect("the memory session store is infallible")
    }

    pub fn with_store(store: impl SessionStore + 'static) -> Result<Self> {
        let mut store: Box<dyn SessionStore> = Box::new(store);
        let mut entries = Vec::new();
        let mut active = None;
        let mut checkpoints = HashMap::new();
        let mut elided = 0;
        let mut title = None;
        for record in store.load()? {
            match record {
                Record::Entry(entry) => {
                    active = Some(entry.id);
                    entries.push(entry);
                }
                Record::Head { head } => active = head,
                Record::Checkpoint { checkpoint, entry } => {
                    checkpoints.insert(entry, checkpoint);
                }
                Record::Elide { elide } => elided = elided.max(elide),
                Record::Title { title: named } => title = Some(named),
            }
        }
        let mut session = Self { store, entries, active, messages: Vec::new(), checkpoints, elided, title };
        session.rebuild_context()?;
        Ok(session)
    }

    pub fn path(&self) -> Option<&Path> {
        self.store.path()
    }

    /// The name a person gave this conversation, if they gave one.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn set_title(&mut self, title: String) -> Result<()> {
        self.store.append(&Record::Title { title: title.clone() })?;
        self.title = Some(title);
        Ok(())
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Immutable persisted records for storage adapters and export tooling.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// How much of the context the conversation takes, in characters.
    pub fn context_chars(&self) -> usize {
        self.messages.iter().map(weight).sum()
    }

    pub fn contains_event(&self, id: &str) -> bool {
        self.entries.iter().any(|entry| entry.message.event_id.as_deref() == Some(id))
    }

    pub fn push(&mut self, message: Message) -> Result<u64> {
        self.append(message, None)
    }

    /// Replace the context with a summary, keeping the newest `tail` entries
    /// verbatim after it.
    pub fn compact(&mut self, summary: String, tail: usize) -> Result<u64> {
        let message = Message::system(format!("# Conversation summary\n\n{summary}"));
        self.append(message, Some(tail))
    }

    /// How many trailing entries make up the newest `keep_chars` of the
    /// conversation: walk back accumulating, then step off a tool result,
    /// whose call would otherwise be left without an answer. Stops at a
    /// summary, so these are always the last messages of the context too.
    pub fn tail_length(&self, keep_chars: usize) -> Result<usize> {
        let path = self.branch()?;
        let mut chars = 0;
        let mut tail = 0;
        for entry in path.iter().rev() {
            if chars >= keep_chars || matches!(entry.message.role, Role::System) {
                break;
            }
            chars += weight(&entry.message);
            tail += 1;
        }
        while tail > 0 && matches!(path[path.len() - tail].message.role, Role::Tool) {
            tail -= 1;
        }
        Ok(tail)
    }

    /// Show the tool output older than the newest `keep_chars` shortened, and
    /// say how many characters that freed. Nothing is lost from the journal,
    /// and the calls themselves stay in view: what was run, and that it was.
    pub fn elide(&mut self, keep_chars: usize) -> Result<usize> {
        let path = self.branch()?;
        let kept = self.tail_length(keep_chars)?;
        let Some(through) = path[..path.len() - kept].last().map(|entry| entry.id) else { return Ok(0) };
        if through <= self.elided {
            return Ok(0);
        }
        let before = self.context_chars();
        self.store.append(&Record::Elide { elide: through })?;
        self.elided = through;
        self.rebuild_context()?;
        Ok(before.saturating_sub(self.context_chars()))
    }

    /// Start a fresh branch from the root: context empties, the tree and the
    /// JSONL file keep the full history.
    pub fn reset(&mut self) -> Result<()> {
        self.store.append(&Record::Head { head: None })?;
        self.active = None;
        self.messages.clear();
        Ok(())
    }

    /// What a person asked on the active branch, oldest first.
    pub fn prompts(&self) -> Vec<Prompt> {
        self.branch()
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| asked(entry))
            .map(|entry| self.prompt(entry))
            .collect()
    }

    fn prompt(&self, entry: &Entry) -> Prompt {
        Prompt {
            id: entry.id,
            text: entry.message.content.clone().unwrap_or_default(),
            checkpoint: self.checkpoints.get(&entry.id).cloned(),
        }
    }

    /// Remember the state the working tree was in before the turn of the
    /// newest prompt wrote to it. One per prompt: a later record for the same
    /// turn would describe a tree the turn had already changed.
    pub fn checkpoint(&mut self, checkpoint: String) -> Result<()> {
        let Some(entry) = self.branch()?.into_iter().rev().find(|entry| asked(entry)).map(|entry| entry.id)
        else {
            return Ok(());
        };
        if self.checkpoints.contains_key(&entry) {
            return Ok(());
        }
        self.store.append(&Record::Checkpoint { checkpoint: checkpoint.clone(), entry })?;
        self.checkpoints.insert(entry, checkpoint);
        Ok(())
    }

    /// Go back to just before a prompt on the active branch, and return it
    /// for editing. What followed stays in the tree as a branch of its own.
    pub fn rewind(&mut self, id: u64) -> Result<Prompt> {
        let entry = self
            .branch()?
            .into_iter()
            .find(|entry| entry.id == id && asked(entry))
            .ok_or_else(|| Error::Config(format!("no prompt {id} on this branch")))?;
        let (prompt, parent) = (self.prompt(entry), entry.parent);
        match parent {
            Some(parent) => self.checkout(parent)?,
            None => self.reset()?,
        }
        Ok(prompt)
    }

    /// Every line of conversation in the tree, newest first: one for each
    /// entry nothing was ever said after.
    pub fn branches(&self) -> Vec<Branch> {
        let by_id: HashMap<_, _> = self.entries.iter().map(|entry| (entry.id, entry)).collect();
        let parents: std::collections::HashSet<_> =
            self.entries.iter().filter_map(|entry| entry.parent).collect();
        let mut branches: Vec<Branch> = self
            .entries
            .iter()
            .rev()
            .filter(|entry| !parents.contains(&entry.id))
            .map(|leaf| {
                let asked: Vec<_> =
                    std::iter::successors(Some(leaf), |entry| by_id.get(&entry.parent?).copied())
                        .filter(|entry| asked(entry))
                        .collect();
                Branch {
                    leaf: leaf.id,
                    prompts: asked.len(),
                    last: asked.first().and_then(|entry| entry.message.content.clone()).unwrap_or_default(),
                    active: self.active == Some(leaf.id),
                }
            })
            .collect();
        // A branch nothing was asked on is the residue of a rewind to the
        // very start, not a conversation anyone would go back to.
        branches.retain(|branch| branch.prompts > 0);
        branches
    }

    pub fn checkout(&mut self, id: u64) -> Result<()> {
        if !self.entries.iter().any(|entry| entry.id == id) {
            return Err(Error::Config(format!("session entry {id} does not exist")));
        }
        let previous = self.active;
        self.active = Some(id);
        let result =
            self.rebuild_context().and_then(|()| self.store.append(&Record::Head { head: Some(id) }));
        if result.is_err() {
            self.active = previous;
            self.rebuild_context()?;
        }
        result
    }

    /// `tail` is `Some` for a compaction entry, carrying how many preceding
    /// entries survive it verbatim.
    fn append(&mut self, message: Message, tail: Option<usize>) -> Result<u64> {
        let id = self.entries.last().map_or(1, |entry| entry.id + 1);
        let compacted = tail.is_some();
        let record = Record::Entry(Entry {
            schema: SESSION_SCHEMA,
            id,
            parent: self.active,
            compacted,
            tail: tail.unwrap_or_default(),
            message,
        });
        self.store.append(&record)?;
        let Record::Entry(entry) = record else { unreachable!("built as an entry above") };
        self.active = Some(id);
        if compacted {
            self.entries.push(entry);
            self.rebuild_context()?;
        } else {
            self.messages.push(entry.message.clone());
            self.entries.push(entry);
        }
        Ok(id)
    }

    /// Tool-call ids in the newest assistant message that have no tool result yet,
    /// as left behind by an aborted or crashed model/tool cycle.
    pub fn dangling_tool_calls(&self) -> Vec<String> {
        let mut pending = Vec::new();
        for message in &self.messages {
            match message.role {
                Role::Assistant => {
                    pending = message.tool_calls.iter().map(|call| call.id.clone()).collect();
                }
                Role::Tool => {
                    if let Some(id) = &message.tool_call_id {
                        pending.retain(|pending_id| pending_id != id);
                    }
                }
                Role::System | Role::User => pending.clear(),
            }
        }
        pending
    }

    /// The active branch, oldest first.
    fn branch(&self) -> Result<Vec<&Entry>> {
        let by_id: HashMap<_, _> = self.entries.iter().map(|entry| (entry.id, entry)).collect();
        let mut path = Vec::new();
        let mut cursor = self.active;
        while let Some(id) = cursor {
            let entry =
                by_id.get(&id).ok_or_else(|| Error::Config(format!("session parent {id} is missing")))?;
            path.push(*entry);
            cursor = entry.parent;
        }
        path.reverse();
        Ok(path)
    }

    fn rebuild_context(&mut self) -> Result<()> {
        let path = self.branch()?;
        let mut messages = Vec::new();
        if let Some(cut) = path.iter().rposition(|entry| entry.compacted) {
            if let Some(system) = path[..cut].iter().find(|entry| matches!(entry.message.role, Role::System))
            {
                messages.push(system.message.clone());
            }
            let shown = |entry: &&Entry| shown(entry, self.elided);
            // The summary first, then the verbatim tail: a model reading this
            // expects the compacted history to precede what survived it.
            messages.push(path[cut].message.clone());
            let tail = path[cut].tail.min(cut);
            messages.extend(path[cut - tail..cut].iter().map(shown));
            messages.extend(path[cut + 1..].iter().map(shown));
        } else {
            messages.extend(path.iter().map(|entry| shown(entry, self.elided)));
        }
        self.messages = messages;
        Ok(())
    }
}

/// Whether an entry is something a person typed, rather than what the harness
/// said in the user's role.
fn asked(entry: &Entry) -> bool {
    matches!(entry.message.role, Role::User) && entry.message.origin == crate::message::MessageOrigin::User
}

#[cfg(test)]
mod tests;
