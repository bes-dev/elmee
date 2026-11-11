#![forbid(unsafe_code)]

//! Cross-process token accounting through a small append-only ledger.
//!
//! Model turns append their billed token count; every process sharing the path
//! reads one total. [`FileTokenLedger`] supplies core's `TokenLedger` service,
//! so a root and its children enforce one budget without a daemon or database.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use elmee_core::{Result, agent::TokenLedger};

pub struct FileTokenLedger {
    path: PathBuf,
}

impl FileTokenLedger {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl TokenLedger for FileTokenLedger {
    fn path(&self) -> Option<&Path> {
        Some(&self.path)
    }

    fn spent(&self) -> Result<u64> {
        match fs::read_to_string(&self.path) {
            Ok(text) => Ok(text.lines().filter_map(|line| line.parse::<u64>().ok()).sum()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(error.into()),
        }
    }

    fn charge(&self, tokens: u64) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut ledger = OpenOptions::new().create(true).append(true).open(&self.path)?;
        ledger.write_all(format!("{tokens}\n").as_bytes())?;
        Ok(())
    }
}

impl elmee_core::Plugin for FileTokenLedger {
    fn id(&self) -> &'static str {
        "token-ledger"
    }

    fn declare(self, plugins: &mut elmee_core::Registrar) -> Result<()> {
        plugins.provide_service::<dyn elmee_core::TokenLedger>(std::rc::Rc::new(self));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charges_are_appended_and_summed() {
        let path = std::env::temp_dir().join(format!("elmee-ledger-{}.txt", std::process::id()));
        let _ = fs::remove_file(&path);
        let ledger = FileTokenLedger::new(path.clone());
        ledger.charge(30).unwrap();
        ledger.charge(70).unwrap();
        assert_eq!(ledger.spent().unwrap(), 100);
        fs::remove_file(path).unwrap();
    }
}
