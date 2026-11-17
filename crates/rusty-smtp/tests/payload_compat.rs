//! Release-to-release compatibility of what the services hand each other through the database (docs/design-zero-downtime.md): while an update runs, the submission service of one release
//! writes an outbound job that the outbound worker of the other release reads. The payload JSON written today is frozen here (a release must always read it), and a payload with a field this
//! release does not know (a newer release wrote it) is read all the same.

use rusty_smtp::outbound::payload::OutboundPayload;
use serde_json::json;

fn frozen() -> serde_json::Value {
    // The format of the release that introduced the field `tries`; never edit this value, add a new frozen one when the format grows.
    json!({
        "blob": "ab".repeat(32),
        "envelope_from": "alice@one.test",
        "recipients": ["bob@remote.test", "carol@remote.test"],
        "destination": "remote.test",
        "sender_domain": "one.test",
        "tries": 2
    })
}

#[test]
fn the_payload_of_the_previous_release_is_read() {
    let p = OutboundPayload::decode(&frozen()).expect("the frozen payload decodes");
    assert_eq!((p.envelope_from.as_str(), p.destination.as_str(), p.sender_domain.as_str(), p.tries), ("alice@one.test", "remote.test", "one.test", 2));
    assert_eq!(p.recipients.len(), 2);
}

#[test]
fn a_field_of_a_newer_release_does_not_stop_this_one_from_reading_the_job() {
    let mut v = frozen();
    v["priority"] = json!("bulk");
    v["trace"] = json!({ "span": "x" });
    assert!(OutboundPayload::decode(&v).is_ok(), "an unknown field is ignored, not an error");
}
