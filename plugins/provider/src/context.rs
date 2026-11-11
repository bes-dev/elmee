//! Cache/routing identity belongs to a conversation, including when a TUI
//! changes conversation or an ACP host keeps many agents in one process.
use std::{cell::RefCell, path::Path};

pub struct SessionContext(RefCell<String>);

impl Default for SessionContext {
    fn default() -> Self {
        Self(RefCell::new(crate::auth::new_session_id()))
    }
}

impl SessionContext {
    pub fn set(&self, path: Option<&Path>) {
        let key = match path {
            None => crate::auth::new_session_id(),
            Some(path) => {
                let absolute = path.canonicalize().unwrap_or_else(|_| {
                    let parent =
                        path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
                    let parent = parent.canonicalize().unwrap_or_else(|_| parent.to_path_buf());
                    path.file_name().map_or(parent.clone(), |file| parent.join(file))
                });
                let digest =
                    ring::digest::digest(&ring::digest::SHA256, absolute.to_string_lossy().as_bytes());
                digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
            }
        };
        *self.0.borrow_mut() = key;
    }

    pub(crate) fn key(&self) -> String {
        self.0.borrow().clone()
    }

    pub(crate) fn routing_id(&self) -> String {
        let key = self.key();
        if key.len() == 36 {
            return key;
        }
        format!("{}-{}-4{}-a{}-{}", &key[..8], &key[8..12], &key[13..16], &key[17..20], &key[20..32])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_conversations_keep_their_key_across_provider_instances() {
        let first = SessionContext::default();
        let next = SessionContext::default();
        assert_ne!(first.key(), next.key());
        first.set(Some(Path::new("/tmp/elmee-context-a.jsonl")));
        next.set(Some(Path::new("/tmp/elmee-context-a.jsonl")));
        assert_eq!(first.key(), next.key());
        next.set(Some(Path::new("/tmp/elmee-context-b.jsonl")));
        assert_ne!(first.key(), next.key());
        assert!(!first.key().contains("elmee-context"));
    }
}
