//! The MCP client against a stdio server written without any MCP library, and a Streamable HTTP server built
//! with the official Rust SDK.
use async_trait::async_trait;
use patina_core::tool::{Tool, ToolKind};
use patina_tools::mcp::{Launch, McpHub, McpSource, connect};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn python() -> Option<String> {
    ["python3", "python"]
        .into_iter()
        .find(|p| {
            std::process::Command::new(p)
                .arg("--version")
                .output()
                .is_ok_and(|o| o.status.success())
        })
        .map(String::from)
}

fn stdio_launch(env: &[(&str, &str)]) -> Option<Launch> {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mcp_stdio_server.py");
    Some(Launch::Stdio {
        command: python()?,
        args: vec!["-u".into(), script.into()],
        cwd: None,
        env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
    })
}

/// A source with fixed launches and trust the test can change.
struct Fixed {
    launches: HashMap<String, Launch>,
    trusted: Mutex<HashSet<(String, String)>>,
    timeout: Duration,
}

#[async_trait]
impl McpSource for Fixed {
    async fn launch(&self, server: &str) -> Result<Launch, String> {
        self.launches.get(server).cloned().ok_or_else(|| "no such server".into())
    }
    fn trusted(&self, server: &str, tool: &str) -> bool {
        self.trusted.lock().unwrap().contains(&(server.to_string(), tool.to_string()))
    }
    fn timeout(&self, _: &str) -> Duration {
        self.timeout
    }
}

fn hub(launches: Vec<(&str, Launch)>) -> (Arc<McpHub>, Arc<Fixed>) {
    let source = Arc::new(Fixed {
        launches: launches.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        trusted: Mutex::default(),
        timeout: Duration::from_secs(20),
    });
    (McpHub::new(source.clone()), source)
}

fn find(tools: &[Arc<dyn Tool>], name: &str) -> Arc<dyn Tool> {
    tools
        .iter()
        .find(|t| t.spec().name == name)
        .cloned()
        .unwrap_or_else(|| panic!("no tool {name}"))
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    // A zombie has exited; only a running process counts.
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|s| !s.split(") ").nth(1).is_some_and(|rest| rest.starts_with('Z')))
        .unwrap_or(false)
}

#[tokio::test]
async fn a_stdio_server_is_listed_and_called_with_a_scrubbed_environment() {
    let Some(launch) = stdio_launch(&[("GREETING", "hello from mcp.toml")]) else {
        eprintln!("skipped: no python on this machine");
        return;
    };
    // Cargo gives the test process this variable; a server must not inherit it.
    assert!(std::env::var("CARGO_MANIFEST_DIR").is_ok());
    let (hub, source) = hub(vec![("local", launch)]);
    let tools = hub.tools("local", |_| true).await.unwrap();
    let names: Vec<String> = tools.iter().map(|t| t.spec().name).collect();
    assert_eq!(
        names,
        [
            "mcp__local__echo",
            "mcp__local__env",
            "mcp__local__pid",
            "mcp__local__spawn_child",
            "mcp__local__fail",
            "mcp__local__exit",
            "mcp__local__dotted_name"
        ]
    );
    let echo = find(&tools, "mcp__local__echo");
    assert_eq!(echo.spec().description, "Return the text it is given.");
    assert!(echo.spec().schema.get("$schema").is_none(), "{}", echo.spec().schema);

    let out = echo.call(json!({"text": "hi there"})).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.starts_with("[Result of MCP tool \"echo\" of server \"local\""),
        "{}",
        out.content
    );
    assert!(out.content.ends_with("\nhi there"), "{}", out.content);

    let env = find(&tools, "mcp__local__env");
    assert!(
        env.call(json!({"name": "GREETING"}))
            .await
            .content
            .ends_with("\nhello from mcp.toml")
    );
    assert!(env.call(json!({"name": "CARGO_MANIFEST_DIR"})).await.content.ends_with("\n<unset>"));
    assert!(!env.call(json!({"name": "PATH"})).await.content.ends_with("\n<unset>"));

    let fail = find(&tools, "mcp__local__fail").call(json!({})).await;
    assert!(fail.is_error && fail.content.ends_with("it went wrong"), "{}", fail.content);
    assert_eq!(
        find(&tools, "mcp__local__dotted_name").call(json!({})).await.content.lines().last(),
        Some("dotted")
    );

    // Trust is read at each decision, so trusting a tool applies to a run already going.
    assert_eq!(echo.kind(), ToolKind::External { trusted: false });
    source.trusted.lock().unwrap().insert(("local".into(), "echo".into()));
    assert_eq!(echo.kind(), ToolKind::External { trusted: true });
    assert_eq!(env.kind(), ToolKind::External { trusted: false });

    // Only the tools an agent was granted become its tools.
    let some = hub.tools("local", |n| n == "mcp__local__echo").await.unwrap();
    assert_eq!(some.len(), 1);
    assert!(hub.is_connected("local"));
}

#[cfg(unix)]
#[tokio::test]
async fn closing_a_connection_ends_the_server_and_what_it_started() {
    let Some(launch) = stdio_launch(&[]) else {
        eprintln!("skipped: no python on this machine");
        return;
    };
    let (hub, _) = hub(vec![("local", launch)]);
    let tools = hub.tools("local", |_| true).await.unwrap();
    let pid: u32 = find(&tools, "mcp__local__pid")
        .call(json!({}))
        .await
        .content
        .lines()
        .last()
        .unwrap()
        .parse()
        .unwrap();
    let child: u32 = find(&tools, "mcp__local__spawn_child")
        .call(json!({}))
        .await
        .content
        .lines()
        .last()
        .unwrap()
        .parse()
        .unwrap();
    assert!(alive(pid) && alive(child));
    hub.close_all().await;
    for _ in 0..100 {
        if !alive(pid) && !alive(child) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!alive(pid), "the server outlived its connection");
    assert!(!alive(child), "the server's child outlived its connection");
    assert!(!hub.is_connected("local"));
}

#[tokio::test]
async fn a_server_that_died_is_started_again_on_a_later_call() {
    let Some(launch) = stdio_launch(&[]) else {
        eprintln!("skipped: no python on this machine");
        return;
    };
    let (hub, _) = hub(vec![("local", launch)]);
    let tools = hub.tools("local", |_| true).await.unwrap();
    let pid = |out: String| out.lines().last().unwrap().to_string();
    let first = pid(find(&tools, "mcp__local__pid").call(json!({})).await.content);
    let exit = find(&tools, "mcp__local__exit").call(json!({})).await;
    assert!(exit.is_error, "a server that exits mid-call is an error: {}", exit.content);
    let mut again = None;
    for _ in 0..50 {
        let out = find(&tools, "mcp__local__pid").call(json!({})).await;
        if !out.is_error {
            again = Some(pid(out.content));
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let again = again.expect("the server was not started again");
    assert_ne!(again, first);
}

#[tokio::test]
async fn a_server_that_cannot_start_explains_why() {
    let missing = Launch::Stdio {
        command: "patina-no-such-mcp-server".into(),
        args: vec![],
        cwd: None,
        env: vec![],
    };
    let err = connect("gone", &missing).await.err().unwrap();
    assert!(err.contains("could not start"), "{err}");

    let Some(python) = python() else { return };
    let crashing = Launch::Stdio {
        command: python,
        args: vec![
            "-c".into(),
            "import sys; sys.stderr.write('missing API token\\n'); sys.exit(3)".into(),
        ],
        cwd: None,
        env: vec![],
    };
    let err = connect("crash", &crashing).await.err().unwrap();
    assert!(err.contains("missing API token"), "the server's own error output is shown: {err}");
}

// A Streamable HTTP server made with the official SDK.

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct AddRequest {
    a: i64,
    b: i64,
}

#[derive(Debug, Clone)]
struct Adder {
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl Adder {
    #[tool(description = "Add two whole numbers.")]
    fn add(&self, Parameters(AddRequest { a, b }): Parameters<AddRequest>) -> String {
        (a + b).to_string()
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Adder {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
}

/// Serve the adder at `/mcp`, recording the `Authorization` header of each request.
async fn http_server() -> (String, Arc<Mutex<Vec<String>>>) {
    let service: StreamableHttpService<Adder, LocalSessionManager> = StreamableHttpService::new(
        || {
            Ok(Adder {
                tool_router: Adder::tool_router(),
            })
        },
        Default::default(),
        StreamableHttpServerConfig::default().with_sse_keep_alive(None),
    );
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let record = seen.clone();
    let app = axum::Router::new().nest_service("/mcp", service).layer(axum::middleware::from_fn(
        move |req: axum::extract::Request, next: axum::middleware::Next| {
            let record = record.clone();
            async move {
                if let Some(v) = req.headers().get("authorization") {
                    record.lock().unwrap().push(v.to_str().unwrap_or("").to_string());
                }
                next.run(req).await
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/mcp"), seen)
}

#[tokio::test]
async fn an_http_server_is_listed_and_called_with_its_headers() {
    let (url, seen) = http_server().await;
    let launch = Launch::Http {
        url,
        headers: vec![("Authorization".into(), "Bearer test-token".into())],
    };
    let (hub, _) = hub(vec![("remote", launch)]);
    let tools = hub.tools("remote", |_| true).await.unwrap();
    assert_eq!(tools.len(), 1);
    let add = find(&tools, "mcp__remote__add");
    assert_eq!(add.spec().description, "Add two whole numbers.");
    let schema: Value = add.spec().schema;
    assert_eq!(schema["properties"]["a"]["type"], "integer", "{schema}");
    let out = add.call(json!({"a": 2, "b": 40})).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.ends_with("\n42"), "{}", out.content);
    let seen = seen.lock().unwrap().clone();
    assert!(!seen.is_empty() && seen.iter().all(|h| h == "Bearer test-token"), "{seen:?}");
}

#[tokio::test]
async fn an_unreachable_http_server_is_an_error_not_a_hang() {
    // Nothing listens on this port once the listener is gone.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let launch = Launch::Http {
        url: format!("http://127.0.0.1:{port}/mcp"),
        headers: vec![],
    };
    let started = std::time::Instant::now();
    assert!(connect("down", &launch).await.is_err());
    assert!(started.elapsed() < Duration::from_secs(31));
}
