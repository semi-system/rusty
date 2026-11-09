//! Framing: bytes from the wire to whole commands, enforcing the size caps BEFORE anything is
//! buffered or parsed. Sans-I/O and stateless apart from its own buffer: the driver pushes what
//! it read and asks for the next frame until it says `NeedMore`.
//!
//! A command is one or more lines; a line ending in `{n}` or `{n+}` announces `n` literal bytes
//! that follow the CRLF and belong to the same command. The framer
//!
//! * caps a command line at `max_line` bytes (over it: `Fatal`, the session closes),
//! * caps every literal at `max_literal` (`max_append` for `APPEND`) and the whole command at
//!   `max_command` BEFORE asking the client to send it: a synchronising literal over the cap is
//!   `Reject` (the client has not sent the data and is told NO), a non-synchronising one is
//!   `Fatal` (the client is already sending, so the connection is closed instead of draining),
//! * asks the driver to send the continuation request (`SendContinuation`) for acceptable
//!   synchronising literals,
//! * can switch to line mode for the AUTHENTICATE payload and the IDLE `DONE`.
//!
//! The command grammar itself is the grammar library's job: a `Command` frame is a complete command that
//! the decoder never needs more input for.

/// Size caps of one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLimits {
    /// Longest command line (a line with a literal marker counts without the literal), bytes.
    pub max_line: usize,
    /// Largest literal of any command except APPEND (login names, search strings, ...).
    pub max_literal: usize,
    /// Largest APPEND literal (the message size limit).
    pub max_append: usize,
    /// Largest sum of the literals of one command (an APPEND may use `max_append`).
    pub max_command: usize,
    /// Longest line in line mode (AUTHENTICATE payload, `DONE`).
    pub max_line_mode: usize,
}

impl FrameLimits {
    pub fn new(max_append: usize) -> FrameLimits {
        FrameLimits {
            max_line: 64 * 1024,
            max_literal: 1024 * 1024,
            max_append,
            max_command: 4 * 1024 * 1024,
            max_line_mode: 16 * 1024,
        }
    }
}

/// What the framer expects next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A command (the default).
    Command,
    /// One raw line (AUTHENTICATE payload, `DONE`).
    Line,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// A synchronising literal larger than allowed.
    LiteralTooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatalReason {
    LineTooLong,
    /// Lines and literals of one command add up past the command cap.
    CommandTooLarge,
    /// A non-synchronising literal larger than allowed, or literals adding up past the cap.
    LiteralTooLarge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Not enough input yet.
    NeedMore,
    /// A complete command, CRLF included, literals inline.
    Command(Vec<u8>),
    /// A raw line without its line ending (line mode only).
    Line(Vec<u8>),
    /// A synchronising literal of `len` bytes was accepted: send `+ ...` (after reserving memory for
    /// it, `append` marks the message-size path) and keep reading; [`Framer::reject_pending`] can
    /// still refuse it.
    SendContinuation { len: usize, append: bool },
    /// A non-synchronising literal of `len` bytes was accepted and is already on its way.
    NonSyncLiteral { len: usize, append: bool },
    /// Answer `tag NO [..]` (or `* BAD` without a tag); the command's bytes were dropped.
    Reject { tag: Option<String>, reason: RejectReason },
    /// Close the connection.
    Fatal(FatalReason),
}

#[derive(Debug)]
enum State {
    /// Reading a line starting at `scan`.
    Line,
    /// `remaining` literal bytes belong to the command and are skipped over.
    Literal { remaining: usize },
}

pub struct Framer {
    limits: FrameLimits,
    mode: Mode,
    buf: Vec<u8>,
    /// Bytes of `buf` already classified as part of the current command.
    scan: usize,
    state: State,
    /// Literal bytes announced so far in the current command.
    literal_total: usize,
    /// The current command is an APPEND (decided on its first line).
    append: Option<bool>,
    /// The session is logged in: only then may an APPEND use the message-size cap.
    authed: bool,
}

impl Framer {
    pub fn new(limits: FrameLimits) -> Framer {
        Framer { limits, mode: Mode::Command, buf: Vec::new(), scan: 0, state: State::Line, literal_total: 0, append: None, authed: false }
    }

    /// Tell the framer whether the session is logged in (before login an APPEND is refused outright:
    /// no continuation, no 50 MB path).
    pub fn set_authenticated(&mut self, authed: bool) {
        self.authed = authed;
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// Append bytes read from the peer. Call [`Framer::next`] until `NeedMore` before reading again,
    /// so the buffer never holds more than one command plus one read.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Bytes received but not yet returned in a frame. STARTTLS requires this to be 0 when the
    /// command is handled (otherwise plaintext was injected ahead of the handshake).
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    #[allow(clippy::should_implement_trait)] // not an Iterator: NeedMore is not the end
    pub fn next(&mut self) -> Frame {
        if self.mode == Mode::Line {
            return self.next_line();
        }
        loop {
            match self.state {
                State::Literal { remaining } => {
                    let have = self.buf.len().saturating_sub(self.scan);
                    if have < remaining {
                        self.scan += have;
                        self.state = State::Literal { remaining: remaining - have };
                        return Frame::NeedMore;
                    }
                    self.scan += remaining;
                    self.state = State::Line;
                }
                State::Line => {
                    let rest = self.buf.get(self.scan..).unwrap_or(&[]);
                    let Some(nl) = rest.iter().position(|b| *b == b'\n') else {
                        if rest.len() > self.limits.max_line {
                            return self.fatal(FatalReason::LineTooLong);
                        }
                        return Frame::NeedMore;
                    };
                    if nl + 1 > self.limits.max_line {
                        return self.fatal(FatalReason::LineTooLong);
                    }
                    let line_end = self.scan + nl + 1;
                    let append_now = self.append == Some(true) || (self.scan == 0 && is_append(self.buf.get(..line_end).unwrap_or(&[])));
                    let byte_cap = self.limits.max_command + if append_now && self.authed { self.limits.max_append } else { 0 };
                    if line_end > byte_cap {
                        return self.fatal(FatalReason::CommandTooLarge);
                    }
                    let line = self.buf.get(self.scan..line_end).unwrap_or(&[]);
                    let first_line = self.scan == 0;
                    if first_line {
                        self.append = Some(is_append(line));
                    }
                    let Some(marker) = literal_marker(line) else {
                        let cmd: Vec<u8> = self.buf.drain(..line_end).collect();
                        self.reset();
                        return Frame::Command(cmd);
                    };
                    let append = self.append.unwrap_or(false);
                    let cap = if append && self.authed { self.limits.max_append } else { self.limits.max_literal };
                    let forbidden = append && !self.authed;
                    let total_cap = self.limits.max_command.max(cap);
                    let too_large = forbidden
                        || marker.len > cap as u64
                        || self.literal_total as u64 + marker.len > total_cap as u64;
                    if too_large {
                        if marker.sync {
                            let tag = tag_of(&self.buf);
                            self.buf.drain(..line_end);
                            self.reset();
                            return Frame::Reject { tag, reason: RejectReason::LiteralTooLarge };
                        }
                        return self.fatal(FatalReason::LiteralTooLarge);
                    }
                    // len <= cap, so it fits usize.
                    let len = usize::try_from(marker.len).unwrap_or(usize::MAX);
                    self.literal_total += len;
                    self.scan = line_end;
                    self.state = State::Literal { remaining: len };
                    let append = append && self.authed;
                    return if marker.sync { Frame::SendContinuation { len, append } } else { Frame::NonSyncLiteral { len, append } };
                }
            }
        }
    }

    fn next_line(&mut self) -> Frame {
        let Some(nl) = self.buf.iter().position(|b| *b == b'\n') else {
            if self.buf.len() > self.limits.max_line_mode {
                return self.fatal(FatalReason::LineTooLong);
            }
            return Frame::NeedMore;
        };
        if nl + 1 > self.limits.max_line_mode {
            return self.fatal(FatalReason::LineTooLong);
        }
        let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
        line.pop(); // LF
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        self.mode = Mode::Command;
        Frame::Line(line)
    }

    /// Drop the command whose literal was just announced (the caller refuses it, e.g. for lack of
    /// memory); returns its tag. Only meaningful right after a `SendContinuation`.
    pub fn reject_pending(&mut self) -> Option<String> {
        let tag = tag_of(&self.buf);
        self.buf = Vec::new();
        self.reset();
        tag
    }

    fn reset(&mut self) {
        self.scan = 0;
        self.state = State::Line;
        self.literal_total = 0;
        self.append = None;
    }

    fn fatal(&mut self, why: FatalReason) -> Frame {
        self.buf = Vec::new();
        self.reset();
        Frame::Fatal(why)
    }
}

struct Marker {
    /// Announced length; `u64::MAX` when it does not even fit a u64.
    len: u64,
    sync: bool,
}

/// `{n}` or `{n+}` at the end of `line` (CRLF or LF included).
fn literal_marker(line: &[u8]) -> Option<Marker> {
    let mut end = line.len();
    if line.get(end.checked_sub(1)?) == Some(&b'\n') {
        end -= 1;
    }
    if end > 0 && line.get(end - 1) == Some(&b'\r') {
        end -= 1;
    }
    let body = line.get(..end)?;
    let body = body.strip_suffix(b"}")?;
    let (digits, sync) = match body.strip_suffix(b"+") {
        Some(d) => (d, false),
        None => (body, true),
    };
    let start = digits.iter().rposition(|b| !b.is_ascii_digit()).map_or(0, |i| i + 1);
    let (head, num) = digits.split_at(start);
    if num.is_empty() || head.last() != Some(&b'{') {
        return None;
    }
    let len = num.iter().try_fold(0u64, |acc, d| acc.checked_mul(10)?.checked_add(u64::from(d - b'0'))).unwrap_or(u64::MAX);
    Some(Marker { len, sync })
}

/// The first token of a command if it is a plausible tag.
pub(crate) fn tag_of(buf: &[u8]) -> Option<String> {
    let tok = buf.split(|b| *b == b' ' || *b == b'\r' || *b == b'\n').next()?;
    let ok = !tok.is_empty()
        && tok.len() <= 64
        && tok.iter().all(|b| b.is_ascii_graphic() && !b"(){%*\"\\]+".contains(b));
    ok.then(|| String::from_utf8_lossy(tok).into_owned())
}

/// Is the second token of the first line `APPEND`?
fn is_append(line: &[u8]) -> bool {
    let mut it = line.split(|b| *b == b' ' || *b == b'\r' || *b == b'\n').filter(|t| !t.is_empty());
    it.next();
    it.next().is_some_and(|v| v.eq_ignore_ascii_case(b"APPEND"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lim() -> FrameLimits {
        FrameLimits { max_line: 128, max_literal: 64, max_append: 1000, max_command: 400, max_line_mode: 32 }
    }

    fn drain_raw(f: &mut Framer) -> Vec<Frame> {
        let mut out = vec![];
        loop {
            match f.next() {
                Frame::NeedMore => return out,
                x => out.push(x),
            }
        }
    }

    /// Like `drain_raw` without the non-synchronising literal announcements.
    fn drain(f: &mut Framer) -> Vec<Frame> {
        let mut v = drain_raw(f);
        v.retain(|x| !matches!(x, Frame::NonSyncLiteral { .. }));
        v
    }

    #[test]
    fn a_line_is_one_command_even_when_split_or_pipelined() {
        let mut f = Framer::new(lim());
        f.push(b"a1 NO");
        assert_eq!(drain(&mut f), vec![]);
        f.push(b"OP\r\na2 NOOP\r\na3 NOO");
        assert_eq!(
            drain(&mut f),
            vec![Frame::Command(b"a1 NOOP\r\n".to_vec()), Frame::Command(b"a2 NOOP\r\n".to_vec())]
        );
        f.push(b"P\r\n");
        assert_eq!(drain(&mut f), vec![Frame::Command(b"a3 NOOP\r\n".to_vec())]);
        assert_eq!(f.buffered(), 0);
    }

    #[test]
    fn sync_literals_ask_for_a_continuation_and_stay_in_one_command() {
        let mut f = Framer::new(lim());
        f.push(b"a LOGIN {5}\r\n");
        assert_eq!(drain(&mut f), vec![Frame::SendContinuation { len: 5, append: false }]);
        f.push(b"alice {4}\r\n");
        assert_eq!(drain(&mut f), vec![Frame::SendContinuation { len: 4, append: false }]);
        f.push(b"pass\r\n");
        assert_eq!(drain(&mut f), vec![Frame::Command(b"a LOGIN {5}\r\nalice {4}\r\npass\r\n".to_vec())]);
    }

    #[test]
    fn non_sync_literals_need_no_continuation() {
        let mut f = Framer::new(lim());
        f.push(b"a LOGIN {5+}\r\nalice {4+}\r\npass\r\nb NOOP\r\n");
        assert_eq!(
            drain_raw(&mut f),
            vec![
                Frame::NonSyncLiteral { len: 5, append: false },
                Frame::NonSyncLiteral { len: 4, append: false },
                Frame::Command(b"a LOGIN {5+}\r\nalice {4+}\r\npass\r\n".to_vec()),
                Frame::Command(b"b NOOP\r\n".to_vec())
            ]
        );
    }

    #[test]
    fn literal_bytes_are_never_framing() {
        let mut f = Framer::new(lim());
        f.set_authenticated(true);
        // The literal holds a CRLF and something that looks like another literal marker.
        f.push(b"a APPEND INBOX {14+}\r\nx\r\n{99}\r\nyyyy\r\n");
        let frames = drain(&mut f);
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert!(matches!(&frames[0], Frame::Command(c) if c.starts_with(b"a APPEND") && c.ends_with(b"yyyy\r\n")));
    }

    #[test]
    fn oversized_sync_literal_is_rejected_before_any_data_is_read() {
        let mut f = Framer::new(lim());
        f.push(b"a LOGIN {2000000}\r\n");
        assert_eq!(
            drain(&mut f),
            vec![Frame::Reject { tag: Some("a".into()), reason: RejectReason::LiteralTooLarge }]
        );
        assert_eq!(f.buffered(), 0, "the dropped command leaves nothing behind");
        f.push(b"b NOOP\r\n");
        assert_eq!(drain(&mut f), vec![Frame::Command(b"b NOOP\r\n".to_vec())]);
    }

    #[test]
    fn a_two_gigabyte_announcement_never_allocates() {
        let mut f = Framer::new(lim());
        f.push(b"a APPEND INBOX {4294967295}\r\n");
        assert!(matches!(drain(&mut f)[..], [Frame::Reject { .. }]));
        f.push(b"a APPEND INBOX {18446744073709551615}\r\n");
        assert!(matches!(drain(&mut f)[..], [Frame::Reject { .. }]));
        f.push(b"a APPEND INBOX {99999999999999999999999}\r\n");
        assert!(matches!(drain(&mut f)[..], [Frame::Reject { .. }]));
        assert!(f.buf.capacity() < 4096);
    }

    #[test]
    fn append_may_exceed_the_ordinary_literal_cap_but_not_its_own() {
        let mut f = Framer::new(lim());
        f.set_authenticated(true);
        f.push(b"a APPEND INBOX {500}\r\n");
        assert_eq!(drain(&mut f), vec![Frame::SendContinuation { len: 500, append: true }]);
        let mut g = Framer::new(lim());
        g.set_authenticated(true);
        g.push(b"b APPEND INBOX {1001}\r\n");
        assert!(matches!(drain(&mut g)[..], [Frame::Reject { .. }]));
        // The same size on a command that is not an APPEND is over the ordinary cap.
        let mut h = Framer::new(lim());
        h.push(b"c SEARCH TEXT {500}\r\n");
        assert!(matches!(drain(&mut h)[..], [Frame::Reject { .. }]));
    }

    #[test]
    fn oversized_non_sync_literal_is_fatal() {
        let mut f = Framer::new(lim());
        f.push(b"a LOGIN {2000+}\r\n");
        assert_eq!(drain(&mut f), vec![Frame::Fatal(FatalReason::LiteralTooLarge)]);
    }

    #[test]
    fn many_small_literals_cannot_add_up_past_the_command_cap() {
        let mut f = Framer::new(lim());
        let mut cmd = b"a SEARCH".to_vec();
        for _ in 0..10 {
            cmd.extend_from_slice(b" {60+}\r\n");
            cmd.extend_from_slice(&[b'x'; 60]);
        }
        cmd.extend_from_slice(b"\r\n");
        f.push(&cmd);
        let fr = drain(&mut f);
        assert!(matches!(fr[0], Frame::Fatal(_)), "{fr:?}");
    }

    #[test]
    fn a_line_without_a_newline_is_cut_at_the_line_cap() {
        let mut f = Framer::new(lim());
        f.push(&[b'a'; 100]);
        assert_eq!(drain(&mut f), vec![]);
        f.push(&[b'a'; 100]);
        assert_eq!(drain(&mut f), vec![Frame::Fatal(FatalReason::LineTooLong)]);
    }

    #[test]
    fn line_mode_returns_one_raw_line_and_enforces_its_own_cap() {
        let mut f = Framer::new(lim());
        f.set_mode(Mode::Line);
        f.push(b"dGVzdA==\r\n");
        assert_eq!(drain(&mut f), vec![Frame::Line(b"dGVzdA==".to_vec())]);
        f.set_mode(Mode::Line);
        f.push(&[b'A'; 40]);
        assert_eq!(drain(&mut f), vec![Frame::Fatal(FatalReason::LineTooLong)]);
    }


    #[test]
    fn zero_length_literals_still_count_toward_the_command_cap() {
        // Review I2: `{0+}` repeated never grew `literal_total`, so a command could buffer without end.
        let mut f = Framer::new(lim());
        f.set_authenticated(true);
        f.push(b"a LOGIN");
        let mut fatal = false;
        for _ in 0..200 {
            f.push(b" {0+}\r\n");
            match drain(&mut f).as_slice() {
                [] => {}
                [Frame::Fatal(_)] => {
                    fatal = true;
                    break;
                }
                other => panic!("{other:?}"),
            }
        }
        assert!(fatal, "buffered {} bytes without a verdict", f.buffered());
        assert!(f.buffered() <= 1024);
    }

    #[test]
    fn long_commands_of_many_short_lines_are_capped_too() {
        let mut f = Framer::new(lim());
        let mut fatal = false;
        for _ in 0..100 {
            f.push(&[b'x'; 50]);
            f.push(b" {1+}\r\nz");
            if matches!(drain(&mut f).as_slice(), [Frame::Fatal(_)]) {
                fatal = true;
                break;
            }
        }
        assert!(fatal);
    }

    #[test]
    fn append_before_login_gets_no_continuation_and_no_big_cap() {
        // Review I3: pre-auth APPEND must not open the message-size path.
        let mut f = Framer::new(lim());
        f.push(b"x1 APPEND INBOX {900}\r\n");
        assert!(matches!(drain(&mut f)[..], [Frame::Reject { .. }]), "refused before `+`");
        let mut g = Framer::new(lim());
        g.push(b"x1 APPEND INBOX {900+}\r\n");
        assert!(matches!(drain(&mut g)[..], [Frame::Fatal(_)]));
        // Authenticated, the same line is fine.
        let mut h = Framer::new(lim());
        h.set_authenticated(true);
        h.push(b"x1 APPEND INBOX {900}\r\n");
        assert_eq!(drain(&mut h), vec![Frame::SendContinuation { len: 900, append: true }]);
    }

    #[test]
    fn a_literal_between_the_ordinary_and_the_command_cap_is_refused_for_non_append() {
        // Review M3 (mutation survivor): max_literal 64 < 200 < max_command 400.
        let mut f = Framer::new(lim());
        f.set_authenticated(true);
        f.push(b"a SEARCH TEXT {200}\r\n");
        assert!(matches!(drain(&mut f)[..], [Frame::Reject { .. }]));
    }


    #[test]
    fn a_pending_literal_can_be_refused_after_the_announcement() {
        let mut f = Framer::new(lim());
        f.set_authenticated(true);
        f.push(b"t9 APPEND INBOX {500}\r\n");
        assert_eq!(drain(&mut f), vec![Frame::SendContinuation { len: 500, append: true }]);
        assert_eq!(f.reject_pending().as_deref(), Some("t9"));
        assert_eq!(f.buffered(), 0);
        f.push(b"t10 NOOP\r\n");
        assert_eq!(drain(&mut f), vec![Frame::Command(b"t10 NOOP\r\n".to_vec())]);
    }

    #[test]
    fn a_bare_lf_ends_a_line_and_is_left_to_the_codec_to_refuse() {
        let mut f = Framer::new(lim());
        f.push(b"a NOOP\n");
        assert_eq!(drain(&mut f), vec![Frame::Command(b"a NOOP\n".to_vec())]);
    }
}
