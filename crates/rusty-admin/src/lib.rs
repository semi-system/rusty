#![forbid(unsafe_code)]
//! `rusty-admin`: the logic behind the admin web (admin web design, sections 5 and 14.3). No HTTP types and no SQL: sessions over the `Kv`
//! store, sign-in over the directory's own authenticator, and what a signed-in account may administer. The HTTP layer (`rusty-http`) and the
//! `rusty-admin` binary use it.
//!
//! * [`session`]: random session ids (only their SHA-256 is stored), idle and absolute timeouts, account-wide revocation, failing closed.
//! * [`auth`]: sign-in with the throttle, one fixed failure for every reason, and the per-request check that the account is still an
//!   administrator and still active.

pub mod auth;
pub mod session;

pub use auth::{AdminAuth, Principal, Scope, SignInError};
pub use session::{Clock, ManualNow, Session, SessionConfig, SessionStore, SystemNow};
