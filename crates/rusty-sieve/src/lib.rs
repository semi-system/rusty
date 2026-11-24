//! Sieve (RFC 5228): the language people write their mail filters in. This crate is a parser and an interpreter with no I/O: a script and a message in, a list of actions out. The delivery path decides what to
//! do with the actions; ManageSieve (RFC 5804) is how people upload scripts. Written from the inside out, like the name server: first the engine.
#![forbid(unsafe_code)]

pub mod parse;
pub mod run;
