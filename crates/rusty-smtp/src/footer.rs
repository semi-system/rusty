//! The mail footer of a domain (docs/design-footer.md): a text the administrator wants at the end of every message the domain sends (a disclaimer, a company line). It is added by the outbound
//! worker just before DKIM signing, so the signature covers it, and only to the two kinds of message it can be added to without breaking them: a single `text/plain` or `text/html` body.
//! A message of any other shape (multipart, attachments, other encodings) goes out unchanged: a footer that could damage a message is worse than none.

use async_trait::async_trait;
use rusty_core::Result;

/// Where the worker asks for the footer of a sending domain.
#[async_trait]
pub trait FooterSource: Send + Sync {
    /// The footer text of the domain, `None` when it has none.
    async fn footer(&self, domain: &str) -> Result<Option<String>>;
}

/// Most bytes of a footer (the admin web refuses a longer one).
pub const MAX_FOOTER: usize = 2000;

fn header_value(headers: &str, name: &str) -> Option<String> {
    let mut value: Option<String> = None;
    for line in headers.split("\r\n") {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(v) = value.as_mut() {
                v.push(' ');
                v.push_str(line.trim());
            }
            continue;
        }
        if value.is_some() {
            break;
        }
        if let Some((n, v)) = line.split_once(':') {
            if n.trim().eq_ignore_ascii_case(name) {
                value = Some(v.trim().to_owned());
            }
        }
    }
    value
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The message with the footer added, or `None` when the message is not of a shape the footer can be added to (the caller then sends it as it is). The message has CRLF line ends.
pub fn append_footer(message: &[u8], footer: &str) -> Option<Vec<u8>> {
    let footer = footer.trim();
    if footer.is_empty() || footer.len() > MAX_FOOTER || footer.contains('\0') {
        return None;
    }
    let split = message.windows(4).position(|w| w == b"\r\n\r\n")?;
    let (head, body) = (message.get(..split + 2)?, message.get(split + 4..)?);
    let head_text = std::str::from_utf8(head).ok()?;
    let content_type = header_value(head_text, "content-type").unwrap_or_else(|| "text/plain".to_owned()).to_ascii_lowercase();
    let encoding = header_value(head_text, "content-transfer-encoding").unwrap_or_else(|| "7bit".to_owned()).to_ascii_lowercase();
    if !matches!(encoding.as_str(), "7bit" | "8bit") {
        return None;
    }
    let media = content_type.split(';').next().unwrap_or_default().trim().to_owned();
    // A charset the footer's own characters may not fit in is not guessed at: only ASCII text or UTF-8.
    let charset_ok = !content_type.contains("charset")
        || content_type.contains("charset=utf-8")
        || content_type.contains("charset=\"utf-8\"")
        || content_type.contains("charset=us-ascii")
        || content_type.contains("charset=\"us-ascii\"");
    if !charset_ok {
        return None;
    }
    let body_text = std::str::from_utf8(body).ok()?;
    let mut out = head.to_vec();
    out.extend_from_slice(b"\r\n");
    match media.as_str() {
        "text/plain" => {
            out.extend_from_slice(body);
            if !body.ends_with(b"\r\n") && !body.is_empty() {
                out.extend_from_slice(b"\r\n");
            }
            out.extend_from_slice(b"\r\n-- \r\n");
            out.extend_from_slice(footer.replace("\r\n", "\n").replace('\n', "\r\n").as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        "text/html" => {
            let block = format!("<hr><p>{}</p>", html_escape(footer).replace("\r\n", "\n").replace('\n', "<br>\r\n"));
            let joined = match body_text.to_ascii_lowercase().rfind("</body>") {
                Some(i) => format!("{}{}\r\n{}", body_text.get(..i)?, block, body_text.get(i..)?),
                None => format!("{body_text}\r\n{block}\r\n"),
            };
            out.extend_from_slice(joined.as_bytes());
        }
        _ => return None,
    }
    Some(out)
}


/// The footers the administrators keep in the configuration of their domains (`footer_text`).
pub struct DomainFooter {
    directory: rusty_directory::Directory,
    plans: std::sync::Arc<dyn rusty_store::PlanRepo>,
}

impl DomainFooter {
    pub fn new(directory: rusty_directory::Directory, plans: std::sync::Arc<dyn rusty_store::PlanRepo>) -> DomainFooter {
        DomainFooter { directory, plans }
    }
}

#[async_trait]
impl FooterSource for DomainFooter {
    async fn footer(&self, domain: &str) -> Result<Option<String>> {
        let Some(d) = self.directory.find_domain(domain).await? else { return Ok(None) };
        let config: serde_json::Value = serde_json::from_str(&self.plans.domain_config(d.id.0).await?).unwrap_or(serde_json::Value::Null);
        Ok(config.get("footer_text").and_then(|v| v.as_str()).map(str::to_owned))
    }
}

#[cfg(test)]
mod tests {
    use super::append_footer;

    fn text(b: Option<Vec<u8>>) -> String {
        String::from_utf8(b.expect("a footer was added")).unwrap()
    }

    #[test]
    fn a_plain_text_message_gets_the_footer_after_a_signature_separator() {
        let m = b"From: a@one.test\r\nSubject: x\r\n\r\nHello\r\n";
        assert_eq!(text(append_footer(m, "Confidential")), "From: a@one.test\r\nSubject: x\r\n\r\nHello\r\n\r\n-- \r\nConfidential\r\n");
        // No final line end: one is added first.
        assert!(text(append_footer(b"Content-Type: text/plain; charset=utf-8\r\n\r\nHi", "F")).ends_with("Hi\r\n\r\n-- \r\nF\r\n"));
    }

    #[test]
    fn an_html_message_gets_it_before_the_end_of_the_body_and_escaped() {
        let m = b"Content-Type: text/html; charset=UTF-8\r\n\r\n<html><body><p>Hi</p></body></html>\r\n";
        let out = text(append_footer(m, "A & B <c>"));
        assert!(out.contains("<p>Hi</p><hr><p>A &amp; B &lt;c&gt;</p>\r\n</body>"), "{out}");
    }

    #[test]
    fn a_message_of_another_shape_is_left_alone() {
        for m in [
            &b"Content-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\n\r\nHi\r\n--x--\r\n"[..],
            b"Content-Type: text/plain; charset=iso-8859-2\r\n\r\nHi\r\n",
            b"Content-Type: text/plain\r\nContent-Transfer-Encoding: base64\r\n\r\nSGk=\r\n",
            b"Content-Type: image/png\r\n\r\nxx\r\n",
            b"no header end",
        ] {
            assert!(append_footer(m, "F").is_none(), "{}", String::from_utf8_lossy(m));
        }
        assert!(append_footer(b"Subject: x\r\n\r\nHi\r\n", "   ").is_none(), "an empty footer adds nothing");
    }
}
