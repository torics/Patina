use crate::auth::Limiter;
use crate::error::{ApiError, ApiResult};
use patina_core::agent::{Allowlist, ApprovalDecision, Signals};
use patina_core::agent_config::AgentConfig;
use patina_core::budget::BudgetTracker;
use patina_core::config::write_atomic;
use patina_core::events::Event;
use patina_core::project_config::{ProjectConfig, ProjectsFile};
use patina_core::provider::{ChatProvider, ProviderError};
use patina_core::provider_config::{ProviderProfile, ProvidersFile};
use patina_core::team_config::TeamConfig;
use patina_core::{Config, Paths};
use patina_providers::{GenaiProvider, Guarded};
use patina_store::{Db, secrets::Secrets};
use patina_tools::sandbox::Caps;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;

/// Builds a provider for a profile and its resolved key. Tests swap in a mock.
pub type ProviderFactory = Arc<dyn Fn(&ProviderProfile, Option<String>) -> Result<Arc<dyn ChatProvider>, ProviderError> + Send + Sync>;

pub fn genai_factory() -> ProviderFactory {
    Arc::new(|profile, key| Ok(Arc::new(GenaiProvider::new(profile, key)?) as Arc<dyn ChatProvider>))
}

/// A profile and the guarded provider built from it.
type CachedProvider = (ProviderProfile, Arc<dyn ChatProvider>);

/// A message to run followers: a persisted event, or a live text delta that is not persisted.
#[derive(Debug, Clone)]
pub enum Live {
    Event(Event),
    /// `after_seq` is the last persisted sequence number when the delta was produced.
    Delta {
        agent_id: String,
        text: String,
        after_seq: i64,
    },
}

pub struct RunHandle {
    pub tx: broadcast::Sender<Live>,
    pub cancel: CancellationToken,
    pub budget: Arc<BudgetTracker>,
    /// Approval requests the run is waiting on, by approval id.
    pub pending: Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>,
    /// The project's command allowlist as this run sees it. A pattern added while approving lands here at once.
    pub allow_commands: Allowlist,
    /// Pause and shutdown requests the loop honors at its next safe point.
    pub signals: Arc<Signals>,
}

pub struct AppState {
    pub paths: Paths,
    pub config: Config,
    pub db: Db,
    pub secrets: Secrets,
    pub factory: ProviderFactory,
    pub limiter: Mutex<Limiter>,
    pub runs: Mutex<HashMap<String, Arc<RunHandle>>>,
    /// The Patina binary, which holds the Landlock helper. Tests point it at the built binary.
    pub helper: PathBuf,
    /// This process, as recorded on the runs it drives.
    pub identity: String,
    /// The preview listener's state: the selected run, each run's port, and the dev command Patina started.
    pub preview: crate::preview::Preview,
    /// Connections to the MCP servers in `mcp.toml`, shared by every run.
    pub mcp: Arc<patina_tools::mcp::McpHub>,
    caps: tokio::sync::OnceCell<Caps>,
    providers: Mutex<Option<(SystemTime, ProvidersFile)>>,
    registry: Mutex<HashMap<String, CachedProvider>>,
}

impl AppState {
    pub fn new(paths: Paths, config: Config, db: Db, secrets: Secrets, factory: ProviderFactory) -> Arc<AppState> {
        let mcp = patina_tools::mcp::McpHub::new(Arc::new(crate::mcp::ConfigSource {
            path: paths.config_dir.join("mcp.toml"),
            secrets: secrets.clone(),
        }));
        Arc::new(AppState {
            mcp,
            paths,
            config,
            db,
            secrets,
            factory,
            limiter: Mutex::new(Limiter::default()),
            runs: Mutex::default(),
            helper: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("patina")),
            identity: patina_tools::process::identity(),
            preview: Default::default(),
            caps: tokio::sync::OnceCell::new(),
            providers: Mutex::default(),
            registry: Mutex::default(),
        })
    }

    /// What sandboxing this host supports. Detected once, on first use.
    pub async fn caps(&self) -> &Caps {
        self.caps.get_or_init(Caps::detect).await
    }

    /// Fix the detected capabilities ahead of time. Returns false if they were already detected.
    pub fn set_caps(&self, caps: Caps) -> bool {
        self.caps.set(caps).is_ok()
    }

    /// The private home and temp folders of one run's commands.
    pub fn sandbox_dir(&self, run_id: &str) -> PathBuf {
        self.paths.data_dir.join("sandbox").join(run_id)
    }

    /// Every configured secret value: what the secret store holds, and the variables that providers and MCP
    /// servers take from the daemon's environment. Read fresh, so a key added a moment ago is covered.
    pub async fn redactor(&self) -> patina_core::redact::Redactor {
        let mut values = self.secrets.values().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "could not read the secret store to remove secrets from output");
            vec![]
        });
        let mut vars: Vec<String> = vec![];
        if let Ok(f) = self.providers() {
            vars.extend(
                f.providers
                    .iter()
                    .filter_map(|p| p.api_key_ref.as_deref()?.strip_prefix("env:").map(String::from)),
            );
        }
        if let Ok(f) = self.mcp_file() {
            for server in &f.servers {
                vars.extend(
                    server
                        .env
                        .values()
                        .chain(server.headers.values())
                        .filter_map(|v| v.strip_prefix("env:").map(String::from)),
                );
            }
        }
        values.extend(vars.iter().filter_map(|v| std::env::var(v).ok()));
        patina_core::redact::Redactor::new(values)
    }

    fn providers_path(&self) -> PathBuf {
        self.paths.config_dir.join("providers.toml")
    }

    /// Read `providers.toml`, reloading when the file changed on disk. A missing file is an empty list.
    pub fn providers(&self) -> ApiResult<ProvidersFile> {
        let path = self.providers_path();
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let mut cache = self.providers.lock().unwrap();
        if let (Some((cached_at, f)), Some(m)) = (cache.as_ref(), mtime)
            && *cached_at == m
        {
            return Ok(f.clone());
        }
        let file = match std::fs::read_to_string(&path) {
            Ok(t) => ProvidersFile::parse(&t).map_err(|e| ApiError::bad_request(format!("providers.toml: {e}")))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ProvidersFile::default(),
            Err(e) => return Err(ApiError::internal(e)),
        };
        if let Some(m) = mtime {
            *cache = Some((m, file.clone()));
        }
        Ok(file)
    }

    pub fn save_providers(&self, file: &ProvidersFile) -> ApiResult<()> {
        file.validate().map_err(|e| ApiError::bad_request(e.to_string()))?;
        let text = file.to_toml().map_err(ApiError::internal)?;
        write_atomic(&self.providers_path(), &text).map_err(ApiError::internal)?;
        *self.providers.lock().unwrap() = None;
        self.registry.lock().unwrap().clear();
        Ok(())
    }

    pub fn forget_provider(&self, id: &str) {
        self.registry.lock().unwrap().remove(id);
    }

    /// The key a profile points at: a stored secret, an environment variable, or none.
    pub async fn resolve_key(&self, profile: &ProviderProfile) -> ApiResult<Option<String>> {
        match profile.api_key_ref.as_deref() {
            None => Ok(None),
            Some(r) if r.starts_with("secret:") => Ok(self.secrets.get(&r["secret:".len()..]).await?),
            Some(r) => Ok(std::env::var(&r["env:".len()..]).ok().filter(|v| !v.is_empty())),
        }
    }

    /// A provider wrapped with the per-provider concurrency limit and idle timeout, cached per profile.
    pub async fn provider(&self, id: &str) -> ApiResult<(ProviderProfile, Arc<dyn ChatProvider>)> {
        let profile = self
            .providers()?
            .get(id)
            .cloned()
            .ok_or_else(|| ApiError::bad_request(format!("provider {id:?} does not exist")))?;
        if let Some((p, prov)) = self.registry.lock().unwrap().get(id)
            && *p == profile
        {
            return Ok((profile, prov.clone()));
        }
        let key = self.resolve_key(&profile).await?;
        let inner = (self.factory)(&profile, key).map_err(|e| ApiError::bad_request(e.to_string()))?;
        let guarded: Arc<dyn ChatProvider> = Arc::new(Guarded::new(
            inner,
            profile.max_concurrency,
            Duration::from_secs(profile.idle_timeout_s),
        ));
        self.registry
            .lock()
            .unwrap()
            .insert(id.to_string(), (profile.clone(), guarded.clone()));
        Ok((profile, guarded))
    }

    fn projects_path(&self) -> PathBuf {
        self.paths.config_dir.join("projects.toml")
    }

    /// Read `projects.toml` fresh each time, so hand edits apply at once. A missing file is an empty list.
    pub fn projects(&self) -> ApiResult<ProjectsFile> {
        match std::fs::read_to_string(self.projects_path()) {
            Ok(t) => ProjectsFile::parse(&t).map_err(ApiError::bad_request),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ProjectsFile::default()),
            Err(e) => Err(ApiError::internal(e)),
        }
    }

    pub fn save_projects(&self, file: &ProjectsFile) -> ApiResult<()> {
        file.validate().map_err(ApiError::bad_request)?;
        let text = file.to_toml().map_err(ApiError::internal)?;
        write_atomic(&self.projects_path(), &text).map_err(ApiError::internal)
    }

    pub fn project(&self, id: &str) -> ApiResult<ProjectConfig> {
        self.projects()?
            .get(id)
            .cloned()
            .ok_or_else(|| ApiError::bad_request(format!("project {id:?} does not exist")))
    }

    pub fn worktree_dir(&self, run_id: &str) -> PathBuf {
        self.paths.data_dir.join("worktrees").join(run_id)
    }

    pub fn artifacts_dir(&self, run_id: &str) -> PathBuf {
        self.paths.data_dir.join("artifacts").join(run_id)
    }

    fn agents_dir(&self) -> PathBuf {
        self.paths.config_dir.join("agents")
    }

    pub fn list_agents(&self) -> ApiResult<Vec<AgentConfig>> {
        let mut out = read_configs(&self.agents_dir(), "agent", AgentConfig::parse)?;
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    pub fn get_agent(&self, id: &str) -> ApiResult<Option<AgentConfig>> {
        Ok(self.list_agents()?.into_iter().find(|a| a.id == id))
    }

    pub fn save_agent(&self, a: &AgentConfig) -> ApiResult<()> {
        a.validate().map_err(ApiError::bad_request)?;
        std::fs::create_dir_all(self.agents_dir()).map_err(ApiError::internal)?;
        let text = a.to_toml().map_err(ApiError::internal)?;
        write_atomic(&self.agents_dir().join(format!("{}.toml", a.id)), &text).map_err(ApiError::internal)
    }

    pub fn delete_agent(&self, id: &str) -> ApiResult<bool> {
        // The id was validated by the caller as [A-Za-z0-9_-], so it cannot escape the folder.
        match std::fs::remove_file(self.agents_dir().join(format!("{id}.toml"))) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(ApiError::internal(e)),
        }
    }

    fn teams_dir(&self) -> PathBuf {
        self.paths.config_dir.join("teams")
    }

    pub fn list_teams(&self) -> ApiResult<Vec<TeamConfig>> {
        let mut out = read_configs(&self.teams_dir(), "team", TeamConfig::parse)?;
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    pub fn get_team(&self, id: &str) -> ApiResult<Option<TeamConfig>> {
        Ok(self.list_teams()?.into_iter().find(|t| t.id == id))
    }

    pub fn save_team(&self, t: &TeamConfig) -> ApiResult<()> {
        t.validate().map_err(ApiError::bad_request)?;
        std::fs::create_dir_all(self.teams_dir()).map_err(ApiError::internal)?;
        let text = t.to_toml().map_err(ApiError::internal)?;
        write_atomic(&self.teams_dir().join(format!("{}.toml", t.id)), &text).map_err(ApiError::internal)
    }

    pub fn delete_team(&self, id: &str) -> ApiResult<bool> {
        // The id was validated by the caller as [A-Za-z0-9_-], so it cannot escape the folder.
        match std::fs::remove_file(self.teams_dir().join(format!("{id}.toml"))) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(ApiError::internal(e)),
        }
    }
}

/// Every valid `*.toml` in a folder. A file that does not parse is skipped with a warning, so one bad hand edit does
/// not hide the rest. A missing folder is an empty list.
fn read_configs<T>(dir: &std::path::Path, what: &str, parse: fn(&str) -> Result<T, String>) -> ApiResult<Vec<T>> {
    let mut out = vec![];
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(ApiError::internal(e)),
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| parse(&t)) {
            Ok(c) => out.push(c),
            Err(e) => tracing::warn!(file = %path.display(), error = %e, "skipping invalid {what} file"),
        }
    }
    Ok(out)
}
