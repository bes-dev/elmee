//! Terminal frontend state: the transcript, editor, scroll position, and the
//! busy/queue machinery. Pure state transitions live here so they stay
//! testable without a terminal.

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use elmee_core::{
    AgentEvent,
    message::{Message, MessageOrigin, Role},
};

use super::commands::Command;
use super::dialog::Dialog;
use super::input::Editor;
use super::worker::{Cmd, Out};

use elmee_host::Participant;

/// Tool outputs longer than this collapse to a preview; Ctrl+O expands them.
pub const TOOL_PREVIEW_LINES: usize = 4;

pub enum Cell {
    /// A line the person ran themselves. Kept apart from a tool call because
    /// it is: nobody asked the model, and the model is never told.
    Shell {
        command: String,
        output: Option<String>,
        ok: bool,
    },
    User(String),
    /// The model working. Watched while it happens and folded to one line
    /// afterwards: during is when it is worth reading, and after is when it is
    /// in the way of the answer.
    Reasoning {
        text: String,
        done: bool,
    },
    /// A reply, and how much of it has already been printed as history.
    ///
    /// A long answer used to sit entirely in the live area until it finished,
    /// so it was redrawn as a scrolling tail and could not be read from the top
    /// until the model stopped writing. Every line that is complete is printed
    /// above instead, exactly as an ordinary program prints, and only the
    /// unfinished last line stays live.
    Assistant {
        text: String,
        done: bool,
        flushed: usize,
    },
    Tool {
        name: String,
        arguments: String,
        output: Option<String>,
        ok: bool,
    },
    Info(String),
    Error(String),
}

impl Cell {
    /// A finalized cell will not change again and can move to the scrollback.
    pub fn finalized(&self) -> bool {
        match self {
            Cell::Assistant { done, .. } | Cell::Reasoning { done, .. } => *done,
            Cell::Tool { output, .. } | Cell::Shell { output, .. } => output.is_some(),
            Cell::User(_) | Cell::Info(_) | Cell::Error(_) => true,
        }
    }
}

/// What a submitted line asks for: a worker command, or one of the few
/// actions that need the terminal side (clipboard, file dialogs, quitting).
pub enum Action {
    None,
    Worker(Cmd),
    /// Reach the agent mid-turn. Not a `Cmd`, because nothing reads that
    /// channel while a turn is running — see `host::Steerer`.
    Steer(String),
    /// A line the person is running themselves. Its output is theirs to read
    /// and never reaches the model.
    Shell(String),
    /// A command template still to be expanded. Expansion may run `` !`cmd` ``
    /// blocks, so it happens off the event loop: a hanging one must not freeze
    /// the UI. The finished prompt comes back as [`super::worker::Out::Expanded`].
    Expand {
        template: String,
        arguments: String,
    },
    Copy(String),
    /// Ask the endpoints what they serve, behind a picker that is already up.
    RefreshModels,
    Export(String),
    New,
    ResumeList,
    Quit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Model,
    Session,
    /// A prompt of this branch to go back to.
    Rewind,
    /// What going back puts back: the conversation, the files, or both.
    Restore,
    Branch,
}

/// A generic selector overlay: models, past sessions, prompts to go back to.
pub struct Picker {
    pub title: String,
    /// (value submitted on Enter, label shown).
    pub items: Vec<(String, String)>,
    /// Position among the rows the filter leaves.
    pub index: usize,
    pub kind: PickerKind,
    /// The value already in force, marked so the list says where you are as
    /// well as where you could go. Absent when nothing is in force.
    pub active: Option<String>,
    /// What has been typed to narrow the list: a gateway serves hundreds of
    /// models, and arrows alone are no way through them.
    pub filter: String,
    /// Still being filled: the list is up at once with what is already
    /// known, and says that more is on its way.
    pub pending: bool,
}

impl Picker {
    pub fn new(title: &str, items: Vec<(String, String)>, kind: PickerKind, active: Option<String>) -> Self {
        let index = active.as_ref().and_then(|active| items.iter().position(|item| item.0 == *active));
        Self {
            title: title.to_owned(),
            items,
            index: index.unwrap_or(0),
            kind,
            active,
            filter: String::new(),
            pending: false,
        }
    }

    /// The rows the filter leaves, in their own order.
    pub fn shown(&self) -> Vec<&(String, String)> {
        let wanted = self.filter.to_lowercase();
        self.items.iter().filter(|(_, label)| label.to_lowercase().contains(&wanted)).collect()
    }

    /// Type into, or erase from, the filter; the selection starts over.
    pub fn narrow(&mut self, typed: Option<char>) {
        match typed {
            Some(character) => self.filter.push(character),
            None => drop(self.filter.pop()),
        }
        self.index = 0;
    }
}

/// A report the harness was asked for: the command list, token usage, who is
/// running. Shown above the input and dismissed with Esc, rather than pushed
/// into the transcript — it answers a question the user asked the harness, and
/// mixing it into the agent's output makes both harder to read.
pub struct Panel {
    pub title: String,
    pub body: String,
    /// Where the view starts and how much of it one screen holds. A key can
    /// only ask to move; drawing settles what that meant, because the room a
    /// panel gets is decided by the layout and by nothing else.
    pub offset: std::cell::Cell<usize>,
    pub page: std::cell::Cell<usize>,
}

impl Panel {
    pub fn new(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self { title: title.into(), body: body.into(), offset: 0.into(), page: 1.into() }
    }

    pub fn scroll(&self, delta: isize) {
        self.offset.set(self.offset.get().saturating_add_signed(delta));
    }
}

pub struct App {
    pub cells: Vec<Cell>,
    pub editor: Editor,
    /// Cells [0..flushed) are already printed into the terminal scrollback.
    pub flushed: usize,
    pub busy: bool,
    pub spinner: usize,
    pub started_at: Option<Instant>,
    /// When the last turn ended, so a key meant to stop it is not read as
    /// something else a moment later.
    pub ended_at: Option<Instant>,
    pub context_chars: usize,
    /// Tokens the session has spent, in and out, counted as requests end —
    /// kept here so asking does not have to wait for the agent to be free.
    pub tokens: (u64, u64),
    /// Threshold the context is measured against, when the model's window is
    /// declared; without it the status line can only show a raw count.
    pub compact_chars: Option<usize>,
    pub model: String,
    pub session: String,
    pub identity: String,
    /// Everyone in this project, mine and otherwise — one list.
    pub agents: Vec<Participant>,
    /// What the agent said it is working through, if it said.
    pub plan: Vec<elmee_core::plan::Step>,
    pub queued: VecDeque<String>,
    pub commands: Vec<Command>,
    /// Project files for `@` completion; filled on the first mention.
    pub files: Vec<String>,
    /// The same files, for asking whether a mention names one.
    known: HashSet<String>,
    /// The completion for what is being typed, computed once per keystroke
    /// rather than once per frame per caller: three callers at sixty frames a
    /// second were filtering twenty thousand paths a hundred and eighty times
    /// a second while an `@` was on the line.
    completion_cache: RefCell<Option<Remembered>>,
    /// Selected row in the completion menu (wraps around the matches).
    pub menu_index: usize,
    /// Open selector overlay (models / sessions / prompts / branches).
    pub picker: Option<Picker>,
    pub dialog: Option<Dialog>,
    pub pending_dialogs: VecDeque<(Option<String>, elmee_core::InteractionRequest)>,
    pub cwd: PathBuf,
    /// Whether sessions persist (false under --no-session).
    pub persist: bool,
    /// Index of the tool cell whose full output is shown as an overlay above
    /// the input; toggled with Ctrl+O.
    pub expanded_view: Option<usize>,
    /// An open report. Esc closes it.
    pub panel: Option<Panel>,
    pub ctrl_c_armed: bool,
    /// One Esc on an empty prompt; a second opens the way back to an earlier one.
    pub escape_armed: bool,
    pub should_quit: bool,
    /// Set by /clear. The event loop scrolls the screen away and redraws from
    /// nothing; the scrollback keeps what was there, because it belongs to the
    /// user and not to us.
    pub clear_screen: bool,
    /// What goes on the screen once it has been cleared: the conversation as
    /// it now stands, after a move to another branch or session.
    pub after_clear: Vec<Cell>,
    /// The prompts of the open rewind list whose files were recorded, so the
    /// next question — put the files back too? — is asked only where it can be.
    pub recorded: Vec<u64>,
    /// A walk of the project is already in flight, so a second `@` does not
    /// start another.
    finding_files: bool,
}

impl App {
    pub fn new(model: String, session: String, commands: Vec<Command>, cwd: PathBuf, persist: bool) -> Self {
        Self {
            cells: vec![Cell::Info(format!(
                "elmee — {}",
                super::keymap::hints(super::keymap::Mode::Editing { menu_open: false, searching: false })
            ))],
            editor: Editor::with_global_history(),
            finding_files: false,
            flushed: 0,
            busy: false,
            spinner: 0,
            started_at: None,
            ended_at: None,
            tokens: (0, 0),
            context_chars: 0,
            compact_chars: None,
            model,
            session,
            identity: String::new(),
            agents: Vec::new(),
            plan: Vec::new(),
            queued: VecDeque::new(),
            commands,
            files: Vec::new(),
            known: HashSet::new(),
            completion_cache: RefCell::new(None),
            menu_index: 0,
            picker: None,
            dialog: None,
            pending_dialogs: VecDeque::new(),
            cwd,
            persist,
            expanded_view: None,
            panel: None,
            ctrl_c_armed: false,
            escape_armed: false,
            should_quit: false,
            clear_screen: false,
            after_clear: Vec::new(),
            recorded: Vec::new(),
        }
    }

    /// Hand back what was waiting behind a turn that will not run it, without
    /// overwriting a line the user has since started typing.
    pub(crate) fn restore_queued(&mut self) {
        let mut lines: Vec<String> = self.queued.drain(..).collect();
        if lines.is_empty() {
            return;
        }
        if !self.editor.is_empty() {
            lines.push(self.editor.text().to_owned());
        }
        self.editor.replace(&lines.join("\n"));
    }

    /// Apply one worker message; returns a prompt to auto-send when a queued
    /// follow-up becomes eligible.
    pub fn apply(&mut self, out: Out) -> Option<String> {
        match out {
            Out::Started => {
                self.busy = true;
                self.started_at = Some(Instant::now());
                self.expanded_view = None; // a new turn dismisses the overlay
            }
            Out::Event(event) => self.apply_event(event),
            Out::Done { error, aborted } => {
                self.busy = false;
                self.started_at = None;
                self.ended_at = Some(Instant::now());
                self.finish_streams();
                let stopped = self.stop_open_calls();
                if aborted || error.is_some() {
                    match error {
                        Some(message) => self.cells.push(Cell::Error(message)),
                        // The call that was cut short already says so.
                        None if stopped => {}
                        None => self.cells.push(Cell::Info("stopped".to_owned())),
                    }
                    // A turn that ended badly must not send what was waiting
                    // behind it, but the person typed those words and they are
                    // not the harness's to throw away: they go back to the
                    // editor, where they can be sent, edited, or abandoned.
                    self.restore_queued();
                } else if let Some(next) = self.queued.pop_front() {
                    return Some(next);
                }
            }
            // An empty listing is a walk that failed — `rg` missing, or a tree
            // it could not read — so the next mention is allowed to try again
            // rather than the session going without completion for good.
            Out::Files(files) => {
                self.finding_files = !files.is_empty();
                self.known = files.iter().cloned().collect();
                self.files = files;
            }
            Out::Shell { ok: succeeded, output: text } => {
                if let Some(Cell::Shell { output, ok, .. }) =
                    self.cells.iter_mut().rev().find(|cell| matches!(cell, Cell::Shell { output: None, .. }))
                {
                    *ok = succeeded;
                    *output = Some(if text.trim().is_empty() {
                        format!("(no output, exit {})", if succeeded { 0 } else { 1 })
                    } else {
                        text.trim_end().to_owned()
                    });
                }
            }
            Out::Models(models) => {
                // Filled into the list that is already up. A list the person
                // closed while waiting stays closed: an answer that arrives
                // late must not open something over what they went on to do.
                if let Some(picker) = self.picker.as_mut().filter(|picker| picker.pending) {
                    picker.pending = false;
                    // Nothing new to say leaves the list as it stands.
                    if models.is_empty() {
                        return None;
                    }
                    let chosen = picker.shown().get(picker.index).map(|item| item.0.clone());
                    if !models.contains(&self.model) {
                        picker.items = vec![(self.model.clone(), self.model.clone())];
                    } else {
                        picker.items.clear();
                    }
                    picker.items.extend(models.into_iter().map(|model| (model.clone(), model)));
                    let at =
                        chosen.and_then(|chosen| picker.shown().iter().position(|item| item.0 == chosen));
                    picker.index = at.unwrap_or(0);
                }
            }
            Out::ModelChanged(model) => {
                self.cells.push(Cell::Info(format!("model → {model}")));
                self.model = model;
            }
            Out::ReasoningEffortChanged(effort) => {
                self.cells.push(Cell::Info(format!("reasoning effort → {effort}")));
            }
            Out::Prompts(prompts) => {
                self.recorded = prompts
                    .iter()
                    .filter(|prompt| prompt.checkpoint.is_some())
                    .map(|prompt| prompt.id)
                    .collect();
                self.picker = Some(Picker::new(
                    "go back to before a prompt",
                    prompts
                        .into_iter()
                        .rev()
                        .map(|prompt| (prompt.id.to_string(), first_line(&prompt.text)))
                        .collect(),
                    PickerKind::Rewind,
                    None,
                ));
            }
            Out::Branches(branches) => {
                let active =
                    branches.iter().find(|branch| branch.active).map(|branch| branch.leaf.to_string());
                self.picker = Some(Picker::new(
                    "continue on another branch",
                    branches
                        .into_iter()
                        .map(|branch| {
                            let asked = if branch.prompts == 1 {
                                "1 prompt".into()
                            } else {
                                format!("{} prompts", branch.prompts)
                            };
                            (branch.leaf.to_string(), format!("{} · {asked}", first_line(&branch.last)))
                        })
                        .collect(),
                    PickerKind::Branch,
                    active,
                ));
            }
            Out::Moved { session, messages, draft, note } => {
                if let Some(session) = session {
                    self.session = session;
                }
                self.picker = None;
                self.plan.clear();
                self.clear_screen = true;
                self.after_clear = transcript(&messages);
                self.after_clear.push(Cell::Info(note));
                if let Some(draft) = draft {
                    self.editor.replace(&draft);
                }
            }
            // Intercepted in `dispatch`, which holds the worker channels an
            // expanded prompt is sent or steered through; never reaches here.
            Out::Expanded(_) => {}
            Out::Info(message) => {
                // Whatever was being asked for has been answered in words.
                if let Some(picker) = self.picker.as_mut() {
                    picker.pending = false;
                }
                self.cells.push(Cell::Info(message));
            }
            Out::Panel { title, body } => self.panel = Some(Panel::new(title, body)),
            Out::Status { context_chars } => self.context_chars = context_chars,
            Out::Interaction { origin, request } => {
                self.finish_streams();
                if self.dialog.is_none() {
                    self.dialog = Some(Dialog::with_origin(request, origin));
                } else {
                    self.pending_dialogs.push_back((origin, request));
                }
            }
            Out::Runtime { name, agents } => {
                self.identity = name;
                // A task that has just ended says so once, in the transcript,
                // and then leaves the footer. The footer answers what is
                // happening; what happened belongs above it, where it scrolls
                // away with everything else instead of accumulating. `/agents`
                // still lists the finished ones, because that is an explicit
                // question about the whole project.
                for ended in newly_ended(&self.agents, &agents) {
                    self.cells.push(ended);
                }
                self.agents = agents;
            }
        }
        None
    }

    /// Close an open chain of reasoning.
    ///
    /// Only the newest cell is ever still arriving, so a reasoning cell left
    /// open once something followed it would redraw its tail in the live area
    /// for the rest of the session and never become scrollback.
    fn finish_reasoning(&mut self) {
        if let Some(Cell::Reasoning { done, .. }) = self.cells.last_mut() {
            *done = true;
        }
    }

    fn apply_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Reasoning { text } => match self.cells.last_mut() {
                Some(Cell::Reasoning { text: existing, done: false }) => existing.push_str(&text),
                _ => self.cells.push(Cell::Reasoning { text, done: false }),
            },
            AgentEvent::Text { text } => {
                self.finish_reasoning();
                match self.cells.last_mut() {
                    Some(Cell::Assistant { text: existing, done: false, .. }) => existing.push_str(&text),
                    _ => self.cells.push(Cell::Assistant { text, done: false, flushed: 0 }),
                }
            }
            AgentEvent::ToolStart { name, arguments, .. } => {
                self.finish_reasoning();
                self.finish_streams();
                self.cells.push(Cell::Tool { name, arguments, output: None, ok: true });
            }
            AgentEvent::ToolEnd { output, .. } => {
                if let Some(Cell::Tool { output: slot @ None, ok, .. }) =
                    self.cells.iter_mut().rev().find(|cell| matches!(cell, Cell::Tool { output: None, .. }))
                {
                    *ok = !output.starts_with("Error:");
                    *slot = Some(output);
                }
            }
            AgentEvent::Plan { steps } => self.plan = steps,
            // Said rather than asked: this happens mid-turn, with nobody to
            // ask, so the person finds out by being told it happened.
            AgentEvent::Compacting { .. } => {}
            AgentEvent::Compacted { notes, .. } => self.cells.push(Cell::Info(match notes {
                0 => "context compacted".to_owned(),
                kept => format!("context compacted · {kept} note(s) kept in project memory"),
            })),
            AgentEvent::ModelEnd { input_tokens, output_tokens, .. } => {
                self.tokens = (self.tokens.0 + input_tokens, self.tokens.1 + output_tokens);
            }
            // How full the context is, as of the request just sent: the
            // figure moves while the turn runs, not only when it is over.
            AgentEvent::ModelStart { context_chars, .. } => self.context_chars = context_chars,
        }
    }

    /// Move the picker selection, wrapping around.
    pub fn picker_move(&mut self, delta: isize) {
        if let Some(picker) = &mut self.picker {
            let len = picker.shown().len() as isize;
            if len > 0 {
                picker.index = ((picker.index as isize + delta % len + len) % len) as usize;
            }
        }
    }

    /// Enter in the picker: the selected (kind, value).
    pub fn picker_selected(&self) -> Option<(PickerKind, String)> {
        let picker = self.picker.as_ref()?;
        picker.shown().get(picker.index).map(|item| (picker.kind, item.0.clone()))
    }

    /// Whether this session has a background agent still able to act.
    pub fn has_running_tasks(&self) -> bool {
        self.agents.iter().any(Participant::is_live_task)
    }

    /// Toggle the full-output overlay: opens on the newest long tool output,
    /// closes if already open.
    pub fn toggle_expanded(&mut self) {
        if self.expanded_view.take().is_some() {
            return;
        }
        for index in (0..self.cells.len()).rev() {
            // Any result, not only a long one: a call that worked shows no
            // output at all now, so "long enough to have been cut" stopped
            // being the thing that makes it worth opening.
            if let Cell::Tool { name, arguments, output: Some(output), ok } = &self.cells[index]
                && !super::ui::tool_body(name, arguments, output, *ok).0.trim().is_empty()
            {
                self.expanded_view = Some(index);
                return;
            }
        }
    }

    /// The whole transcript as Markdown, for /export.
    pub fn transcript_markdown(&self) -> String {
        let mut text = String::new();
        for cell in &self.cells {
            match cell {
                Cell::User(prompt) => text.push_str(&format!("## ❯ {prompt}\n\n")),
                // The model never saw it, so a transcript of the conversation
                // does not carry it either.
                Cell::Shell { .. } => {}
                Cell::Assistant { text: reply, .. } => text.push_str(&format!("{reply}\n\n")),
                Cell::Tool { name, arguments, output, .. } => {
                    text.push_str(&format!("### tool: {name} {arguments}\n"));
                    if let Some(output) = output {
                        text.push_str(&format!("```\n{output}\n```\n\n"));
                    }
                }
                // The reasoning is the model working, not what it produced.
                Cell::Reasoning { .. } | Cell::Info(_) | Cell::Error(_) => {}
            }
        }
        text
    }

    /// Nothing is still arriving, so nothing is still open.
    /// Close every stream that is still open, not merely the newest.
    ///
    /// Only the last cell used to be closed, which is right exactly while
    /// nothing has been pushed since — and reasoning arriving between two
    /// answers pushes. The first answer then stayed open forever.
    fn finish_streams(&mut self) {
        for cell in &mut self.cells {
            if let Cell::Assistant { done, .. } | Cell::Reasoning { done, .. } = cell {
                *done = true;
            }
        }
    }

    /// A call still open when its turn ended will never be answered: it was
    /// stopped, and a spinner frozen in the scrollback would go on saying
    /// otherwise. Says whether there was one.
    fn stop_open_calls(&mut self) -> bool {
        let mut stopped = false;
        for cell in &mut self.cells {
            if let Cell::Tool { output: output @ None, ok, .. } = cell {
                *output = Some(String::new());
                *ok = false;
                stopped = true;
            }
        }
        stopped
    }

    /// Interpret a submitted line: slash commands act locally, map to worker
    /// commands, or expand a custom template; anything else is a prompt
    /// (queued if a turn is running).
    pub fn menu_items(&self) -> Vec<Candidate> {
        self.completion().map_or_else(Vec::new, |completion| completion.items.clone())
    }

    pub fn menu_open(&self) -> bool {
        self.completion().is_some_and(|completion| !completion.items.is_empty())
    }

    /// Whether a mention names a file the project actually has.
    pub fn known_file(&self, path: &str) -> bool {
        self.known.contains(path)
    }

    /// What is being completed, remembered until the typing changes.
    pub(super) fn completion(&self) -> Option<Rc<Completion>> {
        let typing = self.editor.typing();
        let mut cache = self.completion_cache.borrow_mut();
        if let Some((typed, files, completion)) = &*cache
            && typed == typing
            && *files == self.files.len()
        {
            return completion.clone();
        }
        let completion = self.complete(typing).map(Rc::new);
        *cache = Some((typing.to_owned(), self.files.len(), completion.clone()));
        completion
    }

    /// How many bytes before the cursor a choice replaces, and what the
    /// choices are.
    ///
    /// A word that is already typed out in full is not among them. It inserts
    /// nothing, and an open menu costs more than a row: it holds the arrows, so
    /// a finished line kept history out of reach.
    ///
    /// Read from the cursor's own line rather than the whole text, so a `/` on
    /// the third line of a prompt completes like one on the first.
    fn complete(&self, typing: &str) -> Option<Completion> {
        if let Some(rest) = typing.strip_prefix('/') {
            return Some(match rest.split_once(char::is_whitespace) {
                // Past the name: the command decides what its arguments are.
                Some((name, arguments)) => {
                    let partial = arguments.rsplit(char::is_whitespace).next().unwrap_or_default();
                    Completion {
                        replacing: partial.len(),
                        items: argument_values(name, arguments)
                            .into_iter()
                            .filter(|(value, _)| value.starts_with(partial) && value != partial)
                            .map(|(value, description)| Candidate {
                                matched: partial.len(),
                                value,
                                description,
                            })
                            .collect(),
                    }
                }
                None => Completion {
                    replacing: typing.len(),
                    items: self
                        .commands
                        .iter()
                        .filter(|command| command.name.starts_with(rest) && command.name != rest)
                        .map(|command| Candidate {
                            matched: rest.len() + 1,
                            value: format!(
                                "/{}{}",
                                command.name,
                                command.hint.as_deref().map_or_else(String::new, |hint| format!(" {hint}"))
                            ),
                            description: command.description.clone(),
                        })
                        .collect(),
                },
            });
        }
        let prefix = mention_prefix(typing)?;
        Some(Completion {
            replacing: prefix.len() + 1,
            items: self
                .files
                .iter()
                .filter(|path| path.contains(prefix))
                .take(50)
                .map(|path| Candidate {
                    // Bold where it matched, which for a path is wherever the
                    // typed fragment turned up rather than always the front.
                    matched: 0,
                    value: format!("@{path}"),
                    description: String::new(),
                })
                .collect(),
        })
    }

    /// Move the menu selection up (-1) or down (+1), wrapping around.
    pub fn menu_move(&mut self, delta: isize) {
        let len = self.menu_items().len() as isize;
        if len == 0 {
            return;
        }
        let index = self.menu_index as isize % len;
        self.menu_index = ((index + delta % len + len) % len) as usize;
    }

    /// The command Enter means while the line is one unfinished command name:
    /// the highlighted row, unless what is typed already names a command and
    /// the selection was never moved off it.
    pub fn highlighted_command(&self) -> Option<Command> {
        let typed = self.editor.text().strip_prefix('/')?;
        if typed.contains(char::is_whitespace) {
            return None;
        }
        let named = |name: &str| self.commands.iter().find(|command| command.name == name).cloned();
        if self.menu_index == 0 && named(typed).is_some() {
            return None;
        }
        let items = self.menu_items();
        let chosen = items.get(self.menu_index % items.len().max(1))?;
        named(chosen.value.strip_prefix('/')?.split_whitespace().next()?)
    }

    /// Whether Enter means the highlighted argument: a command's argument
    /// that comes from a closed set and is still being chosen.
    pub fn choosing_argument(&self) -> bool {
        let typing = self.editor.typing();
        typing.starts_with('/') && typing.contains(char::is_whitespace) && self.menu_open()
    }

    /// Tab/Enter on the menu: replace the token being typed with the choice.
    pub fn complete_menu(&mut self) {
        let Some(completion) = self.completion() else { return };
        if completion.items.is_empty() {
            return;
        }
        let choice = completion.items[self.menu_index % completion.items.len()].value.clone();
        // Only the token being typed is replaced, wherever it sits: a mention
        // in the middle of a sentence, or a command on the third line.
        let choice = choice.split_whitespace().next().unwrap_or(&choice).to_owned();
        self.editor.splice_before_cursor(completion.replacing, &format!("{choice} "));
        self.menu_index = 0;
    }

    /// Whether the project's files still have to be found, for `@` completion.
    ///
    /// Answered rather than done here: walking a repository is a subprocess and
    /// a wait, and it used to happen inline the first time anybody typed `@` —
    /// so on a large tree the interface simply stopped until `rg` came back.
    pub fn wants_files(&mut self) -> bool {
        let wanted = self.files.is_empty() && !self.finding_files;
        self.finding_files |= wanted;
        wanted
    }
}

/// What was being typed, over how many files, and what it completed to.
type Remembered = (String, usize, Option<Rc<Completion>>);

/// One thing the menu can put in, and how much of it the person already typed.
#[derive(Clone)]
pub(super) struct Candidate {
    pub value: String,
    pub description: String,
    /// Bytes at the front of `value` that the typing accounts for, so the menu
    /// can show *why* a row is in it. Zero where the match was not at the front.
    pub matched: usize,
}

pub(super) struct Completion {
    pub replacing: usize,
    pub items: Vec<Candidate>,
}

/// What a command's arguments can be, where they are a closed set. Nothing
/// currently is: a path or an entry id is not a set, and pretending it is
/// would fill the menu with guesses.
fn argument_values(name: &str, _arguments: &str) -> Vec<(String, String)> {
    match name {
        "reasoning" => std::iter::once("auto")
            .chain(elmee_core::provider::ReasoningEffort::VALUES)
            .map(|level| (level.to_owned(), String::new()))
            .collect(),
        _ => Vec::new(),
    }
}

fn mention_prefix(text: &str) -> Option<&str> {
    let word = text.rsplit(char::is_whitespace).next()?;
    word.strip_prefix('@').filter(|rest| !rest.contains('@'))
}

#[cfg(test)]
mod tests;

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_owned()
}

/// How many prompts of a conversation are put back on screen when it is
/// opened: enough to see where things stand, not a replay of a week's work.
const SHOWN_PROMPTS: usize = 12;

/// A conversation as the cells that would have shown it happening.
///
/// Only what a person typed comes back in their voice: the harness also
/// writes in the user's role — an identity stamp, a peer's report — and a
/// system prompt is not something anybody said.
pub fn transcript(messages: &[Message]) -> Vec<Cell> {
    let asked =
        |message: &Message| matches!(message.role, Role::User) && message.origin == MessageOrigin::User;
    let start = messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, message)| asked(message))
        .nth(SHOWN_PROMPTS - 1)
        .map_or(0, |(index, _)| index);
    let mut cells = Vec::new();
    if start > 0 {
        cells.push(Cell::Info(format!("… {start} earlier messages are in the session and not shown")));
    }
    for message in &messages[start..] {
        let text = message.content.clone().filter(|text| !text.is_empty());
        match message.role {
            Role::User if asked(message) => cells.extend(text.map(Cell::User)),
            Role::Assistant => {
                cells.extend(text.map(|text| Cell::Assistant { text, done: true, flushed: 0 }));
                cells.extend(message.tool_calls.iter().map(|call| Cell::Tool {
                    name: call.function.name.clone(),
                    arguments: call.function.arguments.clone(),
                    output: None,
                    ok: true,
                }));
            }
            Role::Tool => {
                // Results arrive in the order their calls were made.
                if let Some(Cell::Tool { output: slot @ None, ok, .. }) =
                    cells.iter_mut().find(|cell| matches!(cell, Cell::Tool { output: None, .. }))
                {
                    let output = text.unwrap_or_default();
                    *ok = !output.starts_with("Error:");
                    *slot = Some(output);
                }
            }
            _ => {}
        }
    }
    // A call the conversation never answered was cut short, and a cell left
    // open would sit in the live area for ever.
    for cell in &mut cells {
        if let Cell::Tool { output: slot @ None, ok, .. } = cell {
            *slot = Some(String::new());
            *ok = false;
        }
    }
    cells
}

/// The tasks that reached an end between one snapshot and the next.
fn newly_ended(before: &[Participant], now: &[Participant]) -> Vec<Cell> {
    let was_running = |name: &str| {
        before
            .iter()
            .find(|agent| agent.name == name)
            .and_then(|agent| agent.task.as_ref())
            .is_some_and(|task| task.finished_at.is_none())
    };
    now.iter()
        .filter_map(|agent| {
            let task = agent.task.as_ref()?;
            if task.finished_at.is_none() || !was_running(&agent.name) {
                return None;
            }
            let line = format!("{} {} · {}", icon(&task.status), agent.name, task.detail);
            Some(if task.status == "completed" { Cell::Info(line) } else { Cell::Error(line) })
        })
        .collect()
}

fn icon(status: &str) -> char {
    match status {
        "completed" => '✓',
        "stopped" => '■',
        _ => '✘',
    }
}
