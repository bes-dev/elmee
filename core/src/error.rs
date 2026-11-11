use std::{error::Error as StdError, fmt};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug)]
pub enum Error {
    Config(String),
    /// Transport failure, carried as prose: the loop never inspects it, and
    /// keeping the concrete HTTP error type out keeps HTTP itself out.
    Http(String),
    Io(std::io::Error),
    Json(serde_json::Error),
    Provider(String),
    /// The provider refused for a reason it described in a header rather than
    /// in prose: which status, and when to come back.
    ///
    /// A separate variant because the alternative was formatting those facts
    /// into a message and parsing them out again at the point that had to
    /// decide — which is a protocol between two modules of one crate, written
    /// in English, with no compiler checking either end.
    Refused {
        status: u16,
        retry_after: Option<u64>,
        body: String,
    },
    /// The provider declared its account or rate budget exhausted. The adapter
    /// that understands the wire classifies this; core only carries the fact —
    /// no status number or vendor wording is interpreted here.
    Exhausted {
        retry_after: Option<u64>,
        message: String,
    },
    Tool(String),
    /// The request was larger than the model's context. The adapter that
    /// understands the wire recognises this; the loop answers it by making
    /// room and asking again, which is the only way to learn the size of a
    /// window nobody declared.
    Overflow(String),
    /// The provider wants an account this machine is not signed in to. Apart
    /// from other configuration errors because a frontend can do something
    /// about this one: offer the sign-in instead of reporting a failure.
    SignIn(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(value) => write!(f, "configuration error: {value}"),
            Self::Http(value) => write!(f, "http error: {value}"),
            Self::Io(value) => write!(f, "io error: {value}"),
            Self::Json(value) => write!(f, "json error: {value}"),
            Self::Provider(value) => write!(f, "provider error: {value}"),
            Self::Refused { status, body, .. } => write!(f, "provider error: HTTP {status}: {body}"),
            Self::Exhausted { message, .. } => write!(f, "provider error: {message}"),
            Self::Tool(value) => write!(f, "tool error: {value}"),
            Self::Overflow(value) => {
                write!(f, "provider error: the request does not fit the model's context: {value}")
            }
            Self::SignIn(value) => write!(f, "sign-in required: {value}"),
        }
    }
}

impl StdError for Error {}

impl Error {
    /// Account/rate exhaustion is resumable external state, not a failed
    /// session. Frontends use this to expose a stable paused state. The
    /// classification itself happens in the provider adapter — the component
    /// that understands statuses and vendor wording — and arrives here as the
    /// `Exhausted` variant.
    pub fn is_provider_quota(&self) -> bool {
        matches!(self, Self::Exhausted { .. })
    }

    /// How long the provider asked to be left alone, if it said.
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            Self::Refused { retry_after, .. } | Self::Exhausted { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn only_classified_exhaustion_is_a_resumable_quota_pause() {
        // Core never reads statuses or vendor prose: quota is quota only when
        // the adapter said so by constructing the variant.
        let exhausted = Error::Exhausted { retry_after: None, message: "usage limit".into() };
        assert!(exhausted.is_provider_quota());
        assert_eq!(exhausted.retry_after(), None, "and says so when it did not say");
        assert_eq!(
            Error::Exhausted { retry_after: Some(30), message: String::new() }.retry_after(),
            Some(30)
        );
        let refused = |status| Error::Refused { status, retry_after: None, body: String::new() };
        assert!(!refused(429).is_provider_quota(), "an unclassified refusal is not core's to interpret");
        assert!(!refused(500).is_provider_quota());
        assert!(!Error::Provider("rate limit prose".into()).is_provider_quota());
        assert!(!Error::Tool("quota in a source file".into()).is_provider_quota());
    }
}
