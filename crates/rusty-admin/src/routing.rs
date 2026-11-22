//! Where a domain's mail goes besides its accounts (docs/design-routing.md): distribution lists and their members, the catch-all address, and the domain aliases. Domain administrators manage
//! the lists and the catch-all of their own domain; the alias domains are the platform administrators' (an alias claims a name on the internet).

use std::sync::Arc;

use rusty_core::Error;
use rusty_directory::{parse_address, Resolved};
use rusty_http::{handler, Method, Request, Response, Router};
use rusty_store::ListInfo;
use serde::{Deserialize, Serialize};

use crate::api::{fail, AdminApp};
use crate::manage::{platform_only, scoped_domain};

fn err(status: u16, code: &'static str) -> Response {
    Response::error(status, code).with_header("cache-control", "no-store")
}

fn json<T: Serialize>(status: u16, v: &T) -> Response {
    Response::json(status, v).with_header("cache-control", "no-store")
}

#[derive(Serialize)]
struct ListJson {
    local: String,
    address: String,
    members: i64,
}

fn list_json(l: &ListInfo, domain: &str) -> ListJson {
    ListJson { local: l.local.clone(), address: format!("{}@{domain}", l.local), members: l.members }
}

#[derive(Serialize)]
struct RoutingJson {
    lists: Vec<ListJson>,
    catch_all: Option<String>,
    domain_aliases: Vec<String>,
}

#[derive(Serialize)]
struct MemberJson {
    seq: i64,
    address: String,
    external: bool,
}

#[derive(Serialize)]
struct MembersJson {
    items: Vec<MemberJson>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewList {
    local: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewMember {
    address: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutCatchAll {
    local: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewDomainAlias {
    name: String,
}

macro_rules! begin {
    ($app:expr, $req:expr, $mutating:expr) => {{
        let (_, p) = match $app.require(&$req, $mutating).await {
            Ok(x) => x,
            Err(r) => return r,
        };
        let Some(s) = $app.services.as_ref() else { return err(503, "unavailable") };
        let dom = match scoped_domain($app, &p, $req.param("d").unwrap_or_default()).await {
            Ok(d) => d,
            Err(r) => return r,
        };
        (p, s, dom)
    }};
}

async fn get_routing(app: &AdminApp, req: Request) -> Response {
    let (_p, s, dom) = begin!(app, req, false);
    let r = &s.routing;
    match tokio::try_join!(r.lists(dom.id), r.catch_all(dom.id), r.domain_aliases(dom.id)) {
        Ok((lists, catch_all, domain_aliases)) => json(200, &RoutingJson { lists: lists.iter().map(|l| list_json(l, &dom.name)).collect(), catch_all, domain_aliases }),
        Err(e) => fail(&e),
    }
}

async fn create_list(app: &AdminApp, req: Request) -> Response {
    let (_p, _s, dom) = begin!(app, req, true);
    let Ok(b) = serde_json::from_slice::<NewList>(&req.body) else { return err(400, "invalid") };
    match app.directory.create_list(&b.local, dom.id).await {
        Ok(_) => {
            let local = parse_address(&format!("{}@{}", b.local, dom.name)).map(|(l, _)| l).unwrap_or(b.local);
            json(201, &ListJson { address: format!("{local}@{}", dom.name), local, members: 0 })
        }
        Err(e) => fail(&e),
    }
}

async fn delete_list(app: &AdminApp, req: Request) -> Response {
    let (_p, s, dom) = begin!(app, req, true);
    match s.routing.delete_list(dom.id, req.param("l").unwrap_or_default()).await {
        Ok(()) => json(200, &serde_json::json!({})),
        Err(e) => fail(&e),
    }
}

async fn members(app: &AdminApp, req: Request) -> Response {
    let (_p, s, dom) = begin!(app, req, false);
    let list = match s.routing.find_list(dom.id, req.param("l").unwrap_or_default()).await {
        Ok(Some(l)) => l,
        Ok(None) => return err(404, "not_found"),
        Err(e) => return fail(&e),
    };
    match s.routing.members(list.id).await {
        Ok(items) => json(200, &MembersJson { items: items.into_iter().map(|m| MemberJson { seq: m.seq, address: m.address, external: m.external }).collect() }),
        Err(e) => fail(&e),
    }
}

/// Add a member: a local account (of any hosted domain). An address outside this server is refused for now (`400 invalid`): mail to it would have to be forwarded, which needs the outbound
/// queue and loop protection (docs/design-routing.md, "Not done").
async fn add_member(app: &AdminApp, req: Request) -> Response {
    let (_p, s, dom) = begin!(app, req, true);
    let Ok(b) = serde_json::from_slice::<NewMember>(&req.body) else { return err(400, "invalid") };
    let list = match s.routing.find_list(dom.id, req.param("l").unwrap_or_default()).await {
        Ok(Some(l)) => l,
        Ok(None) => return err(404, "not_found"),
        Err(e) => return fail(&e),
    };
    let account = match app.directory.find_account(&b.address).await {
        Ok(Some(a)) => a,
        Ok(None) | Err(Error::Invalid(_)) => return err(400, "invalid"),
        Err(e) => return fail(&e),
    };
    if let Err(e) = app.directory.add_list_member(list.id, Some(account.id), None).await {
        return fail(&e);
    }
    match s.routing.members(list.id).await {
        Ok(items) => json(201, &MembersJson { items: items.into_iter().map(|m| MemberJson { seq: m.seq, address: m.address, external: m.external }).collect() }),
        Err(e) => fail(&e),
    }
}

async fn remove_member(app: &AdminApp, req: Request) -> Response {
    let (_p, s, dom) = begin!(app, req, true);
    let Ok(seq) = req.param("n").unwrap_or_default().parse::<i64>() else { return err(404, "not_found") };
    let list = match s.routing.find_list(dom.id, req.param("l").unwrap_or_default()).await {
        Ok(Some(l)) => l,
        Ok(None) => return err(404, "not_found"),
        Err(e) => return fail(&e),
    };
    match s.routing.remove_member(list.id, seq).await {
        Ok(()) => json(200, &serde_json::json!({})),
        Err(e) => fail(&e),
    }
}

/// Set or clear the catch-all: the local part (an account, alias or list of this domain) that takes the mail of addresses that do not exist.
async fn put_catch_all(app: &AdminApp, req: Request) -> Response {
    let (_p, s, dom) = begin!(app, req, true);
    let Ok(b) = serde_json::from_slice::<PutCatchAll>(&req.body) else { return err(400, "invalid") };
    let local = match b.local.as_deref().map(|l| parse_address(&format!("{l}@{}", dom.name))) {
        None => None,
        Some(Ok((l, _))) => Some(l),
        Some(Err(_)) => return err(400, "invalid"),
    };
    if let Some(l) = &local {
        // It must name something that exists in this domain, or the setting would silently drop mail.
        match app.directory.resolve(&format!("{l}@{}", dom.name)).await {
            Ok(Resolved::Account(_) | Resolved::List(_) | Resolved::External(_)) => {}
            Err(Error::NotFound(_)) => return err(400, "invalid"),
            Err(e) => return fail(&e),
        }
    }
    match s.routing.set_catch_all(dom.id, local.as_deref()).await {
        Ok(()) => json(200, &serde_json::json!({ "catch_all": local })),
        Err(e) => fail(&e),
    }
}

async fn add_domain_alias(app: &AdminApp, req: Request) -> Response {
    let (p, s, dom) = begin!(app, req, true);
    if let Err(r) = platform_only(&p) {
        return r;
    }
    let Ok(b) = serde_json::from_slice::<NewDomainAlias>(&req.body) else { return err(400, "invalid") };
    let Ok((_, name)) = parse_address(&format!("x@{}", b.name)) else { return err(400, "invalid") };
    match s.routing.add_domain_alias(dom.id, &name).await {
        Ok(()) => json(201, &serde_json::json!({ "name": name })),
        Err(e) => fail(&e),
    }
}

async fn remove_domain_alias(app: &AdminApp, req: Request) -> Response {
    let (p, s, dom) = begin!(app, req, true);
    if let Err(r) = platform_only(&p) {
        return r;
    }
    let Ok((_, name)) = parse_address(&format!("x@{}", req.param("n").unwrap_or_default())) else { return err(404, "not_found") };
    match s.routing.remove_domain_alias(dom.id, &name).await {
        Ok(()) => json(200, &serde_json::json!({})),
        Err(e) => fail(&e),
    }
}

macro_rules! route {
    ($router:expr, $app:expr, $method:expr, $path:expr, $f:ident) => {{
        let a = $app.clone();
        $router.route($method, $path, handler(move |r: Request| {
            let a = a.clone();
            async move { $f(&a, r).await }
        }))
    }};
}

/// The routes of this part of the API.
pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    let router = route!(router, app, Method::Get, "/api/v1/domains/{d}/routing", get_routing);
    let router = route!(router, app, Method::Post, "/api/v1/domains/{d}/lists", create_list);
    let router = route!(router, app, Method::Delete, "/api/v1/domains/{d}/lists/{l}", delete_list);
    let router = route!(router, app, Method::Get, "/api/v1/domains/{d}/lists/{l}/members", members);
    let router = route!(router, app, Method::Post, "/api/v1/domains/{d}/lists/{l}/members", add_member);
    let router = route!(router, app, Method::Delete, "/api/v1/domains/{d}/lists/{l}/members/{n}", remove_member);
    let router = route!(router, app, Method::Put, "/api/v1/domains/{d}/catch-all", put_catch_all);
    let router = route!(router, app, Method::Post, "/api/v1/domains/{d}/domain-aliases", add_domain_alias);
    route!(router, app, Method::Delete, "/api/v1/domains/{d}/domain-aliases/{n}", remove_domain_alias)
}
