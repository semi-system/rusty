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
    let Ok(new) = serde_json::from_slice::<DomainSettings>(&req.body) else { return err(400, "invalid") };
    if new.invalid() {
        return err(400, "invalid");
    }
    let Some(s) = app.services.as_ref() else { return err(503, "unavailable") };
    let old = match of(app, dom.id).await {
        Ok(o) => o,
        Err(e) => return fail(&e),
    };
    // A domain administrator may change only the password rules; every other value must stay as it is.
    if !p.scope.is_platform() {
        let same = DomainSettings { password_min_length: old.password_min_length, password_classes: old.password_classes, ..new.clone() } == old;
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

/// The routes of this part of the API.
pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
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
