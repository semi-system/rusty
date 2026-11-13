//! Drift detection for published DNS records (plan U10, "Persistence for later re-validation"):
//! a pure state machine, no I/O and no clock reads. The store applies [`step`] inside its own
//! transaction with the database clock; the validation engine produces the [`Observation`]s.
//!
//! The model, per record key `(name, type, purpose)`:
//!
//! * A status change is an event (`ok -> missing` at time T); a repeated status is not.
//! * `Missing` and `Mismatch` are failures and count in `consecutive_failures`; `Ok` resets the
//!   counter; `Warning` (a check that could not be decided) and `Pending` (propagation lag) are
//!   neutral: they leave the counter alone.
//! * An alert is raised when a record that was `ok` at least once has failed `alert_after`
//!   times in a row, so a record that was never published (or a propagation blip) never alerts.
//!   The alert stays while the record is neutral or failing and is cleared only by `Ok` (or by a
//!   changed expectation).
//! * A changed `expected_hash` (key rotation, new outbound IP, edited selector) starts the
//!   record's history over: what was once `ok` was `ok` for another value, so the new value
//!   cannot "drift" before it was ever seen published.

/// Longest `seen` value kept as evidence, in bytes.
pub const MAX_SEEN_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordStatus {
    Ok,
    Missing,
    Mismatch,
    Warning,
    Pending,
}

impl RecordStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RecordStatus::Ok => "ok",
            RecordStatus::Missing => "missing",
            RecordStatus::Mismatch => "mismatch",
            RecordStatus::Warning => "warning",
            RecordStatus::Pending => "pending",
        }
    }

    pub fn parse(s: &str) -> Option<RecordStatus> {
        Some(match s {
            "ok" => RecordStatus::Ok,
            "missing" => RecordStatus::Missing,
            "mismatch" => RecordStatus::Mismatch,
            "warning" => RecordStatus::Warning,
            "pending" => RecordStatus::Pending,
            _ => return None,
        })
    }

    /// `Missing` and `Mismatch`.
    pub fn is_failure(self) -> bool {
        matches!(self, RecordStatus::Missing | RecordStatus::Mismatch)
    }
}

/// Identity of one record of a domain's expected set.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RecordKey {
    /// Fully qualified, lower case, with the trailing dot.
    pub name: String,
    /// `MX`, `TXT`, ... (the `RecordType::as_str` spelling).
    pub rtype: String,
    pub purpose: String,
}

/// The stored state of one record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateRow {
    pub key: RecordKey,
    pub expected_hash: String,
    pub status: RecordStatus,
    pub seen: Option<String>,
    pub first_ok_at: Option<i64>,
    pub last_ok_at: Option<i64>,
    pub last_checked_at: i64,
    pub status_since: i64,
    pub consecutive_failures: u32,
}

/// What one check saw for one record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub key: RecordKey,
    pub expected_hash: String,
    pub status: RecordStatus,
    pub seen: Option<String>,
}

/// A status change, the unit of the history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusChange {
    pub key: RecordKey,
    /// `None`: the first observation of the record (or of a new expectation).
    pub from: Option<RecordStatus>,
    pub to: RecordStatus,
    pub at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertChange {
    None,
    Raised,
    Cleared,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub row: StateRow,
    pub change: Option<StatusChange>,
    pub alert: AlertChange,
    /// The expectation differs from the stored one: the history was started over.
    pub expectation_changed: bool,
}

/// Is `row` in the alert state (a record that was `ok` once and has failed `alert_after` times in
/// a row, and has not been `ok` since)?
pub fn is_alerting(row: &StateRow, alert_after: u32) -> bool {
    row.first_ok_at.is_some() && row.status != RecordStatus::Ok && row.consecutive_failures >= alert_after.max(1)
}

/// Cut `s` to [`MAX_SEEN_BYTES`] on a character boundary and drop control characters.
pub fn clean_seen(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().filter(|c| !c.is_control()) {
        if out.len() + c.len_utf8() > MAX_SEEN_BYTES {
            break;
        }
        out.push(c);
    }
    out
}

/// Apply one observation. `alert_after` is at least 1 (0 is treated as 1).
pub fn step(prev: Option<&StateRow>, obs: &Observation, now: i64, alert_after: u32) -> Step {
    let was_alerting = prev.is_some_and(|p| is_alerting(p, alert_after));
    let expectation_changed = prev.is_some_and(|p| p.expected_hash != obs.expected_hash);
    // A changed expectation starts over: nothing of the old history carries across.
    let base = if expectation_changed { None } else { prev };
    let ok = obs.status == RecordStatus::Ok;
    let failures = match (obs.status, base) {
        (RecordStatus::Ok, _) => 0,
        (s, Some(b)) if s.is_failure() => b.consecutive_failures.saturating_add(1),
        (s, None) if s.is_failure() => 1,
        (_, Some(b)) => b.consecutive_failures,
        (_, None) => 0,
    };
    let status_since = match prev {
        Some(p) if p.status == obs.status && !expectation_changed => p.status_since,
        _ => now,
    };
    let row = StateRow {
        key: obs.key.clone(),
        expected_hash: obs.expected_hash.clone(),
        status: obs.status,
        seen: obs.seen.as_deref().map(clean_seen),
        first_ok_at: base.and_then(|b| b.first_ok_at).or(ok.then_some(now)),
        last_ok_at: if ok { Some(now) } else { base.and_then(|b| b.last_ok_at) },
        last_checked_at: now,
        status_since,
        consecutive_failures: failures,
    };
    let from = prev.map(|p| p.status);
    let change = if from == Some(obs.status) {
        None
    } else {
        Some(StatusChange { key: obs.key.clone(), from, to: obs.status, at: now })
    };
    let now_alerting = is_alerting(&row, alert_after);
    let alert = match (was_alerting, now_alerting) {
        (false, true) => AlertChange::Raised,
        (true, false) => AlertChange::Cleared,
        _ => AlertChange::None,
    };
    Step { row, change, alert, expectation_changed }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn key() -> RecordKey {
        RecordKey { name: "rusty._domainkey.example.com.".into(), rtype: "TXT".into(), purpose: "dkim".into() }
    }

    fn obs(status: RecordStatus, hash: &str) -> Observation {
        Observation { key: key(), expected_hash: hash.into(), status, seen: Some("v=DKIM1".into()) }
    }

    /// Feed `statuses` at t = 100, 200, ... and collect every step.
    fn run(statuses: &[RecordStatus], n: u32) -> Vec<Step> {
        let mut prev: Option<StateRow> = None;
        let mut out = Vec::new();
        for (i, s) in statuses.iter().enumerate() {
            let st = step(prev.as_ref(), &obs(*s, "h1"), 100 * (i as i64 + 1), n);
            prev = Some(st.row.clone());
            out.push(st);
        }
        out
    }

    use RecordStatus::*;

    #[test]
    fn first_observation_is_recorded_without_alert() {
        let s = &run(&[Missing], 1)[0];
        assert_eq!(s.row.consecutive_failures, 1);
        assert_eq!(s.row.first_ok_at, None);
        assert_eq!(s.change.as_ref().map(|c| (c.from, c.to)), Some((None, Missing)));
        assert_eq!(s.alert, AlertChange::None, "never ok: nothing was lost");
    }

    #[test]
    fn deleted_record_gives_one_event_and_an_alert_after_n_failures() {
        let steps = run(&[Ok, Ok, Missing, Missing, Missing, Missing], 3);
        let changes: Vec<_> = steps.iter().filter_map(|s| s.change.clone()).collect();
        let ok_to_missing: Vec<_> = changes.iter().filter(|c| c.from == Some(Ok) && c.to == Missing).collect();
        assert_eq!(ok_to_missing.len(), 1, "exactly one ok -> missing event: {changes:?}");
        assert_eq!(ok_to_missing[0].at, 300);
        let alerts: Vec<_> = steps.iter().map(|s| s.alert).collect();
        assert_eq!(
            alerts,
            [AlertChange::None, AlertChange::None, AlertChange::None, AlertChange::None, AlertChange::Raised, AlertChange::None]
        );
        assert!(is_alerting(&steps[5].row, 3));
        assert!(!is_alerting(&steps[3].row, 3), "2 failures of 3");
    }

    #[test]
    fn restoring_the_record_clears_the_alert() {
        let steps = run(&[Ok, Missing, Missing, Ok], 2);
        assert_eq!(steps[2].alert, AlertChange::Raised);
        assert_eq!(steps[3].alert, AlertChange::Cleared);
        assert_eq!(steps[3].row.consecutive_failures, 0);
        assert_eq!(steps[3].row.last_ok_at, Some(400));
        assert_eq!(steps[3].row.first_ok_at, Some(100));
        assert!(!is_alerting(&steps[3].row, 2));
    }

    #[test]
    fn a_propagation_blip_does_not_alert() {
        let steps = run(&[Ok, Missing, Ok, Missing, Ok, Missing], 2);
        assert!(steps.iter().all(|s| s.alert == AlertChange::None));
    }

    #[test]
    fn pending_and_warning_are_neutral() {
        // ok, missing (1 failure), pending, warning, missing (2 failures: alert)
        let steps = run(&[Ok, Missing, Pending, Warning, Missing], 2);
        assert_eq!(steps[2].row.consecutive_failures, 1);
        assert_eq!(steps[3].row.consecutive_failures, 1);
        assert_eq!(steps[4].alert, AlertChange::Raised);
        // and the alert survives a neutral observation
        let steps = run(&[Ok, Missing, Missing, Warning, Pending], 2);
        assert_eq!(steps[2].alert, AlertChange::Raised);
        assert!(steps[3].alert == AlertChange::None && is_alerting(&steps[3].row, 2));
        assert!(steps[4].alert == AlertChange::None && is_alerting(&steps[4].row, 2));
    }

    #[test]
    fn never_ok_never_alerts() {
        let steps = run(&[Missing, Missing, Mismatch, Missing, Missing], 1);
        assert!(steps.iter().all(|s| s.alert == AlertChange::None));
    }

    #[test]
    fn repeated_status_is_not_an_event() {
        let steps = run(&[Ok, Ok, Ok], 2);
        assert_eq!(steps.iter().filter(|s| s.change.is_some()).count(), 1, "only the first observation");
        assert_eq!(steps[2].row.status_since, 100);
        assert_eq!(steps[2].row.last_checked_at, 300);
    }

    #[test]
    fn key_rotation_starts_the_history_over() {
        let mut prev = None;
        for (i, (s, h)) in [(Ok, "h1"), (Ok, "h1"), (Mismatch, "h2"), (Mismatch, "h2")].iter().enumerate() {
            let st = step(prev.as_ref(), &obs(*s, h), 100 * (i as i64 + 1), 2);
            if i == 2 {
                assert!(st.expectation_changed);
                assert_eq!(st.row.expected_hash, "h2");
                assert_eq!(st.row.first_ok_at, None, "the new value was never seen published");
                assert_eq!(st.row.consecutive_failures, 1);
                assert_eq!(st.change.as_ref().map(|c| (c.from, c.to)), Some((Some(Ok), Mismatch)));
            }
            if i == 3 {
                assert_eq!(st.alert, AlertChange::None, "not a deleted record: the new key is not published yet");
            }
            prev = Some(st.row);
        }
    }

    #[test]
    fn rotation_while_alerting_clears_the_alert() {
        let steps = run(&[Ok, Missing, Missing], 2);
        assert!(is_alerting(&steps[2].row, 2));
        let st = step(Some(&steps[2].row), &obs(Missing, "h2"), 999, 2);
        assert_eq!(st.alert, AlertChange::Cleared);
        assert!(!is_alerting(&st.row, 2));
    }

    #[test]
    fn seen_is_cleaned_and_cut() {
        let long = format!("a\u{0}b\n{}", "é".repeat(400));
        let c = clean_seen(&long);
        assert!(c.len() <= MAX_SEEN_BYTES && c.starts_with("ab"));
        assert!(!c.chars().any(char::is_control));
        let mut o = obs(Ok, "h");
        o.seen = Some(long);
        assert!(step(None, &o, 1, 1).row.seen.unwrap().len() <= MAX_SEEN_BYTES);
    }

    #[test]
    fn zero_alert_after_means_one() {
        let steps = run(&[Ok, Missing], 0);
        assert_eq!(steps[1].alert, AlertChange::Raised);
    }

    fn any_status() -> impl Strategy<Value = RecordStatus> {
        prop_oneof![Just(Ok), Just(Missing), Just(Mismatch), Just(Warning), Just(Pending)]
    }

    proptest! {
        #[test]
        fn invariants_hold_for_any_sequence(seq in proptest::collection::vec(any_status(), 1..60), n in 1u32..6) {
            let steps = run(&seq, n);
            let mut raised = false;
            let mut changes = 0;
            let mut prev_status: Option<RecordStatus> = None;
            for (i, st) in steps.iter().enumerate() {
                // events exactly at status changes
                let changed = prev_status != Some(seq[i]);
                prop_assert_eq!(st.change.is_some(), changed);
                changes += usize::from(changed);
                prev_status = Some(seq[i]);
                // counter semantics
                if seq[i] == Ok { prop_assert_eq!(st.row.consecutive_failures, 0); }
                // alert state machine: Raised and Cleared alternate, starting with Raised
                match st.alert {
                    AlertChange::Raised => { prop_assert!(!raised); raised = true; }
                    AlertChange::Cleared => { prop_assert!(raised); raised = false; }
                    AlertChange::None => {}
                }
                prop_assert_eq!(is_alerting(&st.row, n), raised);
                if raised {
                    prop_assert!(st.row.first_ok_at.is_some());
                    prop_assert!(st.row.consecutive_failures >= n);
                    prop_assert!(seq[i] != Ok);
                }
                prop_assert!(st.row.status_since <= st.row.last_checked_at);
            }
            prop_assert_eq!(steps.iter().filter(|s| s.change.is_some()).count(), changes);
        }

        #[test]
        fn failures_below_threshold_never_alert(k in 0u32..5, n in 5u32..9) {
            let mut seq = vec![Ok];
            seq.extend(std::iter::repeat_n(Missing, k as usize));
            let steps = run(&seq, n);
            prop_assert!(steps.iter().all(|s| s.alert == AlertChange::None));
        }
    }
}
