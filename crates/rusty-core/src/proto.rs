//! Protocol version negotiation between Rusty nodes.
//!
//! Each side advertises an inclusive range of protocol versions it speaks; both pick the
//! highest version in the intersection. Version 0 is reserved (never valid). Error messages
//! are fixed strings and never echo the peer's numbers.

use crate::{Error, Result};

/// The protocol version this build speaks natively.
pub const PROTOCOL_VERSION: u16 = 1;

/// The range of protocol versions this build accepts.
pub const SUPPORTED: VersionRange = VersionRange {
    min: PROTOCOL_VERSION,
    max: PROTOCOL_VERSION,
};

/// An inclusive range of protocol versions, `1 <= min <= max`.
///
/// The fields are public for reading; build values with [`VersionRange::new`]. A value built
/// by hand with `min == 0` or `min > max` is rejected by [`negotiate`] as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionRange {
    pub min: u16,
    pub max: u16,
}

impl VersionRange {
    /// `Error::Invalid` if `min == 0` or `min > max`.
    pub fn new(min: u16, max: u16) -> Result<VersionRange> {
        let r = VersionRange { min, max };
        r.check()?;
        Ok(r)
    }

    fn check(&self) -> Result<()> {
        if self.min == 0 || self.min > self.max {
            return Err(Error::Invalid("invalid protocol version range".into()));
        }
        Ok(())
    }
}

/// The highest version both ranges contain, or `Error::Invalid("protocol version mismatch")`
/// if they do not overlap (or either range is malformed). Symmetric in its arguments.
pub fn negotiate(ours: VersionRange, peer: VersionRange) -> Result<u16> {
    ours.check()?;
    peer.check()?;
    let lo = ours.min.max(peer.min);
    let hi = ours.max.min(peer.max);
    if lo > hi {
        return Err(Error::Invalid("protocol version mismatch".into()));
    }
    Ok(hi)
}
