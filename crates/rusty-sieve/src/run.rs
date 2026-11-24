//! The Sieve interpreter (RFC 5228 section 5): a parsed script and a message in, the actions to take out. The interpreter changes nothing and reads nothing but the message it is given. Its limits are fixed so that a
//! script cannot make it do much work or ask for much: at most 10 redirects, 20 folders and one automatic reply per message, and matching with `*` and `?` is linear in the product of the two lengths at worst.

use crate::parse::{AddressPart, Command, Comparator, Match, MatchType, Script, Test, Vacation};

pub const MAX_REDIRECTS: usize = 10;
pub const MAX_FOLDERS: usize = 20;

/// What the script sees of a message: its header fields (unfolded, as written), the envelope, and the size.
#[derive(Debug, Clone, Default)]
pub struct Message {
    pub headers: Vec<(String, String)>,
    pub envelope_from: String,
    pub envelope_to: String,
    pub size: u64,
}

impl Message {
    /// Read the header section of a raw message.
    pub fn from_raw(raw: &[u8], envelope_from: &str, envelope_to: &str) -> Message {
        let text = String::from_utf8_lossy(raw);
        let mut headers: Vec<(String, String)> = Vec::new();
        for line in text.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.is_empty() {
                break;
            }
            if line.starts_with(' ') || line.starts_with('\t') {
                if let Some(last) = headers.last_mut() {
                    last.1.push(' ');
                    last.1.push_str(line.trim());
                }
            } else if let Some((n, v)) = line.split_once(':') {
                headers.push((n.trim().to_owned(), v.trim().to_owned()));
            }
            if headers.len() > 2000 {
                break;
            }
        }
        // A header test sees the text a person sees (RFC 5228 section 5.7): the encoded words are decoded.
        for h in &mut headers {
            h.1 = crate::words::decode(&h.1);
        }
        Message { headers, envelope_from: envelope_from.to_owned(), envelope_to: envelope_to.to_owned(), size: raw.len() as u64 }
    }

    fn values(&self, name: &str) -> Vec<&str> {
        self.headers.iter().filter(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str()).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Keep,
    Discard,
    FileInto(String),
    Redirect(String),
    Reject(String),
    Vacation(Vacation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub actions: Vec<Action>,
    /// Nothing the script did cancels the message being kept in the Inbox (RFC 5228 section 2.10.2).
    pub implicit_keep: bool,
}

/// `*` and `?` with `\` to make either literal (RFC 5228 section 2.7.1), over bytes already folded to the comparator's case.
fn glob(pattern: &[u8], text: &[u8]) -> bool {
    // Iterative with one backtrack point: linear in the product of the lengths at worst, never exponential.
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while t < text.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some(p);
                mark = t;
                p += 1;
            }
            Some(b'?') => {
                p += 1;
                t += 1;
            }
            Some(b'\\') if pattern.get(p + 1).is_some() => {
                if pattern.get(p + 1) == text.get(t) {
                    p += 2;
                    t += 1;
                } else if let Some(s) = star {
                    p = s + 1;
                    mark += 1;
                    t = mark;
                } else {
                    return false;
                }
            }
            Some(c) if Some(c) == text.get(t) => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some(s) => {
                    p = s + 1;
                    mark += 1;
                    t = mark;
                }
                None => return false,
            },
        }
    }
    while pattern.get(p) == Some(&b'*') {
        p += 1;
    }
    p == pattern.len()
}

fn fold(s: &str, c: &Comparator) -> Vec<u8> {
    match c {
        Comparator::AsciiCasemap => s.bytes().map(|b| b.to_ascii_lowercase()).collect(),
        Comparator::Octet => s.bytes().collect(),
    }
}

fn matches_one(m: &Match, value: &str, key: &str) -> bool {
    let (v, k) = (fold(value, &m.comparator), fold(key, &m.comparator));
    match m.kind {
        MatchType::Is => v == k,
        MatchType::Contains => k.is_empty() || v.windows(k.len()).any(|w| w == k.as_slice()),
        MatchType::Matches => glob(&k, &v),
    }
}

fn any_match(m: &Match, values: &[String], keys: &[String]) -> bool {
    values.iter().any(|v| keys.iter().any(|k| matches_one(m, v, k)))
}

/// The address specifications in a header value (`Name <a@b>, c@d`): what is inside `<>`, or the bare word with an `@`.
fn addresses(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    let bytes = value.as_bytes();
    let mut parts = Vec::new();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' => quoted = !quoted,
            b',' if !quoted => {
                parts.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    for p in parts {
        let p = p.trim();
        let inner = match (p.rfind('<'), p.rfind('>')) {
            (Some(a), Some(b)) if a < b => &p[a + 1..b],
            _ => p,
        };
        let inner = inner.trim();
        if inner.contains('@') && !inner.contains(char::is_whitespace) {
            out.push(inner.to_owned());
        }
    }
    out
}

fn part_of(addr: &str, part: &AddressPart) -> String {
    match (part, addr.rsplit_once('@')) {
        (AddressPart::All, _) => addr.to_owned(),
        (AddressPart::LocalPart, Some((l, _))) => l.to_owned(),
        (AddressPart::Domain, Some((_, d))) => d.to_owned(),
        _ => String::new(),
    }
}

fn eval(t: &Test, msg: &Message) -> bool {
    match t {
        Test::True => true,
        Test::False => false,
        Test::Not(x) => !eval(x, msg),
        Test::AllOf(v) => v.iter().all(|x| eval(x, msg)),
        Test::AnyOf(v) => v.iter().any(|x| eval(x, msg)),
        Test::Exists(names) => names.iter().all(|n| !msg.values(n).is_empty()),
        Test::Size { over, limit } => {
            if *over {
                msg.size > *limit
            } else {
                msg.size < *limit
            }
        }
        Test::Header { m, names, keys } => {
            let values: Vec<String> = names.iter().flat_map(|n| msg.values(n)).map(str::to_owned).collect();
            any_match(m, &values, keys)
        }
        Test::Address { m, part, names, keys } => {
            let values: Vec<String> = names.iter().flat_map(|n| msg.values(n)).flat_map(addresses).map(|a| part_of(&a, part)).collect();
            any_match(m, &values, keys)
        }
        Test::Envelope { m, part, names, keys } => {
            let mut values = Vec::new();
            for n in names {
                match n.to_ascii_lowercase().as_str() {
                    "from" => values.push(part_of(&msg.envelope_from, part)),
                    "to" => values.push(part_of(&msg.envelope_to, part)),
                    _ => {}
                }
            }
            any_match(m, &values, keys)
        }
    }
}

struct State {
    actions: Vec<Action>,
    cancel_keep: bool,
    redirects: usize,
    folders: usize,
    vacation: bool,
    stopped: bool,
}

impl State {
    fn add(&mut self, a: Action) {
        if !self.actions.contains(&a) {
            self.actions.push(a);
        }
    }
}

fn exec(cmds: &[Command], msg: &Message, st: &mut State) {
    for c in cmds {
        if st.stopped {
            return;
        }
        match c {
            Command::If { branches, otherwise } => {
                let mut taken = false;
                for (t, body) in branches {
                    if eval(t, msg) {
                        exec(body, msg, st);
                        taken = true;
                        break;
                    }
                }
                if !taken {
                    if let Some(body) = otherwise {
                        exec(body, msg, st);
                    }
                }
            }
            Command::Stop => st.stopped = true,
            Command::Keep => {
                st.cancel_keep = true;
                st.add(Action::Keep);
            }
            Command::Discard => {
                st.cancel_keep = true;
                st.add(Action::Discard);
            }
            Command::Redirect(a) => {
                st.cancel_keep = true;
                if st.redirects < MAX_REDIRECTS && !st.actions.contains(&Action::Redirect(a.clone())) {
                    st.redirects += 1;
                    st.add(Action::Redirect(a.clone()));
                }
            }
            Command::FileInto(f) => {
                st.cancel_keep = true;
                if st.folders < MAX_FOLDERS && !st.actions.contains(&Action::FileInto(f.clone())) {
                    st.folders += 1;
                    st.add(Action::FileInto(f.clone()));
                }
            }
            Command::Reject(r) => {
                // A rejection stands alone: nothing else of the script is done with the message, and it is not kept.
                st.cancel_keep = true;
                st.actions.retain(|a| matches!(a, Action::Vacation(_)));
                st.actions.push(Action::Reject(r.clone()));
                st.stopped = true;
            }
            Command::Vacation(v) => {
                if !st.vacation {
                    st.vacation = true;
                    st.add(Action::Vacation(v.clone()));
                }
            }
        }
    }
}

/// Run a script on a message.
pub fn run(script: &Script, msg: &Message) -> Outcome {
    let mut st = State { actions: Vec::new(), cancel_keep: false, redirects: 0, folders: 0, vacation: false, stopped: false };
    exec(&script.commands, msg, &mut st);
    Outcome { actions: st.actions, implicit_keep: !st.cancel_keep }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn msg() -> Message {
        Message::from_raw(
            b"From: Bob Smith <bob@Example.ORG>\r\nTo: alice@one.test, \"Doe, J\" <j@two.test>\r\nSubject: Weekly REPORT\r\nX-Spam-Flag: YES\r\nList-Id: <dev.lists.example.org>\r\nSubject: second\r\n  folded\r\n\r\nbody\r\n",
            "bounce@example.org",
            "alice@one.test",
        )
    }

    fn go(script: &str) -> Outcome {
        run(&parse(script).unwrap(), &msg())
    }

    #[test]
    fn nothing_to_do_keeps_the_message() {
        let o = go("");
        assert_eq!((o.actions.len(), o.implicit_keep), (0, true));
        let o = go("# only a comment\nrequire \"fileinto\";\n");
        assert!(o.implicit_keep);
    }

#[test]    fn a_header_test_sees_the_decoded_text_of_encoded_words() {        let m = Message::from_raw(b"Subject: =?utf-8?Q?Caf=C3=A9_menu?=
From: =?utf-8?B?QWRhIEzDtnZlbGFjZQ==?= <ada@example.org>

body
", "a@b.test", "c@d.test");        let run_on = |s: &str| run(&parse(s).unwrap(), &m);        assert_eq!(run_on("require \"fileinto\"; if header :contains \"subject\" \"café\" { fileinto \"Cafe\"; }").actions, [Action::FileInto("Cafe".into())]);        assert_eq!(run_on("require \"fileinto\"; if address :contains :all \"from\" \"example.org\" { fileinto \"Org\"; }").actions.len(), 1);        assert!(run_on("require \"fileinto\"; if header :contains \"subject\" \"=?utf-8\" { fileinto \"X\"; }").actions.is_empty(), "the words are not tested in their encoded form");    }
    #[test]
    fn header_tests_use_the_comparator_the_match_type_and_every_header_of_that_name() {
        assert_eq!(go("require \"fileinto\"; if header :contains \"subject\" \"report\" { fileinto \"Reports\"; }").actions, [Action::FileInto("Reports".into())]);
        assert!(go("require \"fileinto\"; if header :is \"subject\" \"weekly\" { fileinto \"X\"; }").actions.is_empty(), ":is needs the whole value");
        assert_eq!(go("require \"fileinto\"; if header :matches \"subject\" \"weekly*\" { fileinto \"M\"; }").actions.len(), 1);
        assert!(go("require \"fileinto\"; if header :comparator \"i;octet\" :contains \"subject\" \"report\" { fileinto \"X\"; }").actions.is_empty(), "octet: the case matters");
        assert_eq!(go("require \"fileinto\"; if header :contains \"subject\" \"folded\" { fileinto \"F\"; }").actions.len(), 1, "the second Subject, unfolded");
        assert_eq!(go("require \"fileinto\"; if header :is [\"x-spam-flag\", \"x-other\"] [\"no\", \"yes\"] { fileinto \"Spam\"; }").actions, [Action::FileInto("Spam".into())]);
    }

    #[test]
    fn address_tests_read_the_addresses_of_the_header_and_their_parts() {
        assert_eq!(go("if address :is :domain \"from\" \"example.org\" { discard; }").actions, [Action::Discard], "the domain is compared without case");
        assert_eq!(go("if address :is :localpart \"from\" \"bob\" { discard; }").actions.len(), 1);
        assert_eq!(go("if address :is :all \"to\" \"j@two.test\" { discard; }").actions.len(), 1, "a quoted comma does not split the address");
        assert!(go("if address :is \"from\" \"Bob Smith\" { discard; }").actions.is_empty(), "the name is not the address");
    }

    #[test]
    fn envelope_exists_and_size_and_the_combinators() {
        assert_eq!(go("require \"envelope\"; if envelope :is \"to\" \"alice@one.test\" { discard; }").actions.len(), 1);
        assert_eq!(go("require \"envelope\"; if envelope :matches :domain \"from\" \"*.org\" { discard; }").actions.len(), 1);
        assert_eq!(go("if exists [\"subject\", \"from\"] { keep; }").actions, [Action::Keep]);
        assert!(go("if exists [\"subject\", \"nothing\"] { keep; }").actions.is_empty());
        assert_eq!(go("if size :under 10K { keep; }").actions.len(), 1);
        assert!(go("if size :over 10 { keep; }").actions.len() == 1);
        assert_eq!(go("if allof (exists \"subject\", not size :over 1M, anyof (false, header :contains \"from\" \"bob\")) { discard; }").actions.len(), 1);
    }

    #[test]
    fn actions_cancel_the_implicit_keep_and_vacation_does_not() {
        let o = go("require [\"fileinto\", \"vacation\"]; vacation :days 3 :subject \"Away\" \"Back soon\"; fileinto \"A\";");
        assert!(!o.implicit_keep);
        assert_eq!(o.actions.len(), 2);
        assert!(matches!(&o.actions[0], Action::Vacation(v) if v.days == 3 && v.subject.as_deref() == Some("Away") && v.reason == "Back soon"));
        let only = go("require \"vacation\"; vacation \"Back soon\";");
        assert!(only.implicit_keep, "a reply alone leaves the message in the Inbox");
        assert!(matches!(&only.actions[0], Action::Vacation(v) if v.days == 7));
    }

    #[test]
    fn elsif_else_and_stop_choose_one_path_and_end_the_run() {
        let s = "require \"fileinto\"; if header :contains \"subject\" \"nope\" { fileinto \"A\"; } elsif header :contains \"subject\" \"report\" { fileinto \"B\"; stop; } else { fileinto \"C\"; } fileinto \"D\";";
        assert_eq!(go(s).actions, [Action::FileInto("B".into())], "stop ends the script");
        assert_eq!(go("require \"fileinto\"; if false { fileinto \"A\"; } else { fileinto \"C\"; } fileinto \"D\";").actions.len(), 2);
    }

    #[test]
    fn reject_stands_alone_and_redirects_and_folders_are_bounded_and_not_repeated() {
        let o = go("require [\"fileinto\", \"reject\"]; fileinto \"A\"; reject \"No thanks\"; fileinto \"B\";");
        assert_eq!(o.actions, [Action::Reject("No thanks".into())]);
        assert!(!o.implicit_keep);
        let many: String = (0..30).map(|i| format!("redirect \"x{i}@example.org\"; ")).collect();
        assert_eq!(go(&many).actions.len(), MAX_REDIRECTS);
        let same = go("redirect \"a@example.org\"; redirect \"a@example.org\";");
        assert_eq!(same.actions.len(), 1);
        let folders: String = format!("require \"fileinto\"; {}", (0..40).map(|i| format!("fileinto \"f{i}\"; ")).collect::<String>());
        assert_eq!(go(&folders).actions.len(), MAX_FOLDERS);
    }

    #[test]
    fn wildcards_match_like_the_standard_and_never_blow_up() {
        assert!(glob(b"a*b*c", b"aXXbYYc") && !glob(b"a*b*c", b"aXXbYY"));
        assert!(glob(b"?x", b"yx") && !glob(b"?x", b"x"));
        assert!(glob(b"a\\*b", b"a*b") && !glob(b"a\\*b", b"aXb"));
        assert!(glob(b"*", b"") && glob(b"", b""));
        let evil = "a".repeat(5000);
        let pat = format!("{}b", "a*".repeat(2000));
        assert!(!glob(pat.as_bytes(), evil.as_bytes()), "many stars on a long text end quickly");
    }

    #[test]
    fn text_blocks_comments_and_numbers_are_read() {
        let s = "require \"vacation\";\n/* a block\n comment */ vacation :days 2 text:\n# not a comment inside\n..dot first\nline two\n.\n;\n";
        let o = go(s);
        assert!(matches!(&o.actions[0], Action::Vacation(v) if v.reason == "# not a comment inside\n.dot first\nline two\n" && v.days == 2));
    }

    #[test]
    fn what_is_wrong_is_refused_with_a_line_and_what_is_not_supported_by_name() {
        for (script, line, text) in [
            ("keep\n", 1, ";"),
            ("fileinto \"A\";", 1, "require"),
            ("require \"regex\";", 1, "regex"),
            ("if true { keep; ", 1, "closed"),
            ("\n\nfrobnicate;", 3, "not known"),
            ("if header :weird \"a\" \"b\" { keep; }", 1, "weird"),
            ("keep; require \"fileinto\";", 1, "first"),
            ("if envelope :is \"to\" \"a\" { keep; }", 1, "envelope"),
            ("redirect \"a\"", 1, ";"),
            ("redirect [\"a\", ];", 1, "string"),
        ] {
            let e = parse(script).unwrap_err();
            assert!(e.line == line && e.message.contains(text), "{script:?}: {e}");
        }
        assert!(parse(&"keep; ".repeat(1001)).is_err(), "at most 1000 commands");
        assert!(parse(&"if true { ".repeat(20)).is_err());
        assert!(parse(&"#".repeat(70_000)).is_err(), "at most 64 KiB");
    }

    proptest::proptest! {
        /// Whatever text is given, the parser answers (never panics), and a script it accepts runs.
        #[test]
        fn any_text_parses_or_is_refused_and_a_parsed_script_runs(s in "[ -~\\n]{0,300}") {
            if let Ok(script) = parse(&s) {
                let _ = run(&script, &msg());
            }
        }
    }
}
