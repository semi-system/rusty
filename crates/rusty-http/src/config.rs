//! Limits and the headers every response carries.

use std::time::Duration;

/// Hard bounds, fixed in code (design section 3). The defaults are the production values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// The request line and all headers together.
    pub max_head_bytes: usize,
    pub max_headers: usize,
    /// The request body. The API of the first version uploads nothing.
    pub max_body_bytes: usize,
    /// The path of the request target.
    pub max_path_bytes: usize,
    /// The whole request target (path and query).
    pub max_target_bytes: usize,
    /// Receiving the request head. Also bounds how long an idle keep-alive connection is kept.
    pub head_timeout: Duration,
    /// The handler, from the complete request to its response.
    pub request_timeout: Duration,
    /// A connection is closed after this many requests.
    pub max_requests_per_connection: u32,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_head_bytes: 16 * 1024,
            max_headers: 64,
            max_body_bytes: 64 * 1024,
            max_path_bytes: 2048,
            max_target_bytes: 4096,
            head_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            max_requests_per_connection: 100,
        }
    }
}

/// The browser-side policy of design section 6. A handler may set any of these itself (the static files
/// set `Cache-Control`); the server adds only the ones the response lacks.
pub const SECURITY_HEADERS: [(&str, &str); 8] = [
    (
        "content-security-policy",
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'; font-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
    ),
    ("strict-transport-security", "max-age=31536000; includeSubDomains"),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
    ("cross-origin-opener-policy", "same-origin"),
    ("cross-origin-resource-policy", "same-origin"),
    ("permissions-policy", "accelerometer=(), camera=(), geolocation=(), gyroscope=(), microphone=(), payment=(), usb=()"),
    ("cache-control", "no-store"),
];

#[derive(Debug, Clone, Default)]
pub struct HttpConfig {
    pub limits: Limits,
}
