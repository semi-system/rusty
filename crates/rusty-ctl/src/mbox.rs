//! The mbox format of `rustyctl mailbox export` and `import` (docs/design-mailbox-tools.md): the "mboxrd" variant. Each message is preceded by a line `From <sender> <date>`, and a body line that
//! starts with `From ` (after any number of `>`) gets one more `>` on the way out and loses one on the way in, so that no body can end a message early. Line ends are LF in the file and CRLF
//! in the mailbox.

/// `Mon Jan  1 00:00:00 2024` for a unix time (UTC), the date of a `From ` line.
pub fn asctime(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (the algorithm of Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"].get(usize::try_from(days.rem_euclid(7)).unwrap_or(0)).copied().unwrap_or("Thu");
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"].get(usize::try_from(m - 1).unwrap_or(0)).copied().unwrap_or("Jan");
    format!("{weekday} {month} {d:2} {:02}:{:02}:{:02} {y}", secs / 3600, secs % 3600 / 60, secs % 60)
}

/// Append one message to an mbox in `out`: the `From ` line, the message with LF line ends and the `>From ` quoting, and a blank line.
pub fn write_message(out: &mut Vec<u8>, internal_date: i64, raw: &[u8]) {
    out.extend_from_slice(format!("From MAILER-DAEMON {}\n", asctime(internal_date)).as_bytes());
    let mut lines = raw.split(|b| *b == b'\n').peekable();
    while let Some(line) = lines.next() {
        // The last piece after the final line end is empty: nothing to write for it.
        if lines.peek().is_none() && line.is_empty() {
            break;
        }
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let quoted = line.iter().skip_while(|b| **b == b'>').collect::<Vec<_>>();
        if quoted.starts_with(&[&b'F', &b'r', &b'o', &b'm', &b' ']) {
            out.push(b'>');
        }
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    if out.last() != Some(&b'\n') {
        out.push(b'\n');
    }
    out.push(b'\n');
}

/// The messages of an mbox, each with CRLF line ends and the quoting undone. Text before the first `From ` line is ignored.
pub fn split(mbox: &[u8]) -> Vec<Vec<u8>> {
    let mut messages: Vec<Vec<u8>> = Vec::new();
    let mut current: Option<Vec<u8>> = None;
    let mut previous_blank = true;
    for line in mbox.split(|b| *b == b'\n') {
        let text = line.strip_suffix(b"\r").unwrap_or(line);
        if text.starts_with(b"From ") && previous_blank {
            if let Some(m) = current.take() {
                messages.push(finish(m));
            }
            current = Some(Vec::new());
            previous_blank = false;
            continue;
        }
        if let Some(m) = current.as_mut() {
            let quotes = text.iter().take_while(|b| **b == b'>').count();
            let unquoted = if quotes > 0 && text.get(quotes..).is_some_and(|t| t.starts_with(b"From ")) { text.get(1..).unwrap_or(text) } else { text };
            m.extend_from_slice(unquoted);
            m.extend_from_slice(b"\r\n");
        }
        previous_blank = text.is_empty();
    }
    if let Some(m) = current.take() {
        messages.push(finish(m));
    }
    messages.retain(|m| !m.is_empty());
    messages
}

/// The blank line that separates a message from the next `From ` line belongs to the file, not to the message.
fn finish(mut m: Vec<u8>) -> Vec<u8> {
    while m.ends_with(b"\r\n\r\n") {
        m.truncate(m.len() - 2);
    }
    if m == b"\r\n" {
        m.clear();
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_date_of_a_from_line_is_asctime_in_utc() {
        assert_eq!(asctime(0), "Thu Jan  1 00:00:00 1970");
        assert_eq!(asctime(1_700_000_000), "Tue Nov 14 22:13:20 2023");
        assert_eq!(asctime(951_782_400), "Tue Feb 29 00:00:00 2000");
    }

    #[test]
    fn a_message_goes_out_with_lf_line_ends_and_its_from_lines_quoted() {
        let raw = b"Subject: x\r\n\r\nFrom here\r\n>From there\r\nplain\r\n";
        let mut out = Vec::new();
        write_message(&mut out, 0, raw);
        assert_eq!(String::from_utf8(out).unwrap(), "From MAILER-DAEMON Thu Jan  1 00:00:00 1970\nSubject: x\n\n>From here\n>>From there\nplain\n\n");
    }

    #[test]
    fn what_went_out_comes_back_the_same() {
        let a = b"Subject: one\r\n\r\nFrom the start\r\n>From quoted\r\n\r\nafter a blank\r\n".to_vec();
        let b = b"Subject: two\r\n\r\nbody without a final line end".to_vec();
        let mut out = Vec::new();
        write_message(&mut out, 100, &a);
        write_message(&mut out, 200, &b);
        let back = split(&out);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0], a);
        assert_eq!(back[1], [b.as_slice(), b"\r\n"].concat(), "a final line end is added");
    }

    #[test]
    fn a_from_line_inside_a_body_without_a_blank_line_before_it_does_not_split() {
        let mbox = b"From a Thu Jan  1 00:00:00 1970\nSubject: x\n\nline\nFrom not a separator\n\nFrom b Thu Jan  1 00:00:00 1970\nSubject: y\n\nbody\n";
        let got = split(mbox);
        assert_eq!(got.len(), 2);
        assert!(String::from_utf8_lossy(&got[0]).contains("From not a separator"));
        assert!(String::from_utf8_lossy(&got[1]).starts_with("Subject: y"));
        assert!(split(b"not an mbox at all\n").is_empty());
    }
}
