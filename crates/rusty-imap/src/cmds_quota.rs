//! QUOTA (RFC 9208): `GETQUOTA` and `GETQUOTAROOT`. A mailbox has one quota root, the empty name, whose only resource is `STORAGE` in units of 1024 octets: what the mailbox holds (the
//! `used_bytes` the database keeps) and the limit that applies to the account (its own, its plan's, its domain's or the global one; docs/design-plans-quota.md). With no limit the root does
//! not exist: `GETQUOTAROOT` names none and `GETQUOTA` answers `NONEXISTENT`. Only `GETQUOTA`/`GETQUOTAROOT` are offered: a client cannot set a quota.

use crate::out::{Closed, Out};
use crate::session::{Done, Session};
use crate::wire;

/// Bytes to the unit of the `STORAGE` resource, rounded up so that a mailbox with anything in it never shows 0.
fn kib(bytes: u64) -> u64 {
    bytes.div_ceil(1024)
}

impl Session {
    /// The quota line of the signed-in mailbox, or `None` when there is no limit. `Err` is the answer to send.
    async fn quota_line(&self) -> Result<Option<String>, Done> {
        let Some(mb) = self.mailbox() else { return Err(Done::bad("Not authenticated")) };
        let Some(limit) = mb.limits().quota_bytes else { return Ok(None) };
        let used = match self.ctx.plans.mailbox_used(mb.id().0).await {
            Ok(u) => u64::try_from(u).unwrap_or(0),
            Err(e) => return Err(Done::store(crate::errors::Op::Mailbox, &e)),
        };
        Ok(Some(format!("QUOTA \"\" (STORAGE {} {})", kib(used), kib(limit))))
    }

    pub(crate) async fn cmd_getquota(&mut self, root: &str, out: &mut Out<'_>) -> Result<Done, Closed> {
        let line = match self.quota_line().await {
            Ok(l) => l,
            Err(d) => return Ok(d),
        };
        match (root, line) {
            ("", Some(line)) => {
                out.untagged(&line);
                Ok(Done::ok("GETQUOTA completed"))
            }
            _ => Ok(Done::no("NONEXISTENT", "No such quota root")),
        }
    }

    pub(crate) async fn cmd_getquotaroot(&mut self, name: &str, out: &mut Out<'_>) -> Result<Done, Closed> {
        let folders = match self.folders().await {
            Ok(f) => f,
            Err(d) => return Ok(d),
        };
        match self.resolve_name(&folders, name) {
            Ok(Some(_)) => {}
            Ok(None) => return Ok(Done::no("NONEXISTENT", "Mailbox does not exist")),
            Err(d) => return Ok(d),
        }
        let line = match self.quota_line().await {
            Ok(l) => l,
            Err(d) => return Ok(d),
        };
        let mut head = b"* QUOTAROOT ".to_vec();
        wire::mailbox(&mut head, name);
        if line.is_some() {
            head.extend_from_slice(b" \"\"");
        }
        head.extend_from_slice(b"\r\n");
        out.raw(&head);
        if let Some(line) = line {
            out.untagged(&line);
        }
        Ok(Done::ok("GETQUOTAROOT completed"))
    }
}

#[cfg(test)]
mod tests {
    use super::kib;

    #[test]
    fn the_storage_resource_is_in_kibibytes_rounded_up() {
        assert_eq!((kib(0), kib(1), kib(1024), kib(1025)), (0, 1, 1, 2));
    }
}
