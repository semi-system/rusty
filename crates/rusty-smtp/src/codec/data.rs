//! DATA framing (RFC 5321 section 4.5.2): dot-unstuffing, a hard size cap, and an end marker that
//! is only ever CRLF.CRLF. A "." line ended by a bare LF or a bare CR does not end the data, so a
//! sender cannot smuggle a second message past a stricter or laxer neighbour ("SMTP smuggling").
//! Bytes are never normalised here: the output is the original data, unstuffed.

/// How a DATA transfer ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataEnd {
    /// The terminator was seen. `data` is the unstuffed content without the terminator line.
    Complete { data: Vec<u8>, has_nul: bool },
    /// The unstuffed content exceeded the cap; nothing is kept and the reader is finished.
    TooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// At the start of a line (after CRLF, or at the very start).
    Start,
    /// Saw a "." at the start of a line.
    Dot,
    /// Saw ".\r" at the start of a line.
    DotCr,
    /// Inside a line.
    Mid,
    /// Inside a line, just after a CR.
    MidCr,
}

#[derive(Debug)]
pub struct DataReader {
    state: State,
    out: Vec<u8>,
    max: usize,
    has_nul: bool,
    finished: bool,
}

impl DataReader {
    /// `max` is the largest unstuffed content in bytes (CRLFs included).
    pub fn new(max: usize) -> DataReader {
        DataReader { state: State::Start, out: Vec::new(), max, has_nul: false, finished: false }
    }

    /// Consume bytes from the front of `input` up to and including the terminator (or until the
    /// cap is hit). Whatever follows stays in `input` for the command reader.
    pub fn feed(&mut self, input: &mut &[u8]) -> Option<DataEnd> {
        if self.finished {
            return Some(DataEnd::TooLarge);
        }
        while let Some((&b, rest)) = input.split_first() {
            *input = rest;
            if let Some(end) = self.step(b) {
                return Some(end);
            }
        }
        None
    }

    fn push(&mut self, b: u8) -> Result<(), DataEnd> {
        if self.out.len() >= self.max {
            self.finished = true;
            self.out = Vec::new();
            return Err(DataEnd::TooLarge);
        }
        if b == 0 {
            self.has_nul = true;
        }
        self.out.push(b);
        Ok(())
    }

    fn step(&mut self, b: u8) -> Option<DataEnd> {
        match self.step_inner(b) {
            Ok(end) => end,
            Err(e) => Some(e),
        }
    }

    fn step_inner(&mut self, b: u8) -> Result<Option<DataEnd>, DataEnd> {
        match self.state {
            State::Start => match b {
                b'.' => self.state = State::Dot,
                b'\r' => {
                    self.push(b)?;
                    self.state = State::MidCr;
                }
                _ => {
                    self.push(b)?;
                    self.state = State::Mid;
                }
            },
            State::Dot => match b {
                b'\r' => self.state = State::DotCr,
                _ => {
                    // ".." is a stuffed dot; ".X" is a leading dot that the sender should have doubled.
                    self.push(b)?;
                    self.state = State::Mid;
                }
            },
            State::DotCr => {
                if b == b'\n' {
                    self.finished = true;
                    return Ok(Some(DataEnd::Complete { data: std::mem::take(&mut self.out), has_nul: self.has_nul }));
                }
                // ".\r" followed by something else: the dot was a stuffing dot of a line "\r...".
                self.push(b'\r')?;
                self.state = State::MidCr;
                return self.step_inner(b);
            }
            State::Mid => {
                self.push(b)?;
                if b == b'\r' {
                    self.state = State::MidCr;
                }
            }
            State::MidCr => {
                self.push(b)?;
                self.state = match b {
                    b'\n' => State::Start,
                    b'\r' => State::MidCr,
                    _ => State::Mid,
                };
            }
        }
        Ok(None)
    }
}
