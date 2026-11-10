//! The one mapping table from store errors to IMAP status lines (plan section 1b rule 4). The
//! texts are fixed: nothing a client sent and nothing from the backend is ever echoed.

use rusty_core::Error;

/// A tagged status line without its tag: `NO [CODE] text`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub code: Option<&'static str>,
    pub text: &'static str,
}

/// What a command was doing when the store failed; it decides the response code of `NotFound`
/// and `Conflict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Anything that names a mailbox (folder).
    Mailbox,
    /// CREATE and RENAME targets.
    Create,
    /// Operations on messages.
    Message,
    /// APPEND and COPY into a folder.
    Store,
}

/// `NO` status for a store error. `Busy` is retryable (`[UNAVAILABLE]`), `Quota` is a limit,
/// `Invalid` and `Conflict` are the client's request, anything else is an internal error that is
/// never described.
pub fn status_for(op: Op, e: &Error) -> Status {
    match e {
        Error::Busy(_) => Status { code: Some("UNAVAILABLE"), text: "Temporary failure, try again later" },
        Error::Quota(_) => Status { code: Some("LIMIT"), text: "Limit exceeded" },
        Error::Invalid(_) => Status { code: Some("CANNOT"), text: "Not allowed" },
        Error::NotFound(_) => match op {
            Op::Mailbox => Status { code: Some("NONEXISTENT"), text: "Mailbox does not exist" },
            Op::Store => Status { code: Some("TRYCREATE"), text: "Mailbox does not exist" },
            Op::Create | Op::Message => Status { code: Some("NONEXISTENT"), text: "Does not exist" },
        },
        Error::Conflict(_) => match op {
            Op::Create => Status { code: Some("ALREADYEXISTS"), text: "Mailbox already exists" },
            _ => Status { code: Some("INUSE"), text: "In use, try again" },
        },
        Error::Storage { .. } => Status { code: Some("SERVERBUG"), text: "Internal error" },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(op: Op, e: Error) -> (Option<&'static str>, &'static str) {
        let st = status_for(op, &e);
        (st.code, st.text)
    }

    #[test]
    fn every_variant_has_a_fixed_status() {
        assert_eq!(s(Op::Mailbox, Error::Busy("x".into())).0, Some("UNAVAILABLE"));
        assert_eq!(s(Op::Store, Error::Quota("x".into())).0, Some("LIMIT"));
        assert_eq!(s(Op::Mailbox, Error::Invalid("x".into())).0, Some("CANNOT"));
        assert_eq!(s(Op::Mailbox, Error::NotFound("x".into())).0, Some("NONEXISTENT"));
        assert_eq!(s(Op::Store, Error::NotFound("x".into())).0, Some("TRYCREATE"));
        assert_eq!(s(Op::Create, Error::Conflict("x".into())).0, Some("ALREADYEXISTS"));
        assert_eq!(s(Op::Message, Error::Conflict("x".into())).0, Some("INUSE"));
        assert_eq!(s(Op::Message, Error::storage("disk on fire")).0, Some("SERVERBUG"));
    }

    #[test]
    fn texts_never_echo_the_error() {
        let secret = "hunter2 /var/lib/rusty/secret.db";
        for e in [
            Error::Busy(secret.into()),
            Error::Quota(secret.into()),
            Error::Invalid(secret.into()),
            Error::NotFound(secret.into()),
            Error::Conflict(secret.into()),
            Error::storage(secret),
            Error::storage_with(secret, std::io::Error::other(secret)),
        ] {
            for op in [Op::Mailbox, Op::Create, Op::Message, Op::Store] {
                let st = status_for(op, &e);
                assert!(!st.text.contains("hunter2") && !st.text.contains("/var"), "{st:?}");
            }
        }
    }
}
