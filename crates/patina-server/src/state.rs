use crate::auth::Limiter;
use crate::error::{ApiError, ApiResult};
use patina_core::agent_config::AgentConfig;
use patina_core::config::write_atomic;
use patina_core::events::Event;
use patina_core::provider::{ChatProvider, ProviderError};
use patina_core::provider_config::{ProviderProfile, ProvidersFile};
use patina_core::{Config, Paths};
use patina_providers::{GenaiProvider, Guarded};
use patina_store::{Db, secrets::Secrets};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Builds a provider for a profile and its resolved key. Tests swap in a mock.
pub type ProviderFactory = Arc<
    dyn Fn(&ProviderProfile, Option<String>) -> Result<Arc<dyn ChatProvider>, ProviderError>
        + Send
        + Sync,
>;

pub fn genai_factory() -> ProviderFactory {
    Arc::new(
        |profile, key| Ok(Arc::new(GenaiProvider::new(profile, key)?) as Arc<dyn ChatProvider>),
    )
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
}

pub struct AppState {
    pub paths: Paths,
    pub config: Config,
    pub db: Db,
    pub secrets: Secrets,
    pub factory: ProviderFactory,
    pub limiter: Mutex<Limiter>,
    pub runs: Mutex<HashMap<String, Arc<RunHandle>>>,
    providers: Mutex<Option<(SystemTime, ProvidersFile)>>,
    registry: Mutex<HashMap<String, CachedProvider>>,
}

impl AppState {
    pub fn new(
        paths: Paths,
        config: Config,
        db: Db,
        secrets: Secrets,
        factory: ProviderFactory,
    ) -> Arc<AppState> {
        Arc::new(AppState {
            paths,
            config,
            db,
            secrets,
            factory,
            limiter: Mutex::new(Limiter::default()),
            runs: Mutex::default(),
            providers: Mutex::default(),
            registry: Mutex::default(),
        })
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
            Ok(t) => ProvidersFile::parse(&t)
                .map_err(|e| ApiError::bad_request(format!("providers.toml: {e}")))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ProvidersFile::default(),
            Err(e) => return Err(ApiError::internal(e)),
        };
        if let Some(m) = mtime {
            *cache = Some((m, file.clone()));
        }
        Ok(file)
    }

    pub fn save_providers(&self, file: &ProvidersFile) -> ApiResult<()> {
        file.validate()
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
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
            Some(r) if r.starts_with("secret:") => {
                Ok(self.secrets.get(&r["secret:".len()..]).await?)
            }
            Some(r) => Ok(std::env::var(&r["env:".len()..])
                .ok()
                .filter(|v| !v.is_empty())),
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
        let inner =
            (self.factory)(&profile, key).map_err(|e| ApiError::bad_request(e.to_string()))?;
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

    fn agents_dir(&self) -> PathBuf {
        self.paths.config_dir.join("agents")
    }

    pub fn list_agents(&self) -> ApiResult<Vec<AgentConfig>> {
        let mut out = vec![];
        let rd = match std::fs::read_dir(self.agents_dir()) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(ApiError::internal(e)),
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            match std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|t| AgentConfig::parse(&t))
            {
                Ok(a) => out.push(a),
                Err(e) => {
                    tracing::warn!(file = %path.display(), error = %e, "skipping invalid agent file")
                }
            }
        }
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
        write_atomic(&self.agents_dir().join(format!("{}.toml", a.id)), &text)
            .map_err(ApiError::internal)
    }

    pub fn delete_agent(&self, id: &str) -> ApiResult<bool> {
        // The id was validated by the caller as [A-Za-z0-9_-], so it cannot escape the folder.
        match std::fs::remove_file(self.agents_dir().join(format!("{id}.toml"))) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(ApiError::internal(e)),
        }
    }
}
