//! `mcp.toml`: the MCP servers the owner configured, and how their tools are named for a model.
use crate::provider_config::valid_id;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use ts_rs::TS;

/// Every MCP tool a model sees starts with this, then the server id, `__`, and the tool's name.
pub const MCP_PREFIX: &str = "mcp__";

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    /// A child process that speaks MCP on its standard input and output.
    #[default]
    Stdio,
    /// A remote server that speaks MCP over Streamable HTTP.
    Http,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServer {
    pub id: String,
    #[serde(default)]
    pub transport: McpTransport,
    /// For `stdio`: the program to start, found on the daemon's `PATH` when it has no folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// For `stdio`: the folder it starts in. The daemon's own when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// For `stdio`: the only variables the server gets beyond a minimal base. Each value is literal text,
    /// `env:<VAR>` (taken from the daemon's environment) or `secret:<name>` (a stored secret).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// For `http`: the endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// For `http`: headers sent with every request. Values take the same forms as `env`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Tools, by the server's own name for them, that run without asking. `*` trusts every tool of the server.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trusted_tools: Vec<String>,
    /// The longest one call may take, in seconds.
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
    /// A disabled server is never started, and agents that name it get none of its tools.
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn default_timeout() -> u64 {
    120
}
fn yes() -> bool {
    true
}

/// The longest timeout a server may set.
pub const MAX_TIMEOUT_S: u64 = 600;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpFile {
    pub schema_version: u32,
    #[serde(default, rename = "server")]
    pub servers: Vec<McpServer>,
}

impl Default for McpFile {
    fn default() -> Self {
        McpFile {
            schema_version: 1,
            servers: vec![],
        }
    }
}

impl McpFile {
    pub fn parse(text: &str) -> Result<McpFile, String> {
        let f: McpFile = toml::from_str(text).map_err(|e| format!("invalid mcp.toml: {e}"))?;
        if f.schema_version > 1 {
            return Err(format!(
                "mcp.toml schema_version {} is newer than this build supports",
                f.schema_version
            ));
        }
        f.validate()?;
        Ok(f)
    }

    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| e.to_string())
    }

    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for s in &self.servers {
            s.validate()?;
            if !seen.insert(s.id.as_str()) {
                return Err(format!("duplicate MCP server id {:?}", s.id));
            }
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&McpServer> {
        self.servers.iter().find(|s| s.id == id)
    }
}

/// A value in `env` or `headers`: literal text, `env:<VAR>` or `secret:<name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueRef<'a> {
    Literal(&'a str),
    Env(&'a str),
    Secret(&'a str),
}

impl<'a> ValueRef<'a> {
    pub fn of(v: &'a str) -> ValueRef<'a> {
        if let Some(var) = v.strip_prefix("env:") {
            ValueRef::Env(var)
        } else if let Some(name) = v.strip_prefix("secret:") {
            ValueRef::Secret(name)
        } else {
            ValueRef::Literal(v)
        }
    }
}

/// The stored secret that holds the value of `env` or header `key` of `server`. The slash keeps it apart from
/// provider secrets, whose names are ids.
pub fn secret_name(server: &str, key: &str) -> String {
    format!("mcp/{server}/{key}")
}

fn valid_env_name(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 128
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !k.starts_with(|c: char| c.is_ascii_digit())
}

/// Headers the MCP transport or HTTP itself sets; a server's settings may not replace them.
const RESERVED_HEADERS: [&str; 7] = [
    "accept",
    "content-type",
    "content-length",
    "host",
    "mcp-session-id",
    "mcp-protocol-version",
    "last-event-id",
];

fn valid_header_name(k: &str) -> bool {
    !RESERVED_HEADERS.iter().any(|r| r.eq_ignore_ascii_case(k))
        && !k.is_empty()
        && k.len() <= 128
        && k.chars().all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c))
}

fn check_values(what: &str, map: &BTreeMap<String, String>, name_ok: fn(&str) -> bool) -> Result<(), String> {
    for (k, v) in map {
        if !name_ok(k) {
            return Err(format!("invalid {what} name {k:?}"));
        }
        let bad_text = |t: &str| t.contains(['\0', '\n', '\r']);
        match ValueRef::of(v) {
            ValueRef::Env(var) if !valid_env_name(var) => return Err(format!("{what} {k}: env:{var} is not a variable name")),
            ValueRef::Secret("") => return Err(format!("{what} {k}: secret: needs a name")),
            ValueRef::Literal(t) | ValueRef::Secret(t) if bad_text(t) => {
                return Err(format!("{what} {k} must be one line without NUL characters"));
            }
            _ => {}
        }
    }
    Ok(())
}

impl McpServer {
    pub fn validate(&self) -> Result<(), String> {
        if !valid_id(&self.id) || self.id.contains("__") || self.id.len() > 32 {
            return Err(format!(
                "invalid MCP server id {:?}: use up to 32 letters, digits, - and _, without a double underscore",
                self.id
            ));
        }
        if self.timeout_s == 0 || self.timeout_s > MAX_TIMEOUT_S {
            return Err(format!("timeout_s must be between 1 and {MAX_TIMEOUT_S}"));
        }
        match self.transport {
            McpTransport::Stdio => {
                if self.command.as_deref().is_none_or(|c| c.trim().is_empty()) {
                    return Err(format!("MCP server {:?} uses stdio and needs a command", self.id));
                }
                if self.url.is_some() || !self.headers.is_empty() {
                    return Err(format!("MCP server {:?} uses stdio, so url and headers do not apply", self.id));
                }
            }
            McpTransport::Http => {
                let url = self.url.as_deref().unwrap_or("");
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err(format!(
                        "MCP server {:?} uses http and needs a url starting with http:// or https://",
                        self.id
                    ));
                }
                if self.command.is_some() || !self.args.is_empty() || self.cwd.is_some() || !self.env.is_empty() {
                    return Err(format!(
                        "MCP server {:?} uses http, so command, args, cwd and env do not apply",
                        self.id
                    ));
                }
            }
        }
        check_values("environment variable", &self.env, valid_env_name)?;
        check_values("header", &self.headers, valid_header_name)?;
        Ok(())
    }

    /// Whether a tool, by the server's name for it, runs without asking.
    pub fn trusts(&self, tool: &str) -> bool {
        self.trusted_tools.iter().any(|t| t == "*" || t == tool)
    }
}

/// The name a model sees for `tool` of `server`. Model APIs accept letters, digits, `_` and `-`, up to 64
/// characters, so anything else becomes `_` and a long name is cut.
pub fn exposed_name(server: &str, tool: &str) -> String {
    let clean: String = tool
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut name = format!("{MCP_PREFIX}{server}__{clean}");
    name.truncate(64);
    name
}

/// What an entry in an agent's `tools` list asks for, when it names MCP tools: every tool of a server
/// (`mcp__<server>`) or one tool (`mcp__<server>__<tool>`, the exposed name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpGrant<'a> {
    Server(&'a str),
    Tool { server: &'a str, exposed: &'a str },
}

pub fn grant_of(entry: &str) -> Option<McpGrant<'_>> {
    let rest = entry.strip_prefix(MCP_PREFIX)?;
    match rest.split_once("__") {
        None if !rest.is_empty() => Some(McpGrant::Server(rest)),
        Some((server, tool)) if !server.is_empty() && !tool.is_empty() => Some(McpGrant::Tool { server, exposed: entry }),
        _ => None,
    }
}

/// Whether an agent with this `tools` list may use the tool a model knows as `exposed`, of `server`.
pub fn granted(tools: &[String], server: &str, exposed: &str) -> bool {
    tools.iter().filter_map(|t| grant_of(t)).any(|g| match g {
        McpGrant::Server(s) => s == server,
        McpGrant::Tool { exposed: e, .. } => e == exposed,
    })
}

/// The servers an agent with this `tools` list draws on, in the order first named.
pub fn servers_named(tools: &[String]) -> Vec<&str> {
    let mut out: Vec<&str> = vec![];
    for g in tools.iter().filter_map(|t| grant_of(t)) {
        let s = match g {
            McpGrant::Server(s) | McpGrant::Tool { server: s, .. } => s,
        };
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
schema_version = 1

[[server]]
id = "fetch"
command = "uvx"
args = ["mcp-server-fetch"]
env = { HTTPS_PROXY = "env:HTTPS_PROXY", TOKEN = "secret:mcp/fetch/TOKEN", MODE = "plain" }
trusted_tools = ["fetch"]

[[server]]
id = "docs"
transport = "http"
url = "https://example.com/mcp"
headers = { Authorization = "secret:mcp/docs/Authorization" }
timeout_s = 30
enabled = false
"#;

    #[test]
    fn parses_both_transports_with_defaults_and_round_trips() {
        let f = McpFile::parse(SAMPLE).unwrap();
        let fetch = f.get("fetch").unwrap();
        assert_eq!((fetch.transport, fetch.timeout_s, fetch.enabled), (McpTransport::Stdio, 120, true));
        assert!(fetch.trusts("fetch") && !fetch.trusts("other"));
        let docs = f.get("docs").unwrap();
        assert_eq!((docs.transport, docs.timeout_s, docs.enabled), (McpTransport::Http, 30, false));
        assert_eq!(McpFile::parse(&f.to_toml().unwrap()).unwrap(), f);
        assert_eq!(McpFile::parse("schema_version = 1\n").unwrap(), McpFile::default());
    }

    #[test]
    fn rejects_bad_servers() {
        for bad in [
            "schema_version=2\n",
            "schema_version=1\n[[server]]\nid='a__b'\ncommand='x'\n",
            "schema_version=1\n[[server]]\nid='a b'\ncommand='x'\n",
            "schema_version=1\n[[server]]\nid='a'\n",
            "schema_version=1\n[[server]]\nid='a'\ncommand='x'\nurl='http://h'\n",
            "schema_version=1\n[[server]]\nid='a'\ntransport='http'\n",
            "schema_version=1\n[[server]]\nid='a'\ntransport='http'\nurl='ftp://h'\n",
            "schema_version=1\n[[server]]\nid='a'\ntransport='http'\nurl='http://h'\ncommand='x'\n",
            "schema_version=1\n[[server]]\nid='a'\ncommand='x'\ntimeout_s=0\n",
            "schema_version=1\n[[server]]\nid='a'\ncommand='x'\ntimeout_s=601\n",
            "schema_version=1\n[[server]]\nid='a'\ncommand='x'\nenv={ 'A=B' = 'x' }\n",
            "schema_version=1\n[[server]]\nid='a'\ncommand='x'\nenv={ A = 'env:no-such' }\n",
            "schema_version=1\n[[server]]\nid='a'\ncommand='x'\nenv={ A = \"two\\nlines\" }\n",
            "schema_version=1\n[[server]]\nid='a'\ntransport='http'\nurl='http://h'\nheaders={ 'Bad Name' = 'x' }\n",
            "schema_version=1\n[[server]]\nid='a'\ntransport='http'\nurl='http://h'\nheaders={ 'Mcp-Session-Id' = 'x' }\n",
            "schema_version=1\n[[server]]\nid='a'\ncommand='x'\n[[server]]\nid='a'\ncommand='y'\n",
        ] {
            assert!(McpFile::parse(bad).is_err(), "accepted: {bad}");
        }
    }

    #[test]
    fn value_refs() {
        assert_eq!(ValueRef::of("env:HOME"), ValueRef::Env("HOME"));
        assert_eq!(ValueRef::of("secret:mcp/a/B"), ValueRef::Secret("mcp/a/B"));
        assert_eq!(ValueRef::of("text"), ValueRef::Literal("text"));
        assert_eq!(secret_name("a", "B"), "mcp/a/B");
    }

    #[test]
    fn exposed_names_are_safe_for_model_apis() {
        assert_eq!(exposed_name("fetch", "fetch"), "mcp__fetch__fetch");
        assert_eq!(exposed_name("gh", "issues.list/all"), "mcp__gh__issues_list_all");
        let long = exposed_name("server", &"x".repeat(100));
        assert_eq!(long.len(), 64);
        assert!(long.starts_with("mcp__server__x"));
    }

    #[test]
    fn agents_opt_in_per_server_or_per_tool() {
        let tools: Vec<String> = ["read_file", "mcp__fetch", "mcp__gh__get_issue", "mcp__", "mcp__x__"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(grant_of("read_file"), None);
        assert_eq!(grant_of("mcp__fetch"), Some(McpGrant::Server("fetch")));
        assert_eq!(
            grant_of("mcp__gh__get_issue"),
            Some(McpGrant::Tool {
                server: "gh",
                exposed: "mcp__gh__get_issue"
            })
        );
        assert!(granted(&tools, "fetch", "mcp__fetch__anything"));
        assert!(granted(&tools, "gh", "mcp__gh__get_issue"));
        assert!(!granted(&tools, "gh", "mcp__gh__create_issue"));
        assert!(!granted(&tools, "x", "mcp__x__y"));
        assert_eq!(servers_named(&tools), vec!["fetch", "gh"]);
    }
}
