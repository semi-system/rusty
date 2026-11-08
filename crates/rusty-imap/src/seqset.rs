//! Sequence sets (`1:5,9,12:*`) over the session's message list. The set arrives parsed
//! (`imap_codec::imap_types::sequence::SequenceSet`); this module turns it into index
//! selections without ever materialising the numbers it names: a hostile `1:4294967295` costs
//! one range, and the answer is bounded by the message list it is applied to.
//!
//! `*` is the largest number in use (the message count for sequence numbers, the largest uid for
//! uids); with nothing in the folder it names nothing.

use imap_codec::imap_types::sequence::{SeqOrUid, Sequence, SequenceSet};

fn bound(v: SeqOrUid, largest: u32) -> u32 {
    match v {
        SeqOrUid::Value(n) => n.get(),
        SeqOrUid::Asterisk => largest,
    }
}

/// Closed ranges `(low, high)`, `low <= high`, in input order.
pub fn ranges(set: &SequenceSet, largest: u32) -> Vec<(u32, u32)> {
    set.0
        .as_ref()
        .iter()
        .map(|s| {
            let (a, b) = match s {
                Sequence::Single(a) => (bound(*a, largest), bound(*a, largest)),
                Sequence::Range(a, b) => (bound(*a, largest), bound(*b, largest)),
            };
            (a.min(b), a.max(b))
        })
        .collect()
}

/// Ranges sorted by start and merged where they overlap or touch.
fn merged(set: &SequenceSet, largest: u32) -> Vec<(u32, u32)> {
    let mut r = ranges(set, largest);
    r.sort_unstable();
    let mut out: Vec<(u32, u32)> = Vec::with_capacity(r.len());
    for (lo, hi) in r {
        match out.last_mut() {
            Some((_, h)) if lo <= h.saturating_add(1) => *h = (*h).max(hi),
            _ => out.push((lo, hi)),
        }
    }
    out
}

/// Indexes (0-based, ascending, unique) of the messages whose 1-based position is in `set`,
/// for a folder of `count` messages.
pub fn select_by_seq(set: &SequenceSet, count: usize) -> Vec<usize> {
    let largest = u32::try_from(count).unwrap_or(u32::MAX);
    let mut out = Vec::new();
    for (lo, hi) in merged(set, largest) {
        let lo = lo.max(1) as usize;
        let hi = (hi as usize).min(count);
        if lo <= hi {
            out.extend((lo - 1)..hi);
        }
    }
    out
}

/// Indexes (0-based, ascending, unique) of the messages whose uid is in `set`. `uids` is the
/// folder's uid list in ascending order.
pub fn select_by_uid(set: &SequenceSet, uids: &[u32]) -> Vec<usize> {
    let largest = uids.last().copied().unwrap_or(0);
    let mut out = Vec::new();
    for (lo, hi) in merged(set, largest) {
        let start = uids.partition_point(|u| *u < lo);
        let end = uids.partition_point(|u| *u <= hi);
        if start < end {
            out.extend(start..end);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::TryFrom;

    fn set(s: &str) -> SequenceSet {
        SequenceSet::try_from(s).unwrap()
    }

    #[test]
    fn sequence_numbers_follow_ranges_in_any_direction_and_star() {
        assert_eq!(select_by_seq(&set("1"), 5), vec![0]);
        assert_eq!(select_by_seq(&set("2:4"), 5), vec![1, 2, 3]);
        assert_eq!(select_by_seq(&set("4:2"), 5), vec![1, 2, 3], "a reversed range is the same range");
        assert_eq!(select_by_seq(&set("1:*"), 3), vec![0, 1, 2]);
        assert_eq!(select_by_seq(&set("*"), 3), vec![2]);
        assert_eq!(select_by_seq(&set("2,2,1:2"), 5), vec![0, 1], "overlaps are merged");
        assert_eq!(select_by_seq(&set("4:9"), 5), vec![3, 4], "numbers past the end are clamped away");
        assert_eq!(select_by_seq(&set("9"), 5), Vec::<usize>::new());
        assert_eq!(select_by_seq(&set("1:*"), 0), Vec::<usize>::new());
    }

    #[test]
    fn uid_sets_pick_existing_uids_only() {
        let uids = [3, 4, 9, 10, 40];
        assert_eq!(select_by_uid(&set("4:10"), &uids), vec![1, 2, 3]);
        assert_eq!(select_by_uid(&set("5:8"), &uids), Vec::<usize>::new());
        assert_eq!(select_by_uid(&set("40:*"), &uids), vec![4]);
        assert_eq!(select_by_uid(&set("*"), &uids), vec![4]);
        assert_eq!(select_by_uid(&set("999999"), &uids), Vec::<usize>::new());
        assert_eq!(select_by_uid(&set("1:*"), &[]), Vec::<usize>::new());
        assert_eq!(select_by_uid(&set("1:4294967295"), &uids), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn a_huge_range_does_not_expand() {
        let r = ranges(&set("1:4294967295,5:*"), 10);
        assert_eq!(r, vec![(1, u32::MAX), (5, 10)]);
    }
}
