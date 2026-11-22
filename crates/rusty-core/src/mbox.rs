//! The mailbox file formats of `rustyctl mailbox export` and `import` (docs/design-mailbox-tools.md).
//!
//! **mbox** is the "mboxrd" variant. Each message is preceded by a line `From <sender> <date>`, and a body line that starts with `From ` (after any number of `>`) gets one more `>` on the way out
//! and loses one on the way in, so that no body can end a message early. Line ends are LF in the file and CRLF in the mailbox. The flags travel in the `Status:` (`R` read) and `X-Status:`
//! (`A` answered, `F` flagged, `D` deleted, `T` draft) headers that mutt and Dovecot use; they are added on the way out and taken off again on the way in. The date is the one of the `From ` line.
//!
//! **Maildir** keeps one file per message; the flags are the letters after `:2,` in its name (`D` draft, `F` flagged, `R` answered, `S` seen) and the date is the number the name starts with.

use crate::Flags;

/// `Mon Jan  1 00:00:00 2024` for a unix time (UTC), the date of a `From ` line.
pub fn asctime(unix: i64) -> String {
    let (y, m, d, secs) = civil(unix);
    let days = unix.div_euclid(86_400);
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"].get(usize::try_from(days.rem_euclid(7)).unwrap_or(0)).copied().unwrap_or("Thu");
    let month = MONTHS.get(usize::try_from(m - 1).unwrap_or(0)).copied().unwrap_or("Jan");
    format!("{weekday} {month} {d:2} {:02}:{:02}:{:02} {y}", secs / 3600, secs % 3600 / 60, secs % 60)
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// (year, month, day, seconds of the day) of a unix time in UTC (the algorithm of Howard Hinnant).
fn civil(unix: i64) -> (i64, i64, i64, i64) {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d, secs)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// The unix time of an `asctime` date (`Tue Nov 14 22:13:20 2023`); `None` when it is not one.
pub fn parse_asctime(text: &str) -> Option<i64> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let [_, month, day, time, year] = parts.as_slice() else { return None };
    let m = i64::try_from(MONTHS.iter().position(|x| x.eq_ignore_ascii_case(month))?).ok()? + 1;
    let (d, y): (i64, i64) = (day.parse().ok()?, year.parse().ok()?);
    let mut t = time.split(':').map(|x| x.parse::<i64>().ok());
    let (h, mi, s) = (t.next()??, t.next()??, t.next()??);
    ((1..=31).contains(&d) && (0..24).contains(&h) && (0..60).contains(&mi) && (0..=60).contains(&s) && (1970..=9999).contains(&y)).then(|| days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60 + s)
}

/// The two header lines that carry flags in an mbox (`Status`, `X-Status`), empty ones left out.
fn status_headers(flags: Flags) -> String {
    let has = |f: Flags| flags.bits() & f.bits() != 0;
    let status = if has(Flags::SEEN) { "RO" } else { "O" };
    let x: String = [(Flags::ANSWERED, 'A'), (Flags::FLAGGED, 'F'), (Flags::DELETED, 'D'), (Flags::DRAFT, 'T')].iter().filter(|(f, _)| has(*f)).map(|(_, c)| *c).collect();
    let mut out = format!("Status: {status}\n");
    if !x.is_empty() {
        out.push_str(&format!("X-Status: {x}\n"));
    }
    out
}

fn is_folder_line(line: &[u8]) -> bool {
    let lower: Vec<u8> = line.iter().take(FOLDER_HEADER.len() + 1).map(u8::to_ascii_lowercase).collect();
    lower.starts_with(format!("{}:", FOLDER_HEADER.to_ascii_lowercase()).as_bytes())
}

fn is_status_line(line: &[u8]) -> bool {
    let lower: Vec<u8> = line.iter().take(9).map(u8::to_ascii_lowercase).collect();
    lower.starts_with(b"status:") || lower.starts_with(b"x-status:")
}

/// Append one message to an mbox in `out`: the `From ` line, the flags as `Status` headers, the message with LF line ends and the `>From ` quoting, and a blank line.
pub fn write_message(out: &mut Vec<u8>, internal_date: i64, flags: Flags, raw: &[u8]) {
    write_message_in(out, internal_date, flags, None, raw);
}

/// The header that says which folder a message was in, for an mbox that holds several folders (taken off again on the way in).
pub const FOLDER_HEADER: &str = "X-Rusty-Folder";

/// [`write_message`] for a file that holds every folder of a mailbox: the folder's name is written in `X-Rusty-Folder` (line breaks and control characters dropped).
pub fn write_message_in(out: &mut Vec<u8>, internal_date: i64, flags: Flags, folder: Option<&str>, raw: &[u8]) {
    out.extend_from_slice(format!("From MAILER-DAEMON {}\n", asctime(internal_date)).as_bytes());
    let mut in_header = true;
    let mut wrote_status = false;
    let mut lines = raw.split(|b| *b == b'\n').peekable();
    while let Some(line) = lines.next() {
        // The last piece after the final line end is empty: nothing to write for it.
        if lines.peek().is_none() && line.is_empty() {
            break;
        }
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if in_header {
            if line.is_empty() {
                out.extend_from_slice(status_headers(flags).as_bytes());
                if let Some(f) = folder {
                    let clean: String = f.chars().filter(|c| !c.is_control()).take(200).collect();
                    out.extend_from_slice(format!("{FOLDER_HEADER}: {clean}\n").as_bytes());
                }
                wrote_status = true;
                in_header = false;
            } else if is_status_line(line) || is_folder_line(line) {
                // Our own come from the flags; the ones the message arrived with would say it twice.
                continue;
            }
        }
        let quoted = line.iter().skip_while(|b| **b == b'>').copied().collect::<Vec<_>>();
        if quoted.starts_with(b"From ") {
            out.push(b'>');
        }
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    if in_header && !wrote_status {
        // A message that is only headers.
        out.extend_from_slice(status_headers(flags).as_bytes());
        if let Some(f) = folder {
            let clean: String = f.chars().filter(|c| !c.is_control()).take(200).collect();
            out.extend_from_slice(format!("{FOLDER_HEADER}: {clean}\n").as_bytes());
        }
    }
    if out.last() != Some(&b'\n') {
        out.push(b'\n');
    }
    out.push(b'\n');
}

/// One message of an mbox or a Maildir as it goes into the mailbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    /// CRLF line ends, the flag headers taken off, the quoting undone.
    pub raw: Vec<u8>,
    pub internal_date: Option<i64>,
    pub flags: Flags,
    /// The folder the file says it was in (`X-Rusty-Folder`), taken off the message.
    pub folder: Option<String>,
}

fn flags_of_headers(status: &str, x_status: &str) -> Flags {
    let mut bits = 0u8;
    if status.contains('R') {
        bits |= Flags::SEEN.bits();
    }
    for (c, f) in [('A', Flags::ANSWERED), ('F', Flags::FLAGGED), ('D', Flags::DELETED), ('T', Flags::DRAFT)] {
        if x_status.contains(c) {
            bits |= f.bits();
        }
    }
    Flags::from_bits(bits).unwrap_or_default()
}

/// The messages of an mbox. Text before the first `From ` line is ignored.
pub fn split(mbox: &[u8]) -> Vec<Parsed> {
    let mut messages: Vec<Parsed> = Vec::new();
    let mut current: Option<(Vec<u8>, Option<i64>)> = None;
    let mut previous_blank = true;
    for line in mbox.split(|b| *b == b'\n') {
        let text = line.strip_suffix(b"\r").unwrap_or(line);
        if text.starts_with(b"From ") && previous_blank {
            if let Some((m, date)) = current.take() {
                messages.push(finish(m, date));
            }
            let line_text = String::from_utf8_lossy(text);
            let words: Vec<&str> = line_text.split_whitespace().collect();
            let date = words.len().checked_sub(5).and_then(|from| words.get(from..)).and_then(|tail| parse_asctime(&tail.join(" ")));
            current = Some((Vec::new(), date));
            previous_blank = false;
            continue;
        }
        if let Some((m, _)) = current.as_mut() {
            let quotes = text.iter().take_while(|b| **b == b'>').count();
            let unquoted = if quotes > 0 && text.get(quotes..).is_some_and(|t| t.starts_with(b"From ")) { text.get(1..).unwrap_or(text) } else { text };
            m.extend_from_slice(unquoted);
            m.extend_from_slice(b"\r\n");
        }
        previous_blank = text.is_empty();
    }
    if let Some((m, date)) = current.take() {
        messages.push(finish(m, date));
    }
    messages.retain(|m| !m.raw.is_empty());
    messages
}

/// Take the flag headers off the head of a message and read them; the blank line that separates the message from the next `From ` line belongs to the file.
fn finish(mut m: Vec<u8>, internal_date: Option<i64>) -> Parsed {
    while m.ends_with(b"\r\n\r\n") {
        m.truncate(m.len() - 2);
    }
    if m == b"\r\n" {
        m.clear();
    }
    let (mut status, mut x_status, mut folder) = (String::new(), String::new(), None::<String>);
    let mut out = Vec::with_capacity(m.len());
    let mut in_header = true;
    for line in m.split_inclusive(|b| *b == b'\n') {
        let bare = line.strip_suffix(b"\r\n").unwrap_or(line);
        if in_header {
            if bare.is_empty() {
                in_header = false;
            } else if is_folder_line(bare) {
                let value = String::from_utf8_lossy(bare).split_once(':').map(|(_, v)| v.trim().to_owned()).unwrap_or_default();
                if !value.is_empty() {
                    folder = Some(value.chars().filter(|c| !c.is_control()).take(200).collect());
                }
                continue;
            } else if is_status_line(bare) {
                let text = String::from_utf8_lossy(bare).to_ascii_uppercase();
                let value = text.split_once(':').map(|(_, v)| v.trim().to_owned()).unwrap_or_default();
                if text.starts_with("X-") {
                    x_status.push_str(&value);
                } else {
                    status.push_str(&value);
                }
                continue;
            }
        }
        out.extend_from_slice(line);
    }
    Parsed { raw: out, internal_date, flags: flags_of_headers(&status, &x_status), folder }
}

// ------------------------------------------------------------------------------------------------ Maildir

/// The file name of a message in a Maildir `cur` directory: the time, a unique part, the host word and the flags (`:2,` and the letters in alphabetical order).
pub fn maildir_name(internal_date: i64, id: i64, flags: Flags) -> String {
    let has = |f: Flags| flags.bits() & f.bits() != 0;
    let letters: String = [(Flags::DRAFT, 'D'), (Flags::FLAGGED, 'F'), (Flags::ANSWERED, 'R'), (Flags::SEEN, 'S'), (Flags::DELETED, 'T')].iter().filter(|(f, _)| has(*f)).map(|(_, c)| *c).collect();
    format!("{internal_date}.M{id}.rusty:2,{letters}")
}

/// The time and the flags a Maildir file name says; a name that says neither gives `(None, no flags)`.
pub fn parse_maildir_name(name: &str) -> (Option<i64>, Flags) {
    let date = name.split('.').next().and_then(|d| d.parse::<i64>().ok()).filter(|d| *d >= 0);
    let letters = name.rsplit_once(":2,").map(|(_, l)| l).unwrap_or_default();
    let mut bits = 0u8;
    for (c, f) in [('D', Flags::DRAFT), ('F', Flags::FLAGGED), ('R', Flags::ANSWERED), ('S', Flags::SEEN), ('T', Flags::DELETED)] {
        if letters.contains(c) {
            bits |= f.bits();
        }
    }
    (date, Flags::from_bits(bits).unwrap_or_default())
}

/// A message read from a Maildir file: its line ends are made CRLF (the mailbox's form).
pub fn maildir_message(name: &str, bytes: &[u8]) -> Parsed {
    let (internal_date, flags) = parse_maildir_name(name);
    let mut raw = Vec::with_capacity(bytes.len() + 16);
    let mut previous = 0u8;
    for b in bytes {
        if *b == b'\n' && previous != b'\r' {
            raw.push(b'\r');
        }
        raw.push(*b);
        previous = *b;
    }
    Parsed { raw, internal_date, flags, folder: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_date_of_a_from_line_is_asctime_in_utc_and_reads_back() {
        assert_eq!(asctime(0), "Thu Jan  1 00:00:00 1970");
        assert_eq!(asctime(1_700_000_000), "Tue Nov 14 22:13:20 2023");
        assert_eq!(asctime(951_782_400), "Tue Feb 29 00:00:00 2000");
        for t in [0, 951_782_400, 1_700_000_000, 4_000_000_000] {
            assert_eq!(parse_asctime(&asctime(t)), Some(t), "{t}");
        }
        assert_eq!(parse_asctime("not a date at all"), None);
        assert_eq!(parse_asctime("Tue Xyz 14 22:13:20 2023"), None);
    }

    #[test]
    fn a_message_goes_out_with_lf_line_ends_status_headers_and_its_from_lines_quoted() {
        let raw = b"Subject: x\r\n\r\nFrom here\r\n>From there\r\nplain\r\n";
        let mut out = Vec::new();
        write_message(&mut out, 0, Flags::SEEN, raw);
        assert_eq!(String::from_utf8(out).unwrap(), "From MAILER-DAEMON Thu Jan  1 00:00:00 1970\nSubject: x\nStatus: RO\n\n>From here\n>>From there\nplain\n\n");
    }

    #[test]
    fn what_went_out_comes_back_the_same_with_its_date_and_flags() {
        let a = b"Subject: one\r\n\r\nFrom the start\r\n>From quoted\r\n\r\nafter a blank\r\n".to_vec();
        let b = b"Subject: two\r\nStatus: RO\r\n\r\nbody without a final line end".to_vec();
        let both = Flags::from_bits(Flags::SEEN.bits() | Flags::FLAGGED.bits() | Flags::ANSWERED.bits()).unwrap();
        let mut out = Vec::new();
        write_message(&mut out, 1_700_000_000, both, &a);
        write_message(&mut out, 200, Flags::empty(), &b);
        let back = split(&out);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0], Parsed { raw: a, internal_date: Some(1_700_000_000), flags: both, folder: None });
        assert_eq!(back[1].raw, b"Subject: two\r\n\r\nbody without a final line end\r\n".to_vec(), "its own Status header was replaced by the flags, and a final line end was added");
        assert_eq!((back[1].internal_date, back[1].flags), (Some(200), Flags::empty()));
    }

    #[test]
    fn a_from_line_inside_a_body_without_a_blank_line_before_it_does_not_split() {
        let mbox = b"From a Thu Jan  1 00:00:00 1970\nSubject: x\n\nline\nFrom not a separator\n\nFrom b Thu Jan  1 00:00:00 1970\nSubject: y\n\nbody\n";
        let got = split(mbox);
        assert_eq!(got.len(), 2);
        assert!(String::from_utf8_lossy(&got[0].raw).contains("From not a separator"));
        assert!(String::from_utf8_lossy(&got[1].raw).starts_with("Subject: y"));
        assert!(split(b"not an mbox at all\n").is_empty());
    }

    #[test]
    fn a_file_with_several_folders_says_which_message_was_in_which() {
        let mut out = Vec::new();
        write_message_in(&mut out, 100, Flags::SEEN, Some("Projects/Q4"), b"Subject: a\r\n\r\nbody\r\n");
        write_message_in(&mut out, 200, Flags::empty(), Some("Inbox"), b"Subject: b\r\n\r\nbody b\r\n");
        let back = split(&out);
        assert_eq!(back.iter().map(|m| m.folder.as_deref()).collect::<Vec<_>>(), vec![Some("Projects/Q4"), Some("Inbox")]);
        assert_eq!(back[0].raw, b"Subject: a\r\n\r\nbody\r\n".to_vec(), "the folder header is taken off again");
    }

    #[test]
    fn a_maildir_name_carries_the_time_and_the_flags_both_ways() {
        let f = Flags::from_bits(Flags::SEEN.bits() | Flags::FLAGGED.bits()).unwrap();
        let name = maildir_name(1_700_000_000, 7, f);
        assert_eq!(name, "1700000000.M7.rusty:2,FS");
        assert_eq!(parse_maildir_name(&name), (Some(1_700_000_000), f));
        assert_eq!(parse_maildir_name("1234.abc.host:2,"), (Some(1234), Flags::empty()));
        assert_eq!(parse_maildir_name("weird"), (None, Flags::empty()));
        assert_eq!(maildir_message("1.x:2,S", b"A: b\n\nbody\n").raw, b"A: b\r\n\r\nbody\r\n".to_vec());
        assert_eq!(maildir_message("1.x", b"A: b\r\n\r\nbody\r\n").raw, b"A: b\r\n\r\nbody\r\n".to_vec(), "CRLF stays CRLF");
    }
}
