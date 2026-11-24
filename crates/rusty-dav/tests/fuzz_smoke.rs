//! Stable-toolchain stand-ins for fuzz/fuzz_targets/fuzz_dav_*.rs: the same entry points and invariants (`rusty_dav::fuzz_checks`), driven by the committed seeds and by proptest.

use proptest::prelude::*;
use rusty_dav::fuzz_checks as fz;

type Target = (&'static str, fn(&[u8]));

const TARGETS: [Target; 4] = [("fuzz_dav_object", fz::object), ("fuzz_dav_moment", fz::moment), ("fuzz_dav_xml", fz::xml), ("fuzz_dav_path", fz::path)];

#[test]
fn every_committed_seed_passes_its_target() {
    let mut seen = 0;
    for (name, f) in TARGETS {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds").join(name);
        for e in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
            f(&std::fs::read(e.unwrap().path()).unwrap());
            seen += 1;
        }
    }
    assert!(seen >= 12, "seeds went missing ({seen})");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn the_readers_are_total(data in proptest::collection::vec(any::<u8>(), 0..900)) {
        fz::object(&data);
        fz::moment(&data);
        fz::xml(&data);
        fz::path(&data);
    }

    #[test]
    fn objects_shaped_like_calendars_and_cards_keep_the_invariants(
        lines in proptest::collection::vec(
            prop_oneof![
                Just("UID:a".to_owned()), Just("DTSTART:20240101T100000Z".to_owned()), Just("DTEND:20230101T100000Z".to_owned()), Just("DURATION:PT1H".to_owned()),
                Just("RRULE:FREQ=DAILY;COUNT=3".to_owned()), Just("RRULE:FREQ=WEEKLY".to_owned()), Just("RDATE:20240301T100000Z".to_owned()), Just("RECURRENCE-ID:20240102T100000Z".to_owned()),
                Just("FN:Ada".to_owned()), Just("EMAIL:a@b.c".to_owned()), Just("BEGIN:VEVENT".to_owned()), Just("END:VEVENT".to_owned()), "[A-Z;=:-]{0,20}",
            ], 0..16),
        card in any::<bool>()) {
        let (head, version) = if card { ("VCARD", "3.0") } else { ("VCALENDAR", "2.0") };
        let text = format!("BEGIN:{head}\r\nVERSION:{version}\r\n{}\r\nEND:{head}\r\n", lines.join("\r\n"));
        fz::object(text.as_bytes());
    }

    #[test]
    fn moments_and_durations_shaped_like_themselves_keep_the_invariants(v in "[0-9]{0,9}(T[0-9]{0,7}Z?)?|[+-]?P[0-9]{0,4}[WD]?(T[0-9]{0,3}[HMS]?)?") {
        fz::moment(v.as_bytes());
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn documents_shaped_like_requests_keep_the_invariants(doc in "(<[?]xml version=\"1.0\"[?]>)?<(D|d|C):(propfind|calendar-query|prop|filter)( xmlns:(D|d|C)=\"(DAV:|urn:ietf:params:xml:ns:caldav)\")?>(<(D|C):[a-z-]{1,12}/>|[a-z&;<>]{0,6}){0,6}</(D|d|C):(propfind|calendar-query|prop|filter)>") {
        fz::xml(doc.as_bytes());
    }

    #[test]
    fn paths_shaped_like_the_services_keep_the_invariants(p in "/dav(/(principals|calendars|addressbooks|[a-z]{1,4}))?(/[A-Za-z0-9%._@-]{1,12}){0,4}/?") {
        fz::path(p.as_bytes());
    }
}
