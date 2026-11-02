//! Envelope extraction from raw RFC 5322 messages, wrapping `mail-parser`.

use mail_parser::{Address, HeaderValue, MessageParser};
use rusty_core::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub subject: String,
    pub from: String,
    pub to: String,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub date: Option<i64>,
}

fn join_addresses(addr: Option<&Address<'_>>) -> String {
    addr.map(|a| {
        a.iter()
            .filter_map(|x| x.address.as_deref())
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
    })
    .unwrap_or_default()
}

fn id_list(v: &HeaderValue<'_>) -> Vec<String> {
    match v {
        HeaderValue::Text(t) => vec![t.to_string()],
        HeaderValue::TextList(l) => l.iter().map(|s| s.to_string()).collect(),
        _ => Vec::new(),
    }
}

/// Parse the envelope fields of a raw message. Empty input is `Error::Invalid`;
/// malformed or missing headers yield empty strings / `None`.
pub fn parse_envelope(raw: &[u8]) -> Result<Envelope> {
    if raw.is_empty() {
        return Err(Error::Invalid("empty message".into()));
    }
    let Some(msg) = MessageParser::default().parse(raw) else {
        return Ok(Envelope {
            subject: String::new(),
            from: String::new(),
            to: String::new(),
            message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            date: None,
        });
    };
    Ok(Envelope {
        subject: msg.subject().unwrap_or_default().to_string(),
        from: join_addresses(msg.from()),
        to: join_addresses(msg.to()),
        message_id: msg.message_id().map(str::to_string),
        in_reply_to: id_list(msg.in_reply_to()).into_iter().next(),
        references: id_list(msg.references()),
        date: msg.date().map(|d| d.to_timestamp()),
    })
}
