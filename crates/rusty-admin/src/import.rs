//! Making many accounts from a CSV (docs/design-import.md): one line per account, `address-or-name, password, plan`, checked line by line, with a dry run that changes nothing. The same rules as
//! the single "new account" form apply to every line, and the first account of a domain gets the role aliases (postmaster, abuse) as it does there.

use std::collections::HashSet;
use std::sync::Arc;

use rusty_core::Error;
use rusty_http::{handler, Method, Request, Response, Router};
use serde::{Deserialize, Serialize};

use crate::api::{fail, AdminApp};
use crate::manage::scoped_domain;

/// Most lines one request takes (the request body is at most 64 KiB; the web sends a larger file in several requests).
const MAX_LINES: usize = 500;
const MAX_LOCAL: usize = 64;
const MIN_PASSWORD: usize = 8;
const MAX_PASSWORD: usize = 1024;
const REQUIRED_ALIASES: [&str; 2] = ["postmaster", "abuse"];

fn err(status: u16, code: &'static str) -> Response {
    Response::error(status, code).with_header("cache-control", "no-store")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportBody {
    csv: String,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Serialize)]
struct RowJson {
    line: usize,
    address: String,
    /// `created` (or `would_create` in a dry run), `exists` or `invalid`.
    result: &'static str,
    reason: Option<&'static str>,
}

#[derive(Serialize)]
struct ImportJson {
    dry_run: bool,
    created: usize,
    exists: usize,
    invalid: usize,
    rows: Vec<RowJson>,
}

/// The fields of one CSV line: commas separate, a field may be in double quotes (with `""` for a quote). No line continues on the next one.
fn fields(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            ('"', _) => quoted = !quoted,
            (',', false) => out.push(std::mem::take(&mut cur).trim().to_owned()),
            (c, _) => cur.push(c),
        }
    }
    out.push(cur.trim().to_owned());
    out
}

/// The lines that carry an account, with their line numbers: blank lines and a header line (`address,password,plan`) are left out.
fn data_lines(csv: &str) -> Vec<(usize, &str)> {
    let mut lines: Vec<(usize, &str)> = Vec::new();
    for (index, line) in csv.lines().enumerate() {
        if !line.trim().is_empty() {
            lines.push((index + 1, line));
        }
    }
    let header = lines.first().is_some_and(|(_, l)| {
        fields(l).first().is_some_and(|f| matches!(f.to_ascii_lowercase().as_str(), "address" | "email" | "local" | "name" | "account"))
    });
    if header {
        lines.remove(0);
    }
    lines
}

async fn import(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, true).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let dom = match scoped_domain(app, &p, req.param("d").unwrap_or_default()).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let Ok(b) = serde_json::from_slice::<ImportBody>(&req.body) else { return err(400, "invalid") };
    let Some(s) = app.services.as_ref() else { return err(503, "unavailable") };
    let plans = match s.plans.list().await {
        Ok(p) => p,
        Err(e) => return fail(&e),
    };
    let was_empty = match app.directory.list_accounts(dom.id, None, 1).await {
        Ok(rows) => rows.is_empty(),
        Err(e) => return fail(&e),
    };
    // The rules of the domain apply to every line (docs/design-domain-settings.md): its password rules, its default plan, and room for the accounts it may still have.
    let settings = match crate::settings::of(app, dom.id).await {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    let mut room = match settings.max_accounts {
        Some(max) => match app.directory.count_accounts(dom.id, &rusty_directory::ListFilter::default()).await {
            Ok(n) => Some(usize::try_from(max).unwrap_or(usize::MAX).saturating_sub(usize::try_from(n).unwrap_or(0))),
            Err(e) => return fail(&e),
        },
        None => None,
    };

    let mut rows: Vec<RowJson> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut first_created: Option<(String, rusty_core::AccountId)> = None;
    let lines = data_lines(&b.csv);
    if lines.len() > MAX_LINES {
        return err(400, "invalid");
    }
    for (n, line) in lines {
        let cols = fields(line);
        let name = cols.first().map(String::as_str).unwrap_or_default();
        let (local, domain_part) = name.rsplit_once('@').map_or((name, None), |(l, d)| (l, Some(d)));
        let address = format!("{}@{}", local.to_ascii_lowercase(), dom.name);
        let password = cols.get(1).map(String::as_str).filter(|x| !x.is_empty());
        let plan = cols.get(2).map(String::as_str).filter(|x| !x.is_empty());
        macro_rules! bad {
            ($reason:expr) => {
                rows.push(RowJson { line: n, address: address.clone(), result: "invalid", reason: Some($reason) })
            };
        }
        if local.is_empty() || local.len() > MAX_LOCAL || local.contains(|c: char| c.is_whitespace() || c == '@') {
            bad!("the name is not valid");
            continue;
        }
        if domain_part.is_some_and(|d| !d.eq_ignore_ascii_case(&dom.name)) {
            bad!("the address is not in this domain");
            continue;
        }
        if password.is_some_and(|pw| pw.chars().count() < MIN_PASSWORD || pw.len() > MAX_PASSWORD) {
            bad!("the password is too short");
            continue;
        }
        if password.is_some_and(|pw| settings.password_problem(pw).is_some()) {
            bad!("the password does not meet the rules of the domain");
            continue;
        }
        if let Some(name) = plan {
            if !plans.iter().any(|x| x.name == name) {
                bad!("no such plan");
                continue;
            }
        }
        if !seen.insert(address.clone()) {
            bad!("the address is listed twice");
            continue;
        }
        match app.directory.find_account(&address).await {
            Ok(Some(_)) => {
                rows.push(RowJson { line: n, address, result: "exists", reason: None });
                continue;
            }
            Ok(None) => {}
            Err(Error::Invalid(_)) => {
                bad!("the name is not valid");
                continue;
            }
            Err(e) => return fail(&e),
        }
        if b.dry_run {
            rows.push(RowJson { line: n, address, result: "would_create", reason: None });
            continue;
        }
        if room == Some(0) {
            bad!("the domain has reached its limit of accounts");
            continue;
        }
        match app.directory.create_account(local, dom.id, password, None).await {
            Ok(id) => {
                if let Some(r) = room.as_mut() {
                    *r -= 1;
                }
                let plan = plan.or(settings.default_plan.as_deref());
                if let Some(name) = plan {
                    if let Err(e) = s.plans.set_account_plan(id, Some(name)).await {
                        return fail(&e);
                    }
                }
                if first_created.is_none() {
                    first_created = Some((local.to_ascii_lowercase(), id));
                }
                rows.push(RowJson { line: n, address, result: "created", reason: None });
            }
            Err(Error::Conflict(_)) => rows.push(RowJson { line: n, address, result: "exists", reason: None }),
            Err(Error::Invalid(_)) => bad!("the name is not valid"),
            Err(e) => return fail(&e),
        }
    }
    // The first account of a domain gets the role addresses, as in the single form.
    if was_empty {
        if let Some((local, id)) = first_created {
            for alias in REQUIRED_ALIASES {
                if alias == local {
                    continue;
                }
                match app.directory.add_alias(alias, dom.id, id).await {
                    Ok(()) | Err(Error::Conflict(_)) => {}
                    Err(e) => return fail(&e),
                }
            }
        }
    }
    let count = |r: &str| rows.iter().filter(|x| x.result == r).count();
    let out = ImportJson { dry_run: b.dry_run, created: count("created") + count("would_create"), exists: count("exists"), invalid: count("invalid"), rows };
    Response::json(200, &out).with_header("cache-control", "no-store")
}

/// The routes of this part of the API.
pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    router.route(Method::Post, "/api/v1/domains/{d}/accounts/import", handler(move |r: Request| {
        let a = app.clone();
        async move { import(&a, r).await }
    }))
}

#[cfg(test)]
mod tests {
    use super::fields;

    #[test]
    fn a_line_splits_at_commas_outside_quotes() {
        assert_eq!(fields("alice, secret-pass ,gold"), vec!["alice", "secret-pass", "gold"]);
        assert_eq!(fields("\"bob\",\"a,b\"\"c\""), vec!["bob", "a,b\"c"]);
        assert_eq!(fields("carol"), vec!["carol"]);
        assert_eq!(fields(",,"), vec!["", "", ""]);
    }
}
