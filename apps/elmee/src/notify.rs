//! `elmee notify`: the one way anything outside an agent hands a running one
//! something to act on.

use elmee_core::{Error, Result};

/// How anything outside elmee — cron, a webhook receiver, a file watcher —
/// hands a running agent something to act on.
///
/// What is piped in becomes the event's payload, quoted apart from the text and
/// marked untrusted: `gh api ... | elmee notify "triage this issue"` must not
/// let the issue body speak with the operator's authority.
pub fn notify(argv: &[String]) -> Result<i32> {
    let (to, rest) = match argv.get(1).map(String::as_str) {
        Some("--to") => (argv.get(2).map(String::as_str), argv.get(3..).unwrap_or_default()),
        _ => (None, argv.get(1..).unwrap_or_default()),
    };
    let text = rest.iter().filter(|arg| !arg.starts_with("--")).cloned().collect::<Vec<_>>().join(" ");
    if text.trim().is_empty() {
        return Err(Error::Config("usage: elmee notify [--to NAME] [--stdin] TEXT".into()));
    }
    let cwd = std::env::current_dir()?;
    match elmee_plugin_mesh::notify(&cwd, to, &text, piped(argv).as_deref()) {
        Ok(report) => {
            println!("{report}");
            Ok(0)
        }
        Err(error) => {
            eprintln!("elmee: {error}");
            Ok(1)
        }
    }
}

/// The untrusted payload, read only when `--stdin` asked for it.
///
/// Deliberately not inferred from stdin not being a terminal: a caller with an
/// inherited pipe it never intends to write would hang here forever, and a
/// command that waits for input nobody will send is the failure this codebase
/// keeps refusing to ship. Wanting the payload is stated, not guessed.
///
/// Bounded and read as bytes, because this is the one place the outside world
/// hands the agent something with no size and no encoding agreed in advance.
/// `read_to_string` was both unbounded — a `gh api` dump or a log file went
/// whole into an envelope, into the inbox, and into a request — and silently
/// all-or-nothing: one invalid byte failed the read, and the event was then
/// delivered with no payload at all while the caller was told it had been.
fn piped(argv: &[String]) -> Option<String> {
    use std::io::Read;
    if !argv.iter().any(|arg| arg == "--stdin") {
        return None;
    }
    let mut payload = Vec::new();
    // One byte over the limit, so what is dropped is dropped by `payload`,
    // which says so, rather than by the read, which would not.
    std::io::stdin().take(PAYLOAD_LIMIT as u64 + 1).read_to_end(&mut payload).ok()?;
    payload_of(payload)
}

fn payload_of(bytes: Vec<u8>) -> Option<String> {
    let text = elmee_core::text::bound(String::from_utf8_lossy(&bytes).into_owned(), PAYLOAD_LIMIT, true);
    (!text.trim().is_empty()).then_some(text)
}

/// As much of an external payload as a turn can carry. The same ceiling a
/// command's output gets, for the same reason: past it the model is paying to
/// read a log it cannot act on.
const PAYLOAD_LIMIT: usize = 50 * 1024;

#[cfg(test)]
mod tests {
    use super::{PAYLOAD_LIMIT, payload_of};

    /// What the outside pipes in is the one input with no size and no encoding
    /// agreed beforehand, and it was read as neither. A log file went whole
    /// into an envelope and then into a request; a payload with one invalid
    /// byte was dropped entirely while the caller was told it had been
    /// delivered.
    #[test]
    fn an_external_payload_is_bounded_and_survives_bytes_that_are_not_text() {
        let huge = payload_of(vec![b'x'; PAYLOAD_LIMIT * 3]).expect("a payload this big is still one");
        assert!(huge.len() < PAYLOAD_LIMIT + 100, "bounded: {} bytes", huge.len());
        assert!(huge.contains("truncated"), "and it says what it dropped");

        let binary = payload_of(vec![0xff, 0xfe, b'h', b'i']).expect("bytes that are not UTF-8 still arrive");
        assert!(binary.contains("hi"), "what was readable is kept: {binary:?}");

        assert_eq!(payload_of(b"   \n".to_vec()), None, "an empty pipe is no payload at all");
    }
}
