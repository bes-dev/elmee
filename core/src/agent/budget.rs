//! What a turn may spend.
//!
//! The **token budget** is money: it
//! is checked before every request this loop pays for — a turn, a compaction,
//! a completion a tool asked for — because the only reliable moment not to
//! spend is before spending, and it is shared with every child through one
//! file, so delegation cannot mint more of it. It is a ceiling on starting,
//! not a reservation: the request that crosses it is paid in full, and agents
//! that check at the same moment may each start one. Room in the context is a
//! different ceiling, and `compaction` keeps it.

use super::Agent;
use crate::{Error, Result};

pub trait TokenLedger: Send + Sync {
    fn path(&self) -> Option<&std::path::Path> {
        None
    }

    fn spent(&self) -> Result<u64>;
    fn charge(&self, tokens: u64) -> Result<()>;
}

impl Agent {
    /// Stop once this many billed tokens have been spent. Zero is unlimited;
    /// unattended callers that need a cost ceiling must opt into one.
    pub fn set_token_budget(&mut self, tokens: u64) {
        self.token_budget = tokens;
    }

    /// Tokens still available, or `None` when unlimited.
    pub fn tokens_left(&self) -> Option<u64> {
        (self.token_budget > 0).then(|| {
            let spent = self.shared_tokens_spent().unwrap_or(self.tokens.0 + self.tokens.1);
            self.token_budget.saturating_sub(spent)
        })
    }

    /// The one gate in front of every paid request. A budget that is spent
    /// stops the next request from starting, whatever it is for.
    pub(super) fn afford(&self) -> Result<()> {
        if self.tokens_left() == Some(0) {
            return Err(Error::Provider(format!(
                "billed-token budget of {} is spent across this agent tree; increase the configured \
                 budget and resume, or wait for an external provider quota to reset",
                self.token_budget
            )));
        }
        Ok(())
    }

    pub(super) fn shared_tokens_spent(&self) -> Result<u64> {
        let Some(ledger) = &self.token_ledger else {
            return Ok(self.tokens.0 + self.tokens.1);
        };
        ledger.spent()
    }

    pub(super) fn charge_shared_tokens(&self, tokens: u64) -> Result<()> {
        let Some(ledger) = &self.token_ledger else { return Ok(()) };
        ledger.charge(tokens)
    }
}
