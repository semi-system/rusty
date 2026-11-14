//! The Valkey implementation of [`Kv`], over the `fred` client. This directory is the only place that names
//! the client library (`scripts/wrapped-libs.txt`): nothing of it leaks into the trait or the options.
//!
//! Every operation is one Lua script (`EVALSHA`, reloaded when the server lost it): the sliding window and the
//! counter need atomicity, and one mechanism for all of them keeps the adapter small. The window uses the
//! server's own clock (`TIME`), so replicas with skewed clocks agree.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fred::prelude::{Builder, Client, ClientLike, Config, ReconnectPolicy, ServerConfig};
use fred::types::scripts::Script;
use fred::types::config::{TlsConfig, TlsConnector};
use rusty_core::{Error, Result};

use crate::{check_key, check_ttl, check_value, check_window, unavailable, Kv, WindowOutcome};

/// Sliding window of timestamps in a sorted set. `KEYS[1]` key, `ARGV[1]` window ms, `ARGV[2]` limit.
/// Returns `{allowed, count}`. An event exactly one window old is out (the same rule as `MemoryKv`).
const WINDOW_TRY: &str = r"
local t = redis.call('TIME')
local now = t[1] * 1000 + math.floor(t[2] / 1000)
local window = tonumber(ARGV[1])
redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', now - window)
local n = redis.call('ZCARD', KEYS[1])
if n < tonumber(ARGV[2]) then
  redis.call('ZADD', KEYS[1], now, now .. '-' .. n)
  redis.call('PEXPIRE', KEYS[1], window)
  return {1, n + 1}
end
return {0, n}
";

/// `KEYS[1]`, `ARGV[1]` window ms. Returns the number of events inside the window.
const WINDOW_COUNT: &str = r"
local t = redis.call('TIME')
local now = t[1] * 1000 + math.floor(t[2] / 1000)
redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', now - tonumber(ARGV[1]))
return redis.call('ZCARD', KEYS[1])
";

/// `KEYS[1]`, `ARGV[1]` ttl ms. The life starts at the first increment and is never extended.
const COUNTER_INCR: &str = r"
local n = redis.call('INCR', KEYS[1])
if n == 1 or redis.call('PTTL', KEYS[1]) < 0 then
  redis.call('PEXPIRE', KEYS[1], ARGV[1])
end
return n
";

const COUNTER_GET: &str = r"
local v = redis.call('GET', KEYS[1])
if v then return tonumber(v) end
return 0
";

const SET: &str = "return redis.call('SET', KEYS[1], ARGV[1], 'PX', ARGV[2])";
const GET: &str = "return redis.call('GET', KEYS[1])";
const TOUCH: &str = "return redis.call('PEXPIRE', KEYS[1], ARGV[1])";
const DELETE: &str = "return redis.call('DEL', KEYS[1])";

/// TLS to the server. The certificate chain and the host name are always verified.
#[derive(Debug, Clone, Default)]
pub struct TlsOptions {
    /// PEM bundle of the CA that signs the server's certificate; `None` uses the platform's roots.
    pub ca_bundle_pem: Option<Vec<u8>>,
}

/// Everything the connection needs, already resolved (the secret is read by the caller, never here).
#[derive(Clone)]
pub struct ValkeyOptions {
    pub host: String,
    pub port: u16,
    /// `None`: plain text (loopback and development only; the configuration refuses it elsewhere in production).
    pub tls: Option<TlsOptions>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub connect_timeout: Duration,
    pub command_timeout: Duration,
    /// Prepended to every key.
    pub key_prefix: String,
}

impl std::fmt::Debug for ValkeyOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValkeyOptions")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("tls", &self.tls.is_some())
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

struct Scripts {
    window_try: Script,
    window_count: Script,
    counter_incr: Script,
    counter_get: Script,
    set: Script,
    get: Script,
    touch: Script,
    delete: Script,
}

impl Scripts {
    fn new() -> Scripts {
        Scripts {
            window_try: Script::from_lua(WINDOW_TRY),
            window_count: Script::from_lua(WINDOW_COUNT),
            counter_incr: Script::from_lua(COUNTER_INCR),
            counter_get: Script::from_lua(COUNTER_GET),
            set: Script::from_lua(SET),
            get: Script::from_lua(GET),
            touch: Script::from_lua(TOUCH),
            delete: Script::from_lua(DELETE),
        }
    }
}

pub struct ValkeyKv {
    client: Client,
    prefix: String,
    scripts: Scripts,
    /// An outer bound on every call, on top of the client's own command timeout.
    bound: Duration,
}

fn millis_arg(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// Any failure of the client is "unavailable" to the caller; the cause is only logged, without the server.
fn down(what: &'static str, e: &fred::error::Error) -> Error {
    tracing::debug!(op = what, kind = ?e.kind(), "valkey call failed");
    unavailable()
}

fn tls_config(t: &TlsOptions) -> Result<TlsConfig> {
    use rustls::pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore::empty();
    match &t.ca_bundle_pem {
        Some(pem) => {
            let mut any = false;
            for cert in rustls::pki_types::CertificateDer::pem_slice_iter(pem) {
                let cert = cert.map_err(|_| Error::Invalid("valkey ca_bundle is not a PEM certificate bundle".into()))?;
                roots.add(cert).map_err(|_| Error::Invalid("valkey ca_bundle holds an unusable certificate".into()))?;
                any = true;
            }
            if !any {
                return Err(Error::Invalid("valkey ca_bundle holds no certificate".into()));
            }
        }
        None => {
            let native = rustls_native_certs::load_native_certs();
            for cert in native.certs {
                // A platform root the library cannot parse is skipped, as every TLS client does.
                let _ = roots.add(cert);
            }
            if roots.is_empty() {
                return Err(Error::Invalid("valkey tls needs a ca_bundle: the platform has no usable roots".into()));
            }
        }
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|_| Error::Invalid("valkey tls: no usable protocol version".into()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsConfig::from(TlsConnector::from(config)))
}

impl ValkeyKv {
    /// Connect and authenticate; a server that is down or refuses the login is an error here (a caller that
    /// wants to start anyway keeps its fallback and tries again later).
    pub async fn connect(o: ValkeyOptions) -> Result<ValkeyKv> {
        let mut config = Config {
            fail_fast: true,
            username: o.username.clone(),
            password: o.password.clone(),
            server: ServerConfig::new_centralized(o.host.clone(), o.port),
            ..Config::default()
        };
        if let Some(t) = &o.tls {
            config.tls = Some(tls_config(t)?);
        }
        let mut builder = Builder::from_config(config);
        builder
            .with_connection_config(|c| {
                c.connection_timeout = o.connect_timeout;
                c.internal_command_timeout = o.command_timeout;
                c.max_command_attempts = 2;
            })
            .with_performance_config(|c| {
                c.default_command_timeout = o.command_timeout;
            })
            // Reconnect for ever with a growing delay (100 ms to 5 s): a Valkey that restarts is found again.
            .set_policy(ReconnectPolicy::new_exponential(0, 100, 5000, 2));
        let client = builder.build().map_err(|e| down("build", &e))?;
        client.init().await.map_err(|e| down("connect", &e))?;
        let kv = ValkeyKv {
            client,
            prefix: o.key_prefix,
            scripts: Scripts::new(),
            bound: o.command_timeout.saturating_add(Duration::from_millis(250)),
        };
        kv.ping().await?;
        Ok(kv)
    }

    fn key(&self, key: &str) -> Result<String> {
        check_key(key)?;
        Ok(format!("{}{key}", self.prefix))
    }

    async fn bounded<T>(&self, what: &'static str, fut: impl std::future::Future<Output = std::result::Result<T, fred::error::Error>>) -> Result<T> {
        match tokio::time::timeout(self.bound, fut).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(down(what, &e)),
            Err(_) => {
                tracing::debug!(op = what, "valkey call timed out");
                Err(unavailable())
            }
        }
    }
}

#[async_trait]
impl Kv for ValkeyKv {
    async fn ping(&self) -> Result<()> {
        let pong: String = self.bounded("ping", self.client.ping(None)).await?;
        if pong.eq_ignore_ascii_case("pong") {
            Ok(())
        } else {
            Err(unavailable())
        }
    }

    async fn window_try_record(&self, key: &str, window: Duration, limit: u32) -> Result<WindowOutcome> {
        check_window(window, limit)?;
        let k = self.key(key)?;
        let out: Vec<i64> = self
            .bounded(
                "window_try",
                self.scripts.window_try.evalsha_with_reload(&self.client, vec![k], vec![millis_arg(window), i64::from(limit)]),
            )
            .await?;
        match out.as_slice() {
            [allowed, count] => Ok(WindowOutcome { allowed: *allowed == 1, count: u32::try_from(*count).unwrap_or(u32::MAX) }),
            _ => Err(unavailable()),
        }
    }

    async fn window_count(&self, key: &str, window: Duration) -> Result<u32> {
        check_ttl(window)?;
        let k = self.key(key)?;
        let n: i64 = self
            .bounded("window_count", self.scripts.window_count.evalsha_with_reload(&self.client, vec![k], vec![millis_arg(window)]))
            .await?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    async fn counter_incr(&self, key: &str, ttl: Duration) -> Result<u64> {
        check_ttl(ttl)?;
        let k = self.key(key)?;
        let n: i64 = self
            .bounded("counter_incr", self.scripts.counter_incr.evalsha_with_reload(&self.client, vec![k], vec![millis_arg(ttl)]))
            .await?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    async fn counter_get(&self, key: &str) -> Result<u64> {
        let k = self.key(key)?;
        let n: i64 = self
            .bounded("counter_get", self.scripts.counter_get.evalsha_with_reload::<i64, _, Vec<String>>(&self.client, vec![k], vec![]))
            .await?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    async fn set(&self, key: &str, value: &[u8], ttl: Duration) -> Result<()> {
        check_ttl(ttl)?;
        check_value(value)?;
        let k = self.key(key)?;
        let args: Vec<fred::types::Value> = vec![fred::types::Value::Bytes(value.to_vec().into()), millis_arg(ttl).into()];
        let _: String = self.bounded("set", self.scripts.set.evalsha_with_reload(&self.client, vec![k], args)).await?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let k = self.key(key)?;
        let v: Option<Vec<u8>> = self
            .bounded("get", self.scripts.get.evalsha_with_reload::<Option<Vec<u8>>, _, Vec<String>>(&self.client, vec![k], vec![]))
            .await?;
        Ok(v)
    }

    async fn touch(&self, key: &str, ttl: Duration) -> Result<bool> {
        check_ttl(ttl)?;
        let k = self.key(key)?;
        let n: i64 = self.bounded("touch", self.scripts.touch.evalsha_with_reload(&self.client, vec![k], vec![millis_arg(ttl)])).await?;
        Ok(n == 1)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let k = self.key(key)?;
        let _: i64 = self
            .bounded("delete", self.scripts.delete.evalsha_with_reload::<i64, _, Vec<String>>(&self.client, vec![k], vec![]))
            .await?;
        Ok(())
    }
}
