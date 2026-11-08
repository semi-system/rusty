//! Which listeners a configuration asks for, and how each one accepts connections.

use rusty_core::config::StartupConfig;

use crate::listener::ListenerSpec;

/// The five service ports of the all-in-one server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerKind {
    /// Inbound mail from other servers; never offers AUTH.
    Smtp25,
    /// Submission with STARTTLS.
    Submission587,
    /// Submission over implicit TLS.
    Submissions465,
    /// IMAP with STARTTLS.
    Imap143,
    /// IMAP over implicit TLS.
    Imaps993,
}

impl ListenerKind {
    pub fn name(self) -> &'static str {
        match self {
            ListenerKind::Smtp25 => "smtp-25",
            ListenerKind::Submission587 => "submission-587",
            ListenerKind::Submissions465 => "submissions-465",
            ListenerKind::Imap143 => "imap-143",
            ListenerKind::Imaps993 => "imaps-993",
        }
    }

    /// The TLS handshake happens before the first byte of the protocol.
    pub fn implicit_tls(self) -> bool {
        matches!(self, ListenerKind::Submissions465 | ListenerKind::Imaps993)
    }

    /// The service accepts credentials on this port.
    pub fn accepts_auth(self) -> bool {
        !matches!(self, ListenerKind::Smtp25)
    }

    pub fn is_imap(self) -> bool {
        matches!(self, ListenerKind::Imap143 | ListenerKind::Imaps993)
    }
}

#[derive(Debug, Clone)]
pub struct PlannedListener {
    pub kind: ListenerKind,
    pub spec: ListenerSpec,
}

/// The enabled listeners of `cfg` (a listener set to the empty string is absent).
pub fn listener_plan(cfg: &StartupConfig) -> Vec<PlannedListener> {
    let smtp = |kind, listen: rusty_core::config::Listen| {
        listen.addr().map(|addr| PlannedListener {
            kind,
            spec: ListenerSpec {
                name: kind.name(),
                addr,
                max_connections: cfg.smtp.max_connections,
                max_per_ip: cfg.smtp.max_connections_per_ip,
            },
        })
    };
    let imap = |kind, listen: rusty_core::config::Listen| {
        listen.addr().map(|addr| PlannedListener {
            kind,
            spec: ListenerSpec {
                name: kind.name(),
                addr,
                max_connections: cfg.imap.max_connections,
                max_per_ip: cfg.imap.max_connections_per_ip,
            },
        })
    };
    [
        smtp(ListenerKind::Smtp25, cfg.smtp.listen_25),
        smtp(ListenerKind::Submission587, cfg.smtp.listen_587),
        smtp(ListenerKind::Submissions465, cfg.smtp.listen_465),
        imap(ListenerKind::Imap143, cfg.imap.listen_143),
        imap(ListenerKind::Imaps993, cfg.imap.listen_993),
    ]
    .into_iter()
    .flatten()
    .collect()
}
