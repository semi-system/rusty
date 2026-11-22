//! The sender policy of the recipients (docs/design-sender-policy.md): the allow and block lists the administrators keep in the configuration layers (`sender_allow`, `sender_block`: addresses
//! or domains, per domain, per plan or per account). It runs before the scanners: a sender every recipient allows skips them, a sender every recipient blocks is refused at the end of DATA.
//! Some recipients blocking and others not delivers (a message cannot be refused for some of its recipients after DATA); the lists are about unwanted senders, not secrecy.

use async_trait::async_trait;
use rusty_directory::Directory;

use crate::filter::{FilterContext, FilterVerdict, InboundFilter};

/// What a pair of lists says about a sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    Allowed,
    Blocked,
    Unknown,
}

/// Does the list entry name the sender? An address (`a@b.example`) names exactly that sender; a domain (`b.example`) names it and its subdomains. Case does not matter.
pub fn entry_matches(entry: &str, sender: &str) -> bool {
    let (entry, sender) = (entry.trim().to_ascii_lowercase(), sender.trim().to_ascii_lowercase());
    if entry.is_empty() || sender.is_empty() {
        return false;
    }
    if entry.contains('@') {
        return entry == sender;
    }
    let Some((_, domain)) = sender.rsplit_once('@') else { return false };
    domain == entry || domain.strip_suffix(entry.as_str()).is_some_and(|rest| rest.ends_with('.'))
}

/// The standing of `sender` under these lists: the allow list wins over the block list (an explicit allow is the administrator's later word).
pub fn standing(allow: &[String], block: &[String], sender: &str) -> Standing {
    if allow.iter().any(|e| entry_matches(e, sender)) {
        Standing::Allowed
    } else if block.iter().any(|e| entry_matches(e, sender)) {
        Standing::Blocked
    } else {
        Standing::Unknown
    }
}

fn list(v: Option<&serde_json::Value>) -> Vec<String> {
    v.and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

pub struct SenderPolicy {
    directory: Directory,
}

impl SenderPolicy {
    pub fn new(directory: Directory) -> SenderPolicy {
        SenderPolicy { directory }
    }
}

#[async_trait]
impl InboundFilter for SenderPolicy {
    async fn check(&self, ctx: &FilterContext<'_>) -> FilterVerdict {
        if ctx.mail_from.is_empty() {
            return FilterVerdict::Accept;
        }
        let (mut allowed, mut blocked, mut total) = (0usize, 0usize, 0usize);
        for r in ctx.recipients {
            for account in &r.accounts {
                total += 1;
                // A database that cannot answer leaves the sender unknown: the scanners still run.
                let Ok(values) = self.directory.effective_configs(*account, &["sender_allow", "sender_block"]).await else { continue };
                match standing(&list(values.first().and_then(Option::as_ref)), &list(values.get(1).and_then(Option::as_ref)), ctx.mail_from) {
                    Standing::Allowed => allowed += 1,
                    Standing::Blocked => blocked += 1,
                    Standing::Unknown => {}
                }
            }
        }
        if total > 0 && allowed == total {
            FilterVerdict::Bypass
        } else if total > 0 && blocked == total {
            FilterVerdict::Reject { code: 550, enhanced: "5.7.1", text: "Sender blocked by the recipient's policy".into() }
        } else {
            FilterVerdict::Accept
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{entry_matches, standing, Standing};

    #[test]
    fn an_address_names_one_sender_and_a_domain_names_it_and_its_subdomains() {
        assert!(entry_matches("Bob@Far.example", "bob@far.example"));
        assert!(!entry_matches("bob@far.example", "rob@far.example"));
        assert!(entry_matches("far.example", "x@far.example"));
        assert!(entry_matches("far.example", "x@mail.far.example"));
        assert!(!entry_matches("far.example", "x@notfar.example"), "a suffix that is not a whole label does not match");
        assert!(!entry_matches("far.example", "x@far.example.evil"));
        assert!(!entry_matches("", "x@far.example"));
        assert!(!entry_matches("far.example", ""));
    }

    #[test]
    fn an_allowed_sender_is_allowed_even_when_a_wider_block_names_it() {
        let allow = vec!["boss@far.example".to_owned()];
        let block = vec!["far.example".to_owned()];
        assert_eq!(standing(&allow, &block, "boss@far.example"), Standing::Allowed);
        assert_eq!(standing(&allow, &block, "spam@far.example"), Standing::Blocked);
        assert_eq!(standing(&allow, &block, "x@other.example"), Standing::Unknown);
    }
}

// ------------------------------------------------------------------------------------------------ attachment blocking

/// The file names that parts of a message give themselves (`filename=` of `Content-Disposition`, `name=` of `Content-Type`), found on the header lines of the parts. A line of a body is not
/// looked at: only a line that starts a header of a part or continues one can name a file.
pub fn attachment_names(data: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(data);
    let mut names = Vec::new();
    let mut in_header = true;
    let mut header_has_disposition = false;
    for raw in text.split('\n') {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() {
            in_header = false;
            header_has_disposition = false;
            continue;
        }
        // A boundary line starts the headers of the next part.
        if line.starts_with("--") {
            in_header = true;
            header_has_disposition = false;
            continue;
        }
        let lower = line.to_ascii_lowercase();
        let continuation = line.starts_with(' ') || line.starts_with('\t');
        if !in_header {
            continue;
        }
        if lower.starts_with("content-disposition:") || lower.starts_with("content-type:") {
            header_has_disposition = true;
        } else if !continuation {
            header_has_disposition = false;
        }
        if header_has_disposition {
            for key in ["filename*=", "filename=", "name="] {
                let mut from = 0usize;
                while let Some(at) = lower.get(from..).and_then(|s| s.find(key)) {
                    let start = from + at;
                    // `name=` must be a parameter of its own, not the end of `filename=`.
                    let before_ok = start == 0 || lower.get(..start).is_some_and(|b| b.ends_with([';', ' ', '\t']));
                    if before_ok {
                        let value = line.get(start + key.len()..).unwrap_or_default();
                        let value = match value.strip_prefix('"') {
                            Some(q) => q.split('"').next().unwrap_or_default(),
                            None => value.split([';', ' ', '\t']).next().unwrap_or_default(),
                        };
                        if !value.is_empty() {
                            names.push(value.to_owned());
                        }
                    }
                    from = start + key.len();
                }
            }
        }
    }
    names
}

/// The extension of a file name in lower case, without the dot (`Invoice.PDF.exe` is `exe`); empty when it has none.
fn extension(name: &str) -> String {
    let name = name.trim().trim_end_matches('.');
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

/// Does any file name of the message end in a blocked extension? Returns the first.
pub fn blocked_attachment<'a>(data: &[u8], blocked: &'a [String]) -> Option<&'a String> {
    attachment_names(data).iter().find_map(|n| {
        let ext = extension(n);
        blocked.iter().find(|b| b.eq_ignore_ascii_case(&ext))
    })
}

/// Refuses, at the end of DATA, a message with an attachment of a kind the platform or the domain blocks (the key `blocked_extensions` of the configuration layers: extensions without a dot).
/// The first recipient's account decides: after DATA one message has one fate for all its recipients.
pub struct AttachmentPolicy(pub Directory);

#[async_trait]
impl InboundFilter for AttachmentPolicy {
    async fn check(&self, ctx: &FilterContext<'_>) -> FilterVerdict {
        let Some(first) = ctx.recipients.iter().flat_map(|r| r.accounts.iter()).next() else { return FilterVerdict::Accept };
        let Ok(Some(value)) = self.0.effective_config(*first, "blocked_extensions").await else { return FilterVerdict::Accept };
        let blocked: Vec<String> = value.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default();
        if blocked.is_empty() {
            return FilterVerdict::Accept;
        }
        match blocked_attachment(ctx.data, &blocked) {
            // The sender learns the kind, which is the point of refusing (a person who attached it can send it another way); the file name is not echoed.
            Some(ext) => FilterVerdict::Reject { code: 554, enhanced: "5.7.1", text: format!("Attachments of type .{ext} are not accepted here") },
            None => FilterVerdict::Accept,
        }
    }
}

#[cfg(test)]
mod attachment_tests {
    use super::{attachment_names, blocked_attachment};

    const MIXED: &str = "From: a@far.example\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nSee the file. name=notafile.exe\r\n--b\r\nContent-Type: application/octet-stream; name=\"Invoice.PDF.exe\"\r\nContent-Disposition: attachment;\r\n filename=\"Invoice.PDF.exe\"\r\n\r\nTVqQAAMAAAAEAAAA\r\n--b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=report.pdf\r\n\r\nJVBERi0x\r\n--b--\r\n";

    #[test]
    fn the_file_names_of_the_parts_are_read_from_their_headers_only() {
        let names = attachment_names(MIXED.as_bytes());
        assert!(names.contains(&"Invoice.PDF.exe".to_owned()) && names.contains(&"report.pdf".to_owned()), "{names:?}");
        assert!(!names.iter().any(|n| n.contains("notafile")), "a body line names nothing: {names:?}");
    }

    #[test]
    fn the_last_extension_decides_and_case_does_not_matter() {
        let blocked = vec!["exe".to_owned(), "SCR".to_owned()];
        assert_eq!(blocked_attachment(MIXED.as_bytes(), &blocked).map(String::as_str), Some("exe"));
        assert!(blocked_attachment(MIXED.as_bytes(), &["pdf".to_owned()]).is_some());
        assert!(blocked_attachment(MIXED.as_bytes(), &["docx".to_owned()]).is_none());
        assert!(blocked_attachment(b"Subject: x\r\n\r\nhello.exe is not attached\r\n", &blocked).is_none());
    }
}
