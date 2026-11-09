//! The codec layer: bytes in, typed values out. No I/O, no clocks, no allocation beyond the
//! documented caps.

mod command;
mod data;
mod line;
mod sasl;

pub use command::{parse_command, AuthMechanism, Command, MailParams, ParseError};
pub use data::{DataEnd, DataReader};
pub use line::{LineEvent, LineReader};
pub use sasl::{
    decode_base64, parse_login_value, parse_plain, PlainCredentials, SaslError, MAX_IDENTITY_BYTES, MAX_PASSWORD_BYTES,
    MAX_SASL_LINE,
};
