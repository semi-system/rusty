//! The setup token of the first platform administrator (design section 14.3).
//!
//! When the admin service starts and no administrator exists it makes one random 256-bit token, hands it to the caller **once** (the binary prints it to
//! standard output and nothing else with it) and keeps only its SHA-256. The setup page asks for the token, then for the address and password of the
//! first platform administrator.
//!
//! * The token is compared in constant time; a new one is made at every start; it is consumed by a successful setup.
//! * **At most 5 wrong tokens per source address and 20 in all per hour**: an address that reached 5 is refused for the rest of its hour, and twenty
//!   wrong tokens from anywhere lock the gate for everyone for an hour (the caller shows `429`).
//! * Whether the page exists is a database fact decided by the caller (`AdminStore::has_platform_admin`), never a flag in memory.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};

use crate::session::Clock;

/// Wrong tokens one address may send per hour.
pub const MAX_WRONG_PER_ADDRESS: u32 = 5;
/// Wrong tokens in all per hour.
pub const MAX_WRONG_IN_ALL: u32 = 20;
/// The counting window, and how long the global lock lasts.
pub const WINDOW_SECS: u64 = 3600;
/// Most distinct addresses remembered (a flood from many addresses cannot grow the table).
const MAX_ADDRESSES: usize = 10_000;

/// What the gate says about a presented token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The token is right (the caller consumes it once the administrator exists).
    Accepted,
    /// The token is wrong.
    Wrong,
    /// Too many wrong tokens: nothing is compared until the lock ends.
    Locked { retry_after_secs: u64 },
}

struct State {
    hash: Option<[u8; 32]>,
    /// Wrong tokens and the start of the window, per address.
    per_address: HashMap<IpAddr, (u32, u64)>,
    all: (u32, u64),
    locked_until: u64,
}

#[derive(Clone)]
pub struct SetupGate {
    state: Arc<Mutex<State>>,
    clock: Arc<dyn Clock>,
}

fn hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.trim().to_ascii_lowercase().as_bytes()).into()
}

/// Equality that does not stop at the first difference.
fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
    std::hint::black_box(a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y))) == 0
}

impl SetupGate {
    /// A gate with a fresh token. Returns the token: print it once and drop it.
    pub fn new(clock: Arc<dyn Clock>) -> (SetupGate, String) {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        let token = hex::encode(bytes);
        let state = State { hash: Some(hash(&token)), per_address: HashMap::new(), all: (0, 0), locked_until: 0 };
        (SetupGate { state: Arc::new(Mutex::new(state)), clock }, token)
    }

    /// Check a presented token from `peer`. A wrong token counts against the address and the total; a right one does not and is not consumed
    /// here ([`consume`](Self::consume) does that once the administrator exists).
    pub fn check(&self, peer: IpAddr, presented: &str) -> Verdict {
        let now = self.clock.unix_secs();
        let Ok(mut s) = self.state.lock() else { return Verdict::Locked { retry_after_secs: WINDOW_SECS } };
        if s.locked_until > now {
            return Verdict::Locked { retry_after_secs: s.locked_until - now };
        }
        // This address already sent too many wrong tokens this hour: nothing is compared for it until its window ends.
        if let Some((n, start)) = s.per_address.get(&peer).copied() {
            let age = now.saturating_sub(start);
            if n >= MAX_WRONG_PER_ADDRESS && age < WINDOW_SECS {
                return Verdict::Locked { retry_after_secs: WINDOW_SECS - age };
            }
        }
        // Compare against the stored hash, or against a fixed one when the token is spent: the work is the same.
        let expected = s.hash.unwrap_or([0u8; 32]);
        if s.hash.is_some() && same(&hash(presented), &expected) {
            return Verdict::Accepted;
        }
        if s.per_address.len() >= MAX_ADDRESSES && !s.per_address.contains_key(&peer) {
            // Full of other addresses: forget the expired ones; if it is still full this wrong token counts against the total only.
            s.per_address.retain(|_, (_, start)| now.saturating_sub(*start) < WINDOW_SECS);
        }
        if s.per_address.len() < MAX_ADDRESSES || s.per_address.contains_key(&peer) {
            let e = s.per_address.entry(peer).or_insert((0, now));
            if now.saturating_sub(e.1) >= WINDOW_SECS {
                *e = (0, now);
            }
            e.0 += 1;
        }
        if now.saturating_sub(s.all.1) >= WINDOW_SECS {
            s.all = (0, now);
        }
        s.all.0 += 1;
        if s.all.0 >= MAX_WRONG_IN_ALL {
            // Twenty wrong tokens from anywhere: nobody is compared for an hour.
            s.locked_until = now + WINDOW_SECS;
        }
        Verdict::Wrong
    }

    /// The setup succeeded: the token is spent for good.
    pub fn consume(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.hash = None;
        }
    }

    /// Whether a token is still outstanding.
    pub fn is_live(&self) -> bool {
        self.state.lock().map(|s| s.hash.is_some()).unwrap_or(false)
    }
}
