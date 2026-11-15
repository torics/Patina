//! The daemon's security rules over every route it has. The routes are read from the router's own source, so a
//! route added later is covered the day it is added, and forgetting to protect it fails here.
mod common;
use common::*;
use patina_providers::mock::Script;
use reqwest::Method;
use serde_json::json;

#[derive(Debug, Clone)]
struct Route {
    method: Method,
    /// The path with every `{param}` filled in.
    path: String,
    /// Reachable without a session: health and sign-in.
    public: bool,
}

/// Every route and method in `router()`, read from `src/lib.rs`.
fn routes(fill: &dyn Fn(&str) -> String) -> Vec<Route> {
    let src = include_str!("../src/lib.rs");
    let body = &src[src.find("pub fn router(").unwrap()..];
    let body = &body[..body.find("\n}\n").unwrap()];
    let mut out = vec![];
    // The router has three parts: protected routes, public sign-in routes, and top-level health checks.
    let (mut public, mut prefix) = (false, "/api/v1");
    for line in body.lines() {
        if line.contains("let public") {
            public = true;
        }
        if line.contains("let api") {
            prefix = "";
        }
        let Some(start) = line.find(".route(\"") else { continue };
        let rest = &line[start + 8..];
        let path = &rest[..rest.find('"').unwrap()];
        let handlers = &rest[path.len()..];
        for (name, method) in [
            ("get", Method::GET),
            ("post", Method::POST),
            ("put", Method::PUT),
            ("patch", Method::PATCH),
            ("delete", Method::DELETE),
        ] {
            let call = format!("{name}(");
            let found = handlers.match_indices(&call).any(|(i, _)| {
                // `get(` but not `budget(`.
                i == 0 || !handlers[..i].ends_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
            });
            if found {
                out.push(Route {
                    method,
                    path: format!("{prefix}{}", fill(path)),
                    public: public || prefix.is_empty(),
                });
            }
        }
    }
    out
}

fn with_x(path: &str) -> String {
    let mut out = String::new();
    let mut rest = path;
    while let Some(open) = rest.find('{') {
        let close = rest[open..].find('}').unwrap() + open;
        out.push_str(&rest[..open]);
        out.push('x');
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

#[test]
fn the_route_list_is_read_whole() {
    let all = routes(&with_x);
    assert!(
        all.len() >= 55,
        "only {} routes were found; the parser no longer matches the router",
        all.len()
    );
    let has = |m: Method, p: &str| all.iter().any(|r| r.method == m && r.path == p);
    assert!(has(Method::PUT, "/api/v1/mcp/x/trust") && has(Method::PATCH, "/api/v1/runs/x/budgets"));
    assert!(has(Method::DELETE, "/api/v1/projects/x") && has(Method::GET, "/healthz"));
    assert!(
        all.iter().filter(|r| r.public).count() == 6,
        "{:?}",
        all.iter().filter(|r| r.public).collect::<Vec<_>>()
    );
}

async fn status(d: &Daemon, r: &Route, headers: &[(&str, &str)]) -> u16 {
    let url = format!("{}{}", d.origin, r.path);
    let mut req = d.http.request(r.method.clone(), url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    if r.method != Method::GET {
        req = req.json(&json!({}));
    }
    req.send().await.unwrap().status().as_u16()
}

#[tokio::test]
async fn every_route_needs_a_session_and_every_change_needs_the_csrf_token_and_origin() {
    let mut d = start(vec![], false).await;
    let all = routes(&with_x);
    // No session: everything but health and sign-in is refused, whatever the method.
    for r in all.iter().filter(|r| !r.public) {
        let origin = d.origin.clone();
        assert_eq!(status(&d, r, &[("origin", &origin)]).await, 401, "{} {}", r.method, r.path);
    }
    d.sign_in().await;
    let (cookie, csrf, origin) = (d.cookie.clone(), d.csrf.clone(), d.origin.clone());
    for r in all.iter().filter(|r| r.method != Method::GET) {
        let what = format!("{} {}", r.method, r.path);
        if !r.public {
            // A session alone is not enough to change anything: the CSRF token and the Origin must match too.
            assert_eq!(
                status(&d, r, &[("cookie", &cookie), ("origin", &origin)]).await,
                403,
                "{what} without the token"
            );
            assert_eq!(
                status(&d, r, &[("cookie", &cookie), ("origin", &origin), ("x-patina-csrf", "00")]).await,
                403,
                "{what} with a wrong token"
            );
        }
        assert_eq!(
            status(
                &d,
                r,
                &[("cookie", &cookie), ("origin", "https://evil.example"), ("x-patina-csrf", &csrf)]
            )
            .await,
            403,
            "{what} from another origin"
        );
        assert_eq!(
            status(&d, r, &[("cookie", &cookie), ("x-patina-csrf", &csrf)]).await,
            403,
            "{what} without an Origin"
        );
    }
    // The session still works, so none of the above was refused for another reason.
    assert_eq!(d.get("/providers").await.0, 200);
}

#[tokio::test]
async fn a_host_name_that_is_not_allowed_is_refused_on_every_route() {
    let mut d = start(vec![], false).await;
    d.sign_in().await;
    let (cookie, csrf, origin) = (d.cookie.clone(), d.csrf.clone(), d.origin.clone());
    let mut all = routes(&with_x);
    all.push(Route {
        method: Method::GET,
        path: "/".into(),
        public: true,
    });
    for r in &all {
        let headers = [
            ("cookie", cookie.as_str()),
            ("origin", origin.as_str()),
            ("x-patina-csrf", csrf.as_str()),
            ("host", "attacker.example"),
        ];
        assert_eq!(status(&d, r, &headers).await, 403, "{} {}", r.method, r.path);
    }
}

#[tokio::test]
async fn no_route_returns_a_stored_secret() {
    const MCP_SECRET: &str = "mcp-token-SECRETVALUE-55aa";
    let mut d = start(vec![Script::text("hello there")], false).await;
    d.sign_in().await;
    d.seed().await;
    let (s, b) = d
        .post(
            "/mcp",
            json!({"id": "search", "transport": "stdio", "command": "patina-no-such-mcp-server",
                   "env": [{"name": "API_TOKEN", "value": MCP_SECRET}], "headers": []}),
        )
        .await;
    assert_eq!(s, 200, "{b}");
    let run = d.run("say hello").await;
    d.wait_status(&run, "completed").await;
    let fill = |path: &str| {
        path.replace("/providers/{id}", "/providers/p1")
            .replace("/agents/{id}", "/agents/a1")
            .replace("/runs/{id}", &format!("/runs/{run}"))
            .replace("/mcp/{id}", "/mcp/search")
            .replace("/projects/{id}", "/projects/site")
            .replace("/teams/{id}", "/teams/t1")
            .replace("{approval_id}", "x")
            .replace("{id}", "x")
    };
    let mut seen = 0;
    for r in routes(&fill).iter().filter(|r| r.method == Method::GET) {
        let url = format!("{}{}", d.origin, r.path);
        let body = d
            .http
            .get(&url)
            .header("cookie", &d.cookie)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            !body.contains(KEY) && !body.contains("SECRETVALUE"),
            "GET {} returned a secret: {body}",
            r.path
        );
        seen += 1;
    }
    assert!(seen >= 25, "{seen}");
    // The MCP secret is stored encrypted, not in the config file.
    let toml = std::fs::read_to_string(d.dir.path().join("config/mcp.toml")).unwrap();
    assert!(!toml.contains(MCP_SECRET), "{toml}");
}
