//! MCP servers: `mcp.toml`, the values its entries point at, and the routes that edit it.
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use async_trait::async_trait;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use patina_core::api::{McpServerInput, McpServerView, McpToolView, McpTrustInput, McpValueInput, McpValueSource, McpValueView};
use patina_core::config::write_atomic;
use patina_core::mcp_config::{McpFile, McpServer, McpTransport, ValueRef, secret_name};
use patina_store::secrets::Secrets;
use patina_tools::mcp::{Launch, McpSource};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::Duration;

type St = State<Arc<AppState>>;

/// `mcp.toml`, read fresh each time so hand edits apply at once. A missing file is an empty list.
pub fn read_file(path: &FsPath) -> Result<McpFile, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => McpFile::parse(&t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(McpFile::default()),
        Err(e) => Err(format!("cannot read mcp.toml: {e}")),
    }
}

/// The owner's MCP settings as the client sees them: launches with every value resolved, trust and timeouts.
pub struct ConfigSource {
    pub path: PathBuf,
    pub secrets: Secrets,
}

impl ConfigSource {
    fn server(&self, id: &str) -> Option<McpServer> {
        read_file(&self.path).ok()?.get(id).cloned()
    }

    async fn resolve(&self, server: &str, map: &BTreeMap<String, String>) -> Result<Vec<(String, String)>, String> {
        let mut out = vec![];
        for (k, v) in map {
            let value = match ValueRef::of(v) {
                ValueRef::Literal(t) => t.to_string(),
                ValueRef::Env(var) => std::env::var(var).map_err(|_| format!("{k} is taken from the variable {var}, which is not set"))?,
                ValueRef::Secret(name) => self
                    .secrets
                    .get(name)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("the stored value of {k} for server {server:?} is missing; set it again"))?,
            };
            out.push((k.clone(), value));
        }
        Ok(out)
    }
}

#[async_trait]
impl McpSource for ConfigSource {
    async fn launch(&self, id: &str) -> Result<Launch, String> {
        let s = self.server(id).ok_or_else(|| format!("MCP server {id:?} is not configured"))?;
        if !s.enabled {
            return Err(format!("MCP server {id:?} is disabled"));
        }
        Ok(match s.transport {
            McpTransport::Stdio => Launch::Stdio {
                command: s.command.clone().unwrap_or_default(),
                args: s.args.clone(),
                cwd: s.cwd.as_ref().map(PathBuf::from),
                env: self.resolve(id, &s.env).await?,
            },
            McpTransport::Http => Launch::Http {
                url: s.url.clone().unwrap_or_default(),
                headers: self.resolve(id, &s.headers).await?,
            },
        })
    }

    fn trusted(&self, server: &str, tool: &str) -> bool {
        self.server(server).is_some_and(|s| s.trusts(tool))
    }

    fn timeout(&self, server: &str) -> Duration {
        Duration::from_secs(self.server(server).map(|s| s.timeout_s).unwrap_or(120))
    }
}

impl AppState {
    fn mcp_path(&self) -> PathBuf {
        self.paths.config_dir.join("mcp.toml")
    }

    pub fn mcp_file(&self) -> ApiResult<McpFile> {
        read_file(&self.mcp_path()).map_err(ApiError::bad_request)
    }

    /// Write `mcp.toml`. A server that was removed or disabled is disconnected; one whose settings changed is
    /// reconnected on its next use.
    pub async fn save_mcp(&self, file: &McpFile) -> ApiResult<()> {
        file.validate().map_err(ApiError::bad_request)?;
        let text = file.to_toml().map_err(ApiError::internal)?;
        write_atomic(&self.mcp_path(), &text).map_err(ApiError::internal)?;
        for id in self.mcp.servers() {
            if !file.get(&id).is_some_and(|s| s.enabled) {
                self.mcp.close(&id).await;
            }
        }
        Ok(())
    }
}

fn value_views(map: &BTreeMap<String, String>) -> Vec<McpValueView> {
    map.iter()
        .map(|(k, v)| {
            let (source, var) = match ValueRef::of(v) {
                ValueRef::Secret(_) => (McpValueSource::Secret, None),
                ValueRef::Env(var) => (McpValueSource::Env, Some(var.to_string())),
                ValueRef::Literal(_) => (McpValueSource::Literal, None),
            };
            McpValueView {
                name: k.clone(),
                source,
                var,
            }
        })
        .collect()
}

fn view(st: &AppState, s: &McpServer) -> McpServerView {
    McpServerView {
        id: s.id.clone(),
        transport: s.transport,
        command: s.command.clone(),
        args: s.args.clone(),
        cwd: s.cwd.clone(),
        url: s.url.clone(),
        env: value_views(&s.env),
        headers: value_views(&s.headers),
        trusted_tools: s.trusted_tools.clone(),
        timeout_s: s.timeout_s as u32,
        enabled: s.enabled,
        connected: st.mcp.is_connected(&s.id),
    }
}

/// Secrets to store, by name, with their values.
type NewSecrets = Vec<(String, String)>;

/// The values of `env` or `headers` after an edit. A new value other than `env:<VAR>` is stored as a secret,
/// so nothing typed into the UI is written to `mcp.toml` in clear text. Returns the secrets to store.
fn merge_values(
    server: &str,
    what: &str,
    old: &BTreeMap<String, String>,
    input: &[McpValueInput],
) -> ApiResult<(BTreeMap<String, String>, NewSecrets)> {
    let mut out = BTreeMap::new();
    let mut store = vec![];
    for v in input {
        let name = v.name.trim().to_string();
        let value = v.value.as_deref().unwrap_or("");
        let entry = if value.is_empty() {
            old.get(&name)
                .cloned()
                .ok_or_else(|| ApiError::bad_request(format!("{what} {name:?} needs a value")))?
        } else if value.starts_with("env:") {
            value.to_string()
        } else {
            let secret = secret_name(server, &name);
            store.push((secret.clone(), value.to_string()));
            format!("secret:{secret}")
        };
        if out.insert(name.clone(), entry).is_some() {
            return Err(ApiError::bad_request(format!("{what} {name:?} is given twice")));
        }
    }
    Ok((out, store))
}

/// Build a server from the input, keeping unchanged values of `old`. Nothing is stored until the caller saves.
fn from_input(input: &McpServerInput, old: Option<&McpServer>) -> ApiResult<(McpServer, NewSecrets)> {
    let empty = BTreeMap::new();
    let id = input.id.trim().to_string();
    let (env, mut store) = merge_values(&id, "environment variable", old.map(|o| &o.env).unwrap_or(&empty), &input.env)?;
    let (headers, more) = merge_values(&id, "header", old.map(|o| &o.headers).unwrap_or(&empty), &input.headers)?;
    store.extend(more);
    let text = |o: &Option<String>| o.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let server = McpServer {
        id,
        transport: input.transport,
        command: text(&input.command),
        args: input.args.clone(),
        cwd: text(&input.cwd),
        url: text(&input.url),
        env,
        headers,
        trusted_tools: input.trusted_tools.clone(),
        timeout_s: input.timeout_s.map(u64::from).or(old.map(|o| o.timeout_s)).unwrap_or(120),
        enabled: input.enabled.or(old.map(|o| o.enabled)).unwrap_or(true),
    };
    server.validate().map_err(ApiError::bad_request)?;
    Ok((server, store))
}

/// Remove the secrets of `old` that `new` no longer points at.
async fn drop_unused_secrets(st: &AppState, old: &McpServer, new: Option<&McpServer>) -> ApiResult<()> {
    let used = |name: &str| {
        new.is_some_and(|n| {
            n.env
                .values()
                .chain(n.headers.values())
                .any(|v| ValueRef::of(v) == ValueRef::Secret(name))
        })
    };
    for v in old.env.values().chain(old.headers.values()) {
        if let ValueRef::Secret(name) = ValueRef::of(v)
            && name.starts_with(&format!("mcp/{}/", old.id))
            && !used(name)
        {
            st.secrets.delete(name).await?;
        }
    }
    Ok(())
}

async fn store_secrets(st: &AppState, store: &[(String, String)]) -> ApiResult<()> {
    for (name, value) in store {
        st.secrets.put(name, value).await?;
    }
    Ok(())
}

pub async fn list(State(st): St) -> ApiResult<Json<Vec<McpServerView>>> {
    let file = st.mcp_file()?;
    Ok(Json(file.servers.iter().map(|s| view(&st, s)).collect()))
}

pub async fn create(State(st): St, Json(input): Json<McpServerInput>) -> ApiResult<Json<McpServerView>> {
    let mut file = st.mcp_file()?;
    if file.get(input.id.trim()).is_some() {
        return Err(ApiError::conflict("exists", format!("MCP server {:?} already exists", input.id)));
    }
    let (server, store) = from_input(&input, None)?;
    file.servers.push(server.clone());
    file.validate().map_err(ApiError::bad_request)?;
    store_secrets(&st, &store).await?;
    st.save_mcp(&file).await?;
    Ok(Json(view(&st, &server)))
}

pub async fn update(State(st): St, Path(id): Path<String>, Json(mut input): Json<McpServerInput>) -> ApiResult<Json<McpServerView>> {
    let mut file = st.mcp_file()?;
    let slot = file
        .servers
        .iter_mut()
        .find(|s| s.id == id)
        .ok_or_else(|| ApiError::not_found("MCP server"))?;
    // The id names its secrets and the tools agents were given, so it does not change.
    input.id = id.clone();
    let old = slot.clone();
    let (server, store) = from_input(&input, Some(&old))?;
    *slot = server.clone();
    file.validate().map_err(ApiError::bad_request)?;
    store_secrets(&st, &store).await?;
    st.save_mcp(&file).await?;
    drop_unused_secrets(&st, &old, Some(&server)).await?;
    Ok(Json(view(&st, &server)))
}

pub async fn delete(State(st): St, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let mut file = st.mcp_file()?;
    let Some(old) = file.get(&id).cloned() else {
        return Err(ApiError::not_found("MCP server"));
    };
    file.servers.retain(|s| s.id != id);
    st.save_mcp(&file).await?;
    st.mcp.close(&id).await;
    drop_unused_secrets(&st, &old, None).await?;
    Ok(Json(json!({"deleted": id})))
}

/// The tools a server offers now. Starts or reaches the server.
pub async fn tools(State(st): St, Path(id): Path<String>) -> ApiResult<Json<Vec<McpToolView>>> {
    let server = st.mcp_file()?.get(&id).cloned().ok_or_else(|| ApiError::not_found("MCP server"))?;
    let conn = st
        .mcp
        .connection(&id)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, "mcp_unavailable", e))?;
    Ok(Json(
        conn.tools()
            .iter()
            .map(|t| McpToolView {
                name: t.name.clone(),
                exposed: t.exposed.clone(),
                description: t.description.clone(),
                trusted: server.trusts(&t.name),
            })
            .collect(),
    ))
}

/// Trust or stop trusting one tool of a server, by the server's name for it, or `*` for all of them.
pub async fn set_trust(st: &AppState, server: &str, tool: &str, trusted: bool) -> ApiResult<McpServer> {
    if tool.trim().is_empty() || tool.len() > 256 {
        return Err(ApiError::bad_request("name the tool to trust"));
    }
    let mut file = st.mcp_file()?;
    let s = file
        .servers
        .iter_mut()
        .find(|s| s.id == server)
        .ok_or_else(|| ApiError::not_found("MCP server"))?;
    s.trusted_tools.retain(|t| t != tool);
    if trusted {
        s.trusted_tools.push(tool.to_string());
    }
    let s = s.clone();
    st.save_mcp(&file).await?;
    Ok(s)
}

pub async fn trust(State(st): St, Path(id): Path<String>, Json(input): Json<McpTrustInput>) -> ApiResult<Json<McpServerView>> {
    let s = set_trust(&st, &id, &input.tool, input.trusted).await?;
    Ok(Json(view(&st, &s)))
}
