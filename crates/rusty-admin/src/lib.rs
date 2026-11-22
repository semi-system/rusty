#![forbid(unsafe_code)]
//! `rusty-admin`: the logic and the HTTP API of the admin web (admin web design, sections 4, 5 and 14.3). No SQL and no HTTP library types: sessions over the `Kv`
//! store, sign-in over the directory's own authenticator, what a signed-in account may administer, and the routes (over `rusty-http`). The
//! `rusty-admin` binary uses them.
//!
//! * [`session`]: random session ids (only their SHA-256 is stored), idle and absolute timeouts, account-wide revocation, failing closed.
//! * [`auth`]: sign-in with the throttle, one fixed failure for every reason, and the per-request check that the account is still an
//!   administrator and still active.

pub mod admins;
pub mod api;
pub mod auth;
pub mod autoconfig;
pub mod autodiscover;
pub mod dns;
pub mod health;
pub mod live;
pub mod manage;
pub mod monitor;
pub mod outbound;
pub mod plans;
pub mod routing;
pub mod protocols;
pub mod queue;
pub mod run;
pub mod session;
pub mod setup;
pub mod storage;
pub mod web;

pub use auth::{AdminAuth, Principal, Scope, SignInError};
pub use api::{AdminApp, MailSettings, Services, COOKIE};
pub use session::{Clock, ManualNow, Session, SessionConfig, SessionStore, SystemNow};
pub use setup::{SetupGate, Verdict as SetupVerdict};
