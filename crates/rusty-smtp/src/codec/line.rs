//! Command line framing: split a byte stream into lines (CRLF or bare LF) with a hard length
//! cap. A line over the cap is reported once, at its end, and its bytes are never kept, so the
//! buffer never exceeds `max + 1` bytes whatever the peer sends.

/// What the reader found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineEvent {
    /// One line without its terminator.
    Line(Vec<u8>),
    /// A line longer than the cap (skipped to its end).
    TooLong,
}

#[derive(Debug)]
pub struct LineReader {
    buf: Vec<u8>,
    max: usize,
    skipping: bool,
}

impl LineReader {
    /// `max` is the longest accepted line, terminator not counted.
    pub fn new(max: usize) -> LineReader {
        LineReader { buf: Vec::new(), max, skipping: false }
    }

    /// Bytes currently held for an unfinished line.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Take the next complete line from the front of `input`, advancing it. `None` once the input
    /// holds no further complete line (a partial line stays buffered inside the reader and the
    /// whole input is consumed).
    pub fn read_line(&mut self, input: &mut &[u8]) -> Option<LineEvent> {
        if input.is_empty() {
            return None;
        }
        let (chunk, rest, ended) = match input.iter().position(|&b| b == b'\n') {
            Some(p) => {
                let (head, tail) = input.split_at(p);
                (head, tail.get(1..).unwrap_or_default(), true)
            }
            None => (*input, <&[u8]>::default(), false),
        };
        *input = rest;
        if !self.skipping {
            // One byte of slack for the CR that precedes the LF.
            let room = (self.max + 1).saturating_sub(self.buf.len());
            if chunk.len() > room {
                self.skipping = true;
                self.buf = Vec::new();
            } else {
                self.buf.extend_from_slice(chunk);
            }
        }
        if !ended {
            return None;
        }
        if self.skipping {
            self.skipping = false;
            self.buf = Vec::new();
            return Some(LineEvent::TooLong);
        }
        let mut line = std::mem::take(&mut self.buf);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.len() > self.max {
            return Some(LineEvent::TooLong);
        }
        Some(LineEvent::Line(line))
    }
}
