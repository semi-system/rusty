//! The settings of a domain (docs/design-domain-settings.md): the defaults and limits that apply to its accounts and that the admin web sets. They are keys of the domain's configuration layer
//! (the services read `quota_bytes` and `max_message_bytes` from it through the directory), so a change reaches the next delivery without a restart.
//!
//! * `default_plan`: the plan a new account is put on when none is named.
//! * `max_accounts`: how many accounts the domain may have.
//! * `quota_bytes`, `max_message_bytes`: the limits of every account of the domain that its own plan and its own configuration do not set.
//! * `password_min_length` and `password_classes`: the rules for a password an administrator sets (at least this long, with characters of at least this many of the four classes: lower case,
//!   upper case, digit, other).
//!
//! Platform administrators set all of them; a domain administrator reads them and sets the password rules of its own domain.

use std::sync::Arc;

use rusty_core::{DomainId, Error};
use rusty_http::{handler, Method, Request, Response, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::api::{fail, AdminApp};
use crate::manage::scoped_domain;

const MAX_BYTES: u64 = 1 << 50;
/// The shortest password the rules may ask for is also the shortest any administrator-set password may be.
pub const MIN_LENGTH_FLOOR: u32 = 8;
const MAX_ACCOUNTS: u64 = 1_000_000;

fn err(status: u16, code: &'static str) -> Response {
    Response::error(status, code).with_header("cache-control", "no-store")
}

/// The settings as stored and as shown; a missing one is `null`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DomainSettings {
    pub default_plan: Option<String>,
    pub max_accounts: Option<u64>,
    pub quota_bytes: Option<u64>,
    pub max_message_bytes: Option<u64>,
    pub password_min_length: Option<u32>,
    pub password_classes: Option<u32>,
    /// Senders (an address, or a domain with its subdomains) the domain's recipients accept without the spam and virus scan.
    #[serde(default)]
    pub sender_allow: Vec<String>,
    /// Senders refused at the end of DATA (when every recipient blocks them).
    #[serde(default)]
    pub sender_block: Vec<String>,
    /// A text added to the end of every plain-text or HTML message the domain sends (a disclaimer); `null`: none.
    #[serde(default)]
    pub footer_text: Option<String>,
    /// File extensions (without a dot) of attachments that are refused when the domain's accounts receive them.
    #[serde(default)]
    pub blocked_extensions: Vec<String>,
}

/// A blocked extension in its stored form: lower case, letters and digits, 1 to 10 of them.
fn extension_entry(e: &str) -> Option<String> {
    let e = e.trim().trim_start_matches('.').to_ascii_lowercase();
    ((1..=10).contains(&e.len()) && e.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(e)
}

/// The list in its stored form (sorted, without repeats), or `None` when an entry is not an extension or there are more than 50.
fn clean_extensions(list: &[String]) -> Option<Vec<String>> {
    if list.len() > 50 {
        return None;
    }
    let mut out: Vec<String> = list.iter().map(|e| extension_entry(e)).collect::<Option<_>>()?;
    out.sort();
    out.dedup();
    Some(out)
}

/// Most entries in one list.
const MAX_LIST: usize = 500;

/// A list entry in its stored form: trimmed, lower case, an address or a domain; `None` when it is neither.
fn list_entry(e: &str) -> Option<String> {
    let e = e.trim().to_ascii_lowercase();
    let ok_char = |c: char| c.is_ascii_alphanumeric() || "-._+@".contains(c);
    let shape = match e.split_once('@') {
        Some((local, domain)) => !local.is_empty() && domain.contains('.') && !domain.contains('@'),
        None => e.contains('.'),
    };
    (e.len() <= 254 && shape && e.chars().all(ok_char) && !e.starts_with('.') && !e.ends_with('.')).then_some(e)
}

/// The list in its stored form, or `None` when it is too long or has an entry that is neither an address nor a domain.
fn clean_list(list: &[String]) -> Option<Vec<String>> {
    if list.len() > MAX_LIST {
        return None;
    }
    let mut out: Vec<String> = list.iter().map(|e| list_entry(e)).collect::<Option<_>>()?;
    out.sort();
    out.dedup();
    Some(out)
}

fn text_list(v: &Value, key: &str) -> Vec<String> {
    v.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

impl DomainSettings {
    pub(crate) fn from_config(config: &str) -> DomainSettings {
        let v: Value = serde_json::from_str(config).unwrap_or(Value::Null);
        let n = |k: &str| v.get(k).and_then(Value::as_u64);
        DomainSettings {
            default_plan: v.get("default_plan").and_then(Value::as_str).map(str::to_owned),
            max_accounts: n("max_accounts"),
            quota_bytes: n("quota_bytes"),
            max_message_bytes: n("max_message_bytes"),
            password_min_length: n("password_min_length").and_then(|x| u32::try_from(x).ok()),
            password_classes: n("password_classes").and_then(|x| u32::try_from(x).ok()),
            sender_allow: text_list(&v, "sender_allow"),
            sender_block: text_list(&v, "sender_block"),
            footer_text: v.get("footer_text").and_then(Value::as_str).map(str::to_owned),
            blocked_extensions: text_list(&v, "blocked_extensions"),
        }
    }

    /// The reason these values cannot be stored, if any.
    fn invalid(&self) -> bool {
        self.max_accounts.is_some_and(|n| n == 0 || n > MAX_ACCOUNTS)
            || self.quota_bytes.is_some_and(|n| n == 0 || n > MAX_BYTES)
            || self.max_message_bytes.is_some_and(|n| n == 0 || n > MAX_BYTES)
            || self.password_min_length.is_some_and(|n| !(MIN_LENGTH_FLOOR..=128).contains(&n))
            || self.password_classes.is_some_and(|n| n > 4)
            || self.default_plan.as_deref().is_some_and(|p| p.is_empty() || p.len() > 64)
            || self.footer_text.as_deref().is_some_and(|f| f.trim().is_empty() || f.len() > rusty_smtp::footer::MAX_FOOTER || f.contains('\0'))
            || clean_extensions(&self.blocked_extensions).is_none()
            || clean_list(&self.sender_allow).is_none()
            || clean_list(&self.sender_block).is_none()
    }

    /// Why `password` does not meet the rules of the domain, in a fixed word, or `None` when it does.
    pub(crate) fn password_problem(&self, password: &str) -> Option<&'static str> {
        let len = password.chars().count();
        if len < usize::try_from(self.password_min_length.unwrap_or(MIN_LENGTH_FLOOR)).unwrap_or(usize::MAX).max(MIN_LENGTH_FLOOR as usize) || password.len() > 1024 {
            return Some("the password is too short");
        }
        let classes = [
            password.chars().any(char::is_lowercase),
            password.chars().any(char::is_uppercase),
            password.chars().any(|c| c.is_ascii_digit()),
            password.chars().any(|c| !c.is_alphanumeric()),
        ]
        .iter()
        .filter(|x| **x)
        .count();
        if classes < usize::try_from(self.password_classes.unwrap_or(0)).unwrap_or(0) {
            return Some("the password needs more kinds of characters");
        }
        None
    }
}

/// The settings of a domain, read from its configuration.
pub(crate) async fn of(app: &AdminApp, domain: DomainId) -> Result<DomainSettings, Error> {
    match app.services.as_ref() {
        Some(s) => Ok(DomainSettings::from_config(&s.plans.domain_config(domain.0).await?)),
        None => Ok(DomainSettings::default()),
    }
}

async fn get(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, false).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let dom = match scoped_domain(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    match of(app, dom.id).await {
        Ok(s) => Response::json(200, &s).with_header("cache-control", "no-store"),
        Err(e) => fail(&e),
    }
}

async fn put(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let dom = match scoped_domain(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let Ok(mut new) = serde_json::from_slice::<DomainSettings>(&req.body) else { return err(400, "invalid") };
    if new.invalid() {
        return err(400, "invalid");
    }
    // Stored (and answered) in one form: trimmed, lower case, sorted, without repeats.
    new.sender_allow = clean_list(&new.sender_allow).unwrap_or_default();
    new.sender_block = clean_list(&new.sender_block).unwrap_or_default();
    new.blocked_extensions = clean_extensions(&new.blocked_extensions).unwrap_or_default();
    let Some(s) = app.services.as_ref() else { return err(503, "unavailable") };
    let old = match of(app, dom.id).await {
        Ok(o) => o,
        Err(e) => return fail(&e),
    };
    // A domain administrator may change only the password rules; every other value must stay as it is.
    if !p.scope.is_platform() {
        let same = DomainSettings {
            password_min_length: old.password_min_length,
            password_classes: old.password_classes,
            sender_allow: old.sender_allow.clone(),
            sender_block: old.sender_block.clone(),
            footer_text: old.footer_text.clone(),
            blocked_extensions: old.blocked_extensions.clone(),
            ..new.clone()
        } == old;
        if !same {
            return err(403, "forbidden");
        }
    }
    if let Some(plan) = new.default_plan.as_deref() {
        match s.plans.list().await {
            Ok(plans) if plans.iter().any(|x| x.name == plan) => {}
            Ok(_) => return err(400, "invalid"),
            Err(e) => return fail(&e),
        }
    }
    let mut set = Map::new();
    let mut remove: Vec<String> = Vec::new();
    for (key, value) in [
        ("default_plan", new.default_plan.clone().map(Value::from)),
        ("max_accounts", new.max_accounts.map(Value::from)),
        ("quota_bytes", new.quota_bytes.map(Value::from)),
        ("max_message_bytes", new.max_message_bytes.map(Value::from)),
        ("password_min_length", new.password_min_length.map(Value::from)),
        ("password_classes", new.password_classes.map(Value::from)),
        ("footer_text", new.footer_text.clone().map(Value::from)),
        ("blocked_extensions", (!new.blocked_extensions.is_empty()).then(|| Value::from(new.blocked_extensions.clone()))),
        ("sender_allow", (!new.sender_allow.is_empty()).then(|| Value::from(new.sender_allow.clone()))),
        ("sender_block", (!new.sender_block.is_empty()).then(|| Value::from(new.sender_block.clone()))),
    ] {
        match value {
            Some(v) => {
                set.insert(key.to_owned(), v);
            }
            None => remove.push(key.to_owned()),
        }
    }
    if let Err(e) = s.plans.set_domain_keys(dom.id.0, &remove, &Value::Object(set).to_string()).await {
        return fail(&e);
    }
    Response::json(200, &json!(new)).with_header("cache-control", "no-store")
}

/// The platform-wide limits: the last layer under the plan, the domain and the account, so a change reaches the next delivery without a restart. A missing one is `null`
/// (the server's own default applies).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlatformSettings {
    pub quota_bytes: Option<u64>,
    pub max_message_bytes: Option<u64>,
    pub messages_per_hour: Option<u64>,
    /// What happens to a message the virus scanner found a virus in: `reject` (refused at DATA) or `quarantine` (filed in the Spam folder, tagged); `null`: reject.
    pub virus_action: Option<String>,
    /// File extensions (without a dot) of attachments refused everywhere (a domain may add its own).
    #[serde(default)]
    pub blocked_extensions: Vec<String>,
}

impl PlatformSettings {
    fn from_config(config: &str) -> PlatformSettings {
        let v: Value = serde_json::from_str(config).unwrap_or(Value::Null);
        let n = |k: &str| v.get(k).and_then(Value::as_u64);
        PlatformSettings { quota_bytes: n("quota_bytes"), max_message_bytes: n("max_message_bytes"), messages_per_hour: n("messages_per_hour"), virus_action: v.get("virus_action").and_then(Value::as_str).map(str::to_owned), blocked_extensions: text_list(&v, "blocked_extensions") }
    }

    fn invalid(&self) -> bool {
        self.quota_bytes.is_some_and(|n| n == 0 || n > MAX_BYTES)
            || self.max_message_bytes.is_some_and(|n| n == 0 || n > MAX_BYTES)
            || self.messages_per_hour.is_some_and(|n| n == 0 || n > 1_000_000)
            || self.virus_action.as_deref().is_some_and(|a| a != "reject" && a != "quarantine")
            || clean_extensions(&self.blocked_extensions).is_none()
    }
}

async fn get_platform(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, false).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if !p.scope.is_platform() {
        return err(403, "forbidden");
    }
    let Some(s) = app.services.as_ref() else { return err(503, "unavailable") };
    match s.plans.global_config().await {
        Ok(c) => Response::json(200, &PlatformSettings::from_config(&c)).with_header("cache-control", "no-store"),
        Err(e) => fail(&e),
    }
}

async fn put_platform(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if !p.scope.is_platform() {
        return err(403, "forbidden");
    }
    let Ok(mut new) = serde_json::from_slice::<PlatformSettings>(&req.body) else { return err(400, "invalid") };
    if new.invalid() {
        return err(400, "invalid");
    }
    new.blocked_extensions = clean_extensions(&new.blocked_extensions).unwrap_or_default();
    let Some(s) = app.services.as_ref() else { return err(503, "unavailable") };
    let mut set = Map::new();
    let mut remove: Vec<String> = Vec::new();
    for (key, value) in [("quota_bytes", new.quota_bytes), ("max_message_bytes", new.max_message_bytes), ("messages_per_hour", new.messages_per_hour)] {
        match value {
            Some(v) => {
                set.insert(key.to_owned(), Value::from(v));
            }
            None => remove.push(key.to_owned()),
        }
    }
    if new.blocked_extensions.is_empty() {
        remove.push("blocked_extensions".to_owned());
    } else {
        set.insert("blocked_extensions".to_owned(), Value::from(new.blocked_extensions.clone()));
    }
    match &new.virus_action {
        Some(a) => {
            set.insert("virus_action".to_owned(), Value::from(a.clone()));
        }
        None => remove.push("virus_action".to_owned()),
    }
    if let Err(e) = s.plans.set_global_keys(&remove, &Value::Object(set).to_string()).await {
        return fail(&e);
    }
    Response::json(200, &json!(new)).with_header("cache-control", "no-store")
}

/// The routes of this part of the API.
pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    let a = app.clone();
    let router = router.route(Method::Get, "/api/v1/settings", handler(move |r: Request| {
        let a = a.clone();
        async move { get_platform(&a, r).await }
    }));
    let a = app.clone();
    let router = router.route(Method::Put, "/api/v1/settings", handler(move |r: Request| {
        let a = a.clone();
        async move { put_platform(&a, r).await }
    }));
    let a = app.clone();
    let router = router.route(Method::Get, "/api/v1/domains/{d}/settings", handler(move |r: Request| {
        let a = a.clone();
        async move { get(&a, r).await }
    }));
    router.route(Method::Put, "/api/v1/domains/{d}/settings", handler(move |r: Request| {
        let a = app.clone();
        async move { put(&a, r).await }
    }))
}

#[cfg(test)]
mod tests {
    use super::DomainSettings;

    #[test]
    fn a_password_meets_the_length_and_the_kinds_of_characters_the_domain_asks_for() {
        let rules = DomainSettings { password_min_length: Some(12), password_classes: Some(3), ..DomainSettings::default() };
        assert!(rules.password_problem("short1A").is_some());
        assert!(rules.password_problem("alllowercaseletters").is_some(), "one kind of character");
        assert!(rules.password_problem("Lowercase and 123").is_none());
        // Without rules the floor of eight characters applies, and nothing else.
        assert!(DomainSettings::default().password_problem("1234567").is_some());
        assert!(DomainSettings::default().password_problem("12345678").is_none());
    }
}
