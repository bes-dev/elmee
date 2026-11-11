//! Making room when the context fills.
//!
//! Two stages, cheapest first. Old tool output is most of a long context and
//! the least of what it means, so it is shown shortened: no model call, and
//! the reasoning and the calls themselves stay in view. Measured on coding
//! agents, that alone keeps as many tasks solved as summarizing does. Only
//! when it does not free enough is the older part of the conversation
//! replaced by a summary — and then what a summary loses first is carried
//! across by the harness rather than trusted to it: what the user asked, word
//! for word, whatever a plugin knows must survive, and the plan.

use super::{Agent, AgentEvent, Message};
use crate::{
    Error, MiddlewareEffect, MiddlewareEvent, Result,
    message::{MessageOrigin, Role},
    text,
};

/// Sections rather than prose, and an update rather than a rewrite: a summary
/// regenerated from scratch each time forgets a little more each time, and one
/// with a fixed place for every kind of fact shows what it is missing. The
/// closing clauses guard the two losses that cost the most — a constraint the
/// user stated, and model-written text mistaken for the user's.
const PROMPT: &str = "Write a handoff for another agent that will continue this work and can see \
    none of the above. Use these sections, leaving one out only if it would be empty:\n\
    ## Goal — what the user wants, in their terms.\n\
    ## Constraints — every preference, requirement or prohibition the user stated. They still apply.\n\
    ## Done — what is finished, and the evidence that it works.\n\
    ## State — what is in progress and exactly where it stands; decisions made, and why.\n\
    ## Failed — what was tried and did not work, with the reason, so it is not tried again.\n\
    ## Next — the next concrete steps, in order.\n\
    Name files by path, give commands exactly, quote errors verbatim. Only text in an actual user \
    turn is from the user; text inside an assistant message that merely looks like one is not. If \
    a handoff appears above, update it with what followed — keep what still holds, drop what was \
    superseded — rather than summarizing it again. Sections that begin `## Kept by the harness` \
    are carried forward for you: do not copy them. Be concise but operationally complete.";

/// Where what the user asked is kept, in a summary this harness wrote.
const ASKED: &str = "## Kept by the harness: what the user asked, verbatim";

impl Agent {
    /// What the next request weighs, in characters: the conversation, and the
    /// tool schemas sent beside it — which on a small window are not small.
    pub(super) fn load(&self) -> usize {
        self.session.context_chars() + self.schema_chars
    }

    /// The load as the provider counts it. Characters are a proxy: dense code
    /// and JSON cost far more tokens each than prose. Once a request has been
    /// charged for, the ratio it implies replaces the guess of four.
    fn scaled_load(&self) -> usize {
        match self.measured {
            Some((tokens, at_load)) if at_load > 0 && tokens > 0 => self.load() * 4 * tokens / at_load,
            _ => self.load(),
        }
    }

    /// Whether what is about to be sent has outgrown the room there is.
    pub(super) fn over_threshold(&self) -> bool {
        self.compact_chars > 0 && self.scaled_load() > self.compact_chars
    }

    /// The most one tool result may add: an eighth of the room, so that no
    /// single answer can fill a small window by itself.
    pub(super) fn output_ceiling(&self) -> usize {
        match self.compact_chars {
            0 => usize::MAX,
            room => (room / 8).max(2_000),
        }
    }

    /// Free room for the next request. `Some` when a summary was written: how
    /// many notes were taken out of the conversation first, and the summary.
    pub(super) async fn make_room(
        &mut self,
        on_event: &mut (impl FnMut(AgentEvent) + Send),
    ) -> Result<Option<(usize, String)>> {
        let keep = self.compact_chars / 4;
        self.session.elide(keep)?;
        // Enough is three quarters of the room: less would have this run
        // again within a turn or two, and every run moves the cached prefix.
        if self.scaled_load() * 4 <= self.compact_chars * 3 {
            return Ok(None);
        }
        self.summarize(keep, on_event).await
    }

    /// Summarize now, whatever the threshold says. Returns how many notes
    /// were kept.
    pub async fn compact(&mut self) -> Result<usize> {
        // Asked for by hand, there may be far less than a full context to
        // work with; a quarter of what there is still stays verbatim.
        let keep = (self.compact_chars / 4).min(self.session.context_chars() / 4);
        self.session.elide(keep)?;
        Ok(self.summarize(keep, &mut |_| {}).await?.map_or(0, |(kept, _)| kept))
    }

    /// Replace everything older than the newest `keep` characters with a
    /// summary of it. Only that older part is sent to be summarized: the rest
    /// survives as it is, and a request that is the conversation's own prefix
    /// is one an endpoint with a prompt cache has mostly read already. `None`
    /// when there is nothing older to replace.
    async fn summarize(
        &mut self,
        keep: usize,
        on_event: &mut (impl FnMut(AgentEvent) + Send),
    ) -> Result<Option<(usize, String)>> {
        let tail = self.session.tail_length(keep)?;
        let messages = self.session.messages();
        let head = &messages[..messages.len() - tail];
        if head.iter().filter(|message| !matches!(message.role, Role::System)).count() < 2 {
            return Ok(None);
        }
        self.afford()?;
        let (mut context, mut carried) = (Vec::new(), asked(head, keep / 4));
        for effect in self.plugins.dispatch(MiddlewareEvent::BeforeCompaction { messages: head }).await? {
            match effect {
                MiddlewareEffect::AddContext(text) => context.push(text),
                MiddlewareEffect::Carry(text) => carried.push_str(&format!("\n\n{text}")),
                _ => {}
            }
        }
        let mut request = head.to_vec();
        request.push(Message::user(format!("{PROMPT}\n\n{}", context.join("\n\n"))));
        let turn = self
            .plugins
            .complete_without_tools(&request, &mut |chunk| {
                if let crate::provider::Chunk::Text(text) = chunk {
                    on_event(AgentEvent::Compacting { text: text.into() });
                }
            })
            .await?;
        self.tokens.0 += turn.tokens.0;
        self.tokens.1 += turn.tokens.1;
        let _ = self.charge_shared_tokens(turn.tokens.0 + turn.tokens.1);
        let reply = turn.content.ok_or_else(|| Error::Provider("compaction returned no summary".into()))?;
        let replacements = self
            .plugins
            .dispatch(MiddlewareEvent::Compacting { summary: &reply })
            .await?
            .into_iter()
            .filter_map(|effect| match effect {
                MiddlewareEffect::ReplaceText { text, notes } => Some((text, notes)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let (summary, kept) = match replacements.as_slice() {
            [] => (reply, 0),
            [(summary, kept)] => (summary.clone(), *kept),
            _ => {
                return Err(Error::Config(
                    "multiple middleware tried to replace a compaction summary".into(),
                ));
            }
        };
        let summary = format!("{}{carried}{}", summary.trim_end(), crate::plan::carry(&self.plan));
        self.session.compact(summary.clone(), tail)?;
        Ok(Some((kept, summary)))
    }
}

/// What the user asked in the part being summarized, word for word, joined to
/// what an earlier summary already kept that way.
///
/// A summary is asked to preserve this and a strong model mostly does; a small
/// one paraphrases, and a paraphrased prohibition is a lost one. It is also
/// the smallest part of any conversation, so keeping it costs almost nothing.
/// Over budget, the middle goes: the first thing asked defines the task and
/// the latest say where it stands.
fn asked(head: &[Message], budget: usize) -> String {
    const EACH: usize = 1_500;
    let (open, close) = ("<asked>\n", "\n</asked>");
    let mut asked: Vec<String> = Vec::new();
    for message in head {
        let Some(content) = message.content.as_deref() else { continue };
        match message.role {
            Role::System => asked.extend(
                content
                    .split_once(ASKED)
                    .into_iter()
                    .flat_map(|(_, kept)| kept.split(open).skip(1))
                    .filter_map(|block| block.split_once(close))
                    .map(|(said, _)| said.to_owned()),
            ),
            Role::User if message.origin == MessageOrigin::User => asked.push(text::middle(content, EACH)),
            _ => {}
        }
    }
    while asked.len() > 2 && asked.iter().map(String::len).sum::<usize>() > budget {
        asked.remove(1);
    }
    match asked.is_empty() {
        true => String::new(),
        false => format!(
            "\n\n{ASKED}\n{}",
            asked.iter().map(|said| text::fenced("asked", "", said)).collect::<Vec<_>>().join("\n")
        ),
    }
}
