//! Elmee background events as mesh messages.
//!
//! The data plane knows about peers, presence and durable delivery. This is
//! the only place that knows what an Elmee event looks like.

use std::path::Path;

use elmee_core::BackgroundEvent;

use crate::{Envelope, deliver, peers_of, unique};

/// Deliver an event from outside any Elmee process.
///
/// Without a recipient the event goes to every live peer in the project. The
/// operator's instruction and untrusted event data remain separate so payload
/// text cannot silently acquire instruction authority.
pub fn notify(cwd: &Path, to: Option<&str>, text: &str, data: Option<&str>) -> Result<String, String> {
    let base = elmee_plugin_paths::project_state_directory(cwd);
    let recipients: Vec<String> = match to {
        Some(name) => vec![name.to_owned()],
        None => peers_of(&base, "")
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|peer| peer.name)
            .collect(),
    };
    if recipients.is_empty() {
        return Err(format!("no elmee agent is running in {}", cwd.display()));
    }

    let id = unique("external");
    let body = match data {
        None => text.to_owned(),
        Some(data) => format!("{text}\n{}", elmee_core::text::fenced("data", " trusted=\"no\"", data)),
    };
    let prompt = elmee_core::text::fenced("external_event", &format!(" id=\"{id}\""), &body);
    let (delivered, failed): (Vec<_>, Vec<_>) = recipients.into_iter().partition(|recipient| {
        persist_event(&base, recipient, BackgroundEvent { id: id.clone(), prompt: prompt.clone() })
    });
    if delivered.is_empty() {
        return Err(format!("could not write to any inbox ({})", failed.join(", ")));
    }
    Ok(if failed.is_empty() {
        format!("delivered to {}", delivered.join(", "))
    } else {
        format!("delivered to {}; failed for {}", delivered.join(", "), failed.join(", "))
    })
}

/// Returns whether the event became durable.
pub fn persist_event(base: &Path, recipient: &str, event: BackgroundEvent) -> bool {
    persist_event_from(base, recipient, event, None)
}

/// Deliver an event, optionally recording which peer is owed an answer.
pub fn persist_event_from(base: &Path, recipient: &str, event: BackgroundEvent, from: Option<&str>) -> bool {
    persist_message(base, recipient, event, from)
}

/// Translate an Elmee event into a mesh envelope and enqueue it.
pub fn persist_message(base: &Path, recipient: &str, event: BackgroundEvent, from: Option<&str>) -> bool {
    deliver(
        base,
        recipient,
        &Envelope { id: event.id, prompt: event.prompt, from: from.map(str::to_owned), ..Default::default() },
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn an_elmee_event_is_written_as_a_mesh_envelope() {
        let base = std::env::temp_dir().join(unique("elmee-adapter-test"));
        let event = BackgroundEvent { id: "e1".into(), prompt: "hello".into() };
        assert!(persist_event_from(&base, "worker", event, Some("leader")));

        let bytes = fs::read(base.join("inbox/worker/e1.json")).unwrap();
        let envelope: Envelope = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(envelope.id, "e1");
        assert_eq!(envelope.prompt, "hello");
        assert_eq!(envelope.from.as_deref(), Some("leader"));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn a_delivery_that_could_not_be_written_is_reported() {
        let base = std::env::temp_dir().join(unique("elmee-persist-test"));
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("inbox"), b"not a directory").unwrap();
        assert!(!persist_event(&base, "worker", BackgroundEvent { id: "e1".into(), prompt: "x".into() }));
        let _ = fs::remove_dir_all(base);
    }
}
