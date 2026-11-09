//! The codec layer: bytes in, typed values out. No I/O, no clocks, no allocation beyond the
//! documented caps.

mod command;
mod data;
mod line;

pub use command::{parse_command, Command, MailParams, ParseError};
pub use data::{DataEnd, DataReader};
pub use line::{LineEvent, LineReader};
