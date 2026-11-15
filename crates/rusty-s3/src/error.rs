//! The ways a request to an S3 server fails, as a fixed set of categories: they are what the admin web shows and what a retry decision is made on. No server text, no URL, no
//! credential ever ends up in one.

use rusty_core::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum S3Fail {
    /// The name did not resolve, or the connector refused the address (the endpoint policy).
    Resolve,
    Connect,
    Tls,
    Timeout,
    /// The access key is unknown or the signature was refused (a wrong secret, a clock far off).
    Auth,
    /// Known key, not allowed to do this.
    Denied,
    NoBucket,
    NotFound,
    /// The server asks to slow down.
    Throttled,
    /// 5xx.
    Server,
    /// The reply is not what S3 says (a redirect, a malformed status line, an unexpected status).
    Protocol,
    /// A reply larger than the limit of the call.
    TooLarge,
}

impl S3Fail {
    /// The same failure may go away by itself: retry later, answer the client with a temporary status.
    pub fn transient(self) -> bool {
        matches!(self, S3Fail::Resolve | S3Fail::Connect | S3Fail::Timeout | S3Fail::Throttled | S3Fail::Server)
    }

    /// A short fixed name (for the admin web and the logs).
    pub fn name(self) -> &'static str {
        match self {
            S3Fail::Resolve => "resolve",
            S3Fail::Connect => "connect",
            S3Fail::Tls => "tls",
            S3Fail::Timeout => "timeout",
            S3Fail::Auth => "auth",
            S3Fail::Denied => "denied",
            S3Fail::NoBucket => "no-bucket",
            S3Fail::NotFound => "not-found",
            S3Fail::Throttled => "throttled",
            S3Fail::Server => "server",
            S3Fail::Protocol => "protocol",
            S3Fail::TooLarge => "too-large",
        }
    }
}

impl From<S3Fail> for Error {
    /// A missing object is `NotFound`; a failure that may pass is `Busy` (SMTP 451, IMAP `[UNAVAILABLE]`); the rest (bad credentials, no bucket, a broken reply) is a
    /// storage fault that retrying does not fix.
    fn from(f: S3Fail) -> Error {
        match f {
            S3Fail::NotFound => Error::NotFound("blob".into()),
            f if f.transient() => Error::Busy("blob store unavailable".into()),
            _ => Error::storage("the blob store refused the request"),
        }
    }
}
