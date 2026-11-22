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
