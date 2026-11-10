//! RSA-2048 key generation for DKIM keys (`rustyctl dkim generate`). `ring` cannot generate RSA keys and
//! the RustCrypto `rsa` crate (RUSTSEC-2023-0071) is not allowed in the tree, so the key pair is made here
//! from `num-bigint` and the operating system's random generator: two independent random 1024-bit primes
//! (top two bits set, so the modulus has exactly 2048 bits), strong probable-prime testing (trial
//! division, then 40 Miller-Rabin rounds with random bases: error below 2^-80), the public exponent 65537,
//! and the CRT values. The result is DER `RSAPrivateKey` (PKCS #1), which `ring` then loads: ring checks the
//! structure and the key's consistency, and the adapter also signs and verifies with it before the key is
//! written, so a faulty key never leaves this function.
//!
//! This runs once per domain in the operator's CLI, never in the daemon and never on attacker-chosen input.

use num_bigint::{BigInt, BigUint};
use num_integer::Integer;
use num_traits::{One, Zero};
use rand_core::{OsRng, RngCore};
use rusty_core::Result;

const E: u32 = 65_537;
const PRIME_BITS: u64 = 1024;
const MR_ROUNDS: usize = 40;

const SMALL_PRIMES: [u32; 54] = [
    3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83, 89, 97, 101, 103, 107, 109, 113, 127, 131, 137, 139,
    149, 151, 157, 163, 167, 173, 179, 181, 191, 193, 197, 199, 211, 223, 227, 229, 233, 239, 241, 251, 257,
];


fn random_below(upper: &BigUint) -> BigUint {
    // Rejection sampling over the bit length of `upper`: unbiased.
    let bits = upper.bits();
    loop {
        let mut buf = vec![0u8; usize::try_from(bits.div_ceil(8)).unwrap_or(0)];
        OsRng.fill_bytes(&mut buf);
        let mut x = BigUint::from_bytes_be(&buf);
        x &= (BigUint::one() << bits) - BigUint::one();
        if &x < upper {
            return x;
        }
    }
}

fn is_probable_prime(n: &BigUint) -> bool {
    if n < &BigUint::from(2u32) {
        return false;
    }
    for p in SMALL_PRIMES {
        let p = BigUint::from(p);
        if n == &p {
            return true;
        }
        if (n % &p).is_zero() {
            return false;
        }
    }
    // n - 1 = d * 2^s
    let one = BigUint::one();
    let n_minus_1 = n - &one;
    let s = n_minus_1.trailing_zeros().unwrap_or(0);
    let d = &n_minus_1 >> s;
    let two = BigUint::from(2u32);
    'witness: for _ in 0..MR_ROUNDS {
        // base in [2, n-2]
        let a = random_below(&(n - BigUint::from(3u32))) + &two;
        let mut x = a.modpow(&d, n);
        if x == one || x == n_minus_1 {
            continue;
        }
        for _ in 1..s {
            x = x.modpow(&two, n);
            if x == n_minus_1 {
                continue 'witness;
            }
        }
        return false;
    }
    true
}

fn gen_prime() -> BigUint {
    let e = BigUint::from(E);
    loop {
        let mut buf = vec![0u8; (PRIME_BITS / 8) as usize];
        OsRng.fill_bytes(&mut buf);
        let mut c = BigUint::from_bytes_be(&buf);
        // exactly PRIME_BITS bits and the top two bits set (so p*q has 2*PRIME_BITS bits); odd.
        c |= BigUint::one() << (PRIME_BITS - 1);
        c |= BigUint::one() << (PRIME_BITS - 2);
        c |= BigUint::one();
        if c.bits() != PRIME_BITS {
            continue;
        }
        // gcd(e, p - 1) = 1, so that e is invertible mod (p-1)(q-1).
        if ((&c - BigUint::one()) % &e).is_zero() {
            continue;
        }
        if is_probable_prime(&c) {
            return c;
        }
    }
}

fn mod_inverse(a: &BigUint, m: &BigUint) -> Option<BigUint> {
    let (a, m) = (BigInt::from(a.clone()), BigInt::from(m.clone()));
    let g = a.extended_gcd(&m);
    if !g.gcd.is_one() {
        return None;
    }
    let inv = g.x.mod_floor(&m);
    inv.to_biguint()
}

fn der_len(n: usize) -> Vec<u8> {
    if n < 0x80 {
        vec![n as u8]
    } else if n <= 0xff {
        vec![0x81, n as u8]
    } else {
        vec![0x82, (n >> 8) as u8, n as u8]
    }
}

fn der_int(v: &BigUint) -> Vec<u8> {
    let mut b = v.to_bytes_be();
    if b.is_empty() {
        b.push(0);
    }
    if b.first().is_some_and(|x| x & 0x80 != 0) {
        b.insert(0, 0);
    }
    let mut out = vec![0x02];
    out.extend(der_len(b.len()));
    out.extend(b);
    out
}

fn der_seq(parts: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = parts.concat();
    let mut out = vec![0x30];
    out.extend(der_len(body.len()));
    out.extend(body);
    out
}

/// A fresh RSA-2048 key: `(PKCS #1 RSAPrivateKey DER, PKCS #1 RSAPublicKey DER)`.
pub(crate) fn generate_2048() -> Result<(Vec<u8>, Vec<u8>)> {
    // The loop ends with an Ok; the Result is for symmetry with the other algorithm.
    Ok(generate_2048_inner())
}

fn generate_2048_inner() -> (Vec<u8>, Vec<u8>) {
    let e = BigUint::from(E);
    let one = BigUint::one();
    loop {
        let (mut p, mut q) = (gen_prime(), gen_prime());
        if p == q {
            continue;
        }
        if p < q {
            std::mem::swap(&mut p, &mut q);
        }
        // Primes that are too close make n easy to factor (Fermat): refuse a gap below 2^(1024-100).
        let gap = &p - &q;
        if gap.bits() < PRIME_BITS - 100 {
            continue;
        }
        let n = &p * &q;
        if n.bits() != 2 * PRIME_BITS {
            continue;
        }
        let (p1, q1) = (&p - &one, &q - &one);
        let lambda = p1.lcm(&q1);
        let Some(d) = mod_inverse(&e, &lambda) else { continue };
        // d must be large enough (d > 2^(n/2) holds with overwhelming probability for e = 65537; check).
        if d.bits() < PRIME_BITS + 1 {
            continue;
        }
        let (dp, dq) = (&d % &p1, &d % &q1);
        let Some(qinv) = mod_inverse(&q, &p) else { continue };
        let private = der_seq(&[der_int(&BigUint::zero()), der_int(&n), der_int(&e), der_int(&d), der_int(&p), der_int(&q), der_int(&dp), der_int(&dq), der_int(&qinv)]);
        let public = der_seq(&[der_int(&n), der_int(&e)]);
        return (private, public);
    }
}
