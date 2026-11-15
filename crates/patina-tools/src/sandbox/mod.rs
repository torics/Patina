//! Sandbox backends behind `patina_core::sandbox::Sandbox`, and detection of what the host supports.
//!
//! Selection fails closed: a backend that was asked for and cannot be set up is an error, never a silent
//! downgrade to something weaker.
use patina_core::sandbox::{Prepared, ResourceLimits, Sandbox, SandboxChoice, SandboxError, SandboxLevel, SandboxStatus};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod container;
#[cfg(target_os = "linux")]
pub mod landlock;

use crate::cgroup::CgroupState;
use crate::process::MARKER_VAR;

/// What one run's sandbox is built from.
#[derive(Debug, Clone, Default)]
pub struct Setup {
    /// The run's worktree: the workspace root, and the only project folder commands can write to.
    pub worktree: PathBuf,
    /// A home folder that belongs to this run alone.
    pub home: PathBuf,
    /// A temp folder that belongs to this run alone.
    pub tmp: PathBuf,
    /// The Patina binary, which contains the Landlock helper.
    pub helper: PathBuf,
    /// Whether commands may use the network.
    pub network: bool,
    /// The project's extra environment variables.
    pub env: BTreeMap<String, String>,
    pub extra_read: Vec<PathBuf>,
    pub extra_write: Vec<PathBuf>,
    pub container_image: Option<String>,
    /// A name for this run's container.
    pub container_name: String,
    /// Patina's own config and data folders. No command gets access to them.
    pub protected: Vec<PathBuf>,
    /// The run's preview port. Commands get it as `PORT`, and may listen on it even when TCP is blocked.
    pub port: Option<u16>,
    /// The project's caps on memory, processes, CPU and file size.
    pub limits: ResourceLimits,
}

/// What the host was seen to support.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Caps {
    /// The highest Landlock ABI that was probed successfully, up to 6.
    pub landlock_abi: Option<u32>,
    /// The container runtime that answered, as (program, version).
    pub runtime: Option<(String, String)>,
    /// Whether commands get cgroup v2 limits.
    pub cgroup: CgroupState,
}

impl Caps {
    /// Look at the host. Takes up to a few seconds when a container runtime is installed but slow to answer.
    pub async fn detect() -> Caps {
        Caps {
            landlock_abi: landlock_abi(),
            runtime: container::detect_runtime().await,
            cgroup: crate::cgroup::detect(),
        }
    }

    /// The backend `auto` gives a project with these settings.
    pub fn auto_level(&self, has_image: bool) -> SandboxLevel {
        if self.landlock_abi.is_some() {
            SandboxLevel::Landlock
        } else if self.runtime.is_some() && has_image {
            SandboxLevel::Container
        } else {
            SandboxLevel::None
        }
    }

    /// The backend a project gets, or why the one it asked for is not available.
    pub fn resolve(&self, choice: SandboxChoice, has_image: bool) -> Result<SandboxLevel, SandboxError> {
        match choice {
            SandboxChoice::Auto => Ok(self.auto_level(has_image)),
            SandboxChoice::None => Ok(SandboxLevel::None),
            SandboxChoice::Landlock if self.landlock_abi.is_some() => Ok(SandboxLevel::Landlock),
            SandboxChoice::Landlock => Err(SandboxError::Setup(
                "this host does not support Landlock (it needs Linux 5.13 or later with Landlock enabled)".into(),
            )),
            SandboxChoice::Container if self.runtime.is_none() => {
                Err(SandboxError::Setup("no container runtime answered (docker or podman)".into()))
            }
            SandboxChoice::Container if !has_image => Err(SandboxError::Setup("the project has no container_image".into())),
            SandboxChoice::Container => Ok(SandboxLevel::Container),
        }
    }

    /// The host-wide picture for the UI badge and `patina doctor`.
    pub fn status(&self) -> SandboxStatus {
        let level = self.auto_level(false);
        let abi = self.landlock_abi;
        let tcp_block = abi.is_some_and(|a| a >= 4);
        let runtime = self.runtime.as_ref().map(|(name, version)| format!("{name} {version}"));
        let summary = match (level, abi) {
            (SandboxLevel::Landlock, Some(a)) => format!(
                "Landlock (ABI {a}{}): commands can write only to the run's worktree and its own home and temp folders, and cannot read your home folder or Patina's data. {}{}",
                if a >= 6 { " or later" } else { "" },
                if tcp_block { "TCP can be blocked per project; UDP cannot. " } else { "Network access cannot be restricted. " },
                if a >= 6 { "Commands cannot signal processes outside the sandbox." } else { "Commands can still signal your other processes (that needs ABI 6)." },
            ),
            _ if runtime.is_some() => "No Landlock on this host. A project with a container_image runs its commands in a container; any other project asks before every edit and command.".to_string(),
            _ => "No sandbox is available on this host, so every edit and command asks for approval.".to_string(),
        };
        SandboxStatus {
            level,
            landlock_abi: abi,
            tcp_block,
            container_runtime: runtime,
            resource_limits: self.resource_limits(),
            summary,
        }
    }

    /// The mechanisms that enforce a project's limits on this host. Nothing is listed unless it is enforced.
    pub fn resource_limits(&self) -> Vec<String> {
        let mut out = vec![];
        if let CgroupState::Active(controllers) = &self.cgroup {
            out.push(format!("{} per command (cgroup v2)", crate::cgroup::describe(controllers)));
        }
        if cfg!(unix) {
            out.push("largest file a command may write (RLIMIT_FSIZE)".to_string());
        }
        if self.runtime.is_some() {
            out.push("container backend: memory, processes, CPU and file size for the run's container".to_string());
        }
        out
    }
}

#[cfg(target_os = "linux")]
fn landlock_abi() -> Option<u32> {
    landlock::probe_abi()
}

#[cfg(not(target_os = "linux"))]
fn landlock_abi() -> Option<u32> {
    None
}

/// Whether `a` contains `b`, is `b`, or sits inside `b`.
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// The environment every command gets: an allowlist, never the daemon's own environment.
/// `PATH`, `LANG` and `TERM` come from the daemon unless the project sets them; nothing else is inherited.
pub fn base_env(setup: &Setup, home: &Path, marker: &str) -> Vec<(String, String)> {
    let inherited = |name: &str, fallback: &str| {
        std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| fallback.to_string())
    };
    let mut env = BTreeMap::new();
    env.insert("PATH".to_string(), inherited("PATH", "/usr/local/bin:/usr/bin:/bin"));
    env.insert("LANG".to_string(), inherited("LANG", "C.UTF-8"));
    env.insert("TERM".to_string(), "dumb".to_string());
    for (k, v) in &setup.env {
        env.insert(k.clone(), v.clone());
    }
    env.insert("HOME".to_string(), home.to_string_lossy().into_owned());
    env.insert("TMPDIR".to_string(), setup.tmp.to_string_lossy().into_owned());
    if let Some(port) = setup.port {
        env.insert("PORT".to_string(), port.to_string());
    }
    env.insert(MARKER_VAR.to_string(), marker.to_string());
    env.into_iter().collect()
}

/// No containment: the command runs as the daemon's user with a scrubbed environment and the real home folder.
/// The policy engine confirms every edit and command when this backend is active.
pub struct NoSandbox {
    setup: Setup,
}

impl Sandbox for NoSandbox {
    fn level(&self) -> SandboxLevel {
        SandboxLevel::None
    }
    fn describe(&self) -> String {
        "no sandbox: commands run as you, so each one asks first".into()
    }
    fn prepare(&self, command: &str, marker: &str) -> Result<Prepared, SandboxError> {
        // Without containment a private home would only break toolchains, so commands see the real one.
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| self.setup.home.clone());
        let env = base_env(&self.setup, &home, marker);
        let (program, args) = shell(command);
        Ok(Prepared {
            program,
            args,
            env,
            cwd: Some(self.setup.worktree.clone()),
            limits: Some(self.setup.limits),
        })
    }
}

#[cfg(unix)]
pub(crate) fn shell(command: &str) -> (PathBuf, Vec<String>) {
    (PathBuf::from("/bin/sh"), vec!["-c".into(), command.into()])
}

#[cfg(not(unix))]
pub(crate) fn shell(command: &str) -> (PathBuf, Vec<String>) {
    (PathBuf::from("cmd"), vec!["/C".into(), command.into()])
}

/// Build the sandbox for one run. The run's home and temp folders are created here.
pub fn build(level: SandboxLevel, caps: &Caps, setup: Setup) -> Result<Arc<dyn Sandbox>, SandboxError> {
    for extra in setup.extra_read.iter().chain(&setup.extra_write) {
        if let Some(own) = setup.protected.iter().find(|own| overlaps(extra, own)) {
            return Err(SandboxError::Setup(format!(
                "{} overlaps Patina's own folder {}",
                extra.display(),
                own.display()
            )));
        }
    }
    for dir in [&setup.home, &setup.tmp] {
        std::fs::create_dir_all(dir).map_err(|e| SandboxError::Setup(format!("cannot create {}: {e}", dir.display())))?;
    }
    match level {
        SandboxLevel::None => Ok(Arc::new(NoSandbox { setup })),
        #[cfg(target_os = "linux")]
        SandboxLevel::Landlock => Ok(Arc::new(landlock::LandlockSandbox::new(caps.landlock_abi, setup)?)),
        #[cfg(not(target_os = "linux"))]
        SandboxLevel::Landlock => Err(SandboxError::Setup("Landlock exists only on Linux".into())),
        SandboxLevel::Container => Ok(Arc::new(container::ContainerSandbox::new(caps.runtime.clone(), setup)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(abi: Option<u32>, runtime: bool) -> Caps {
        Caps {
            landlock_abi: abi,
            runtime: runtime.then(|| ("docker".to_string(), "27.0".to_string())),
            cgroup: CgroupState::Unavailable("not delegated".into()),
        }
    }

    #[test]
    fn auto_picks_the_strongest_backend_the_host_and_project_support() {
        assert_eq!(caps(Some(3), true).auto_level(true), SandboxLevel::Landlock);
        assert_eq!(caps(None, true).auto_level(true), SandboxLevel::Container);
        assert_eq!(caps(None, true).auto_level(false), SandboxLevel::None, "a container needs an image");
        assert_eq!(caps(None, false).auto_level(true), SandboxLevel::None);
    }

    #[test]
    fn a_backend_that_was_asked_for_and_is_missing_is_an_error_not_a_downgrade() {
        let none = caps(None, false);
        assert!(none.resolve(SandboxChoice::Landlock, false).is_err());
        assert!(none.resolve(SandboxChoice::Container, true).is_err());
        assert!(caps(None, true).resolve(SandboxChoice::Container, false).is_err());
        assert_eq!(
            caps(None, true).resolve(SandboxChoice::Container, true),
            Ok(SandboxLevel::Container)
        );
        assert_eq!(
            caps(Some(1), false).resolve(SandboxChoice::Landlock, false),
            Ok(SandboxLevel::Landlock)
        );
        assert_eq!(caps(Some(6), true).resolve(SandboxChoice::None, true), Ok(SandboxLevel::None));
        assert_eq!(none.resolve(SandboxChoice::Auto, false), Ok(SandboxLevel::None));
    }

    #[test]
    fn the_status_claims_only_what_was_detected() {
        let old = caps(Some(3), false).status();
        assert_eq!((old.level, old.tcp_block), (SandboxLevel::Landlock, false));
        assert!(
            old.summary.contains("Network access cannot be restricted") && old.summary.contains("can still signal"),
            "{}",
            old.summary
        );
        let new = caps(Some(6), false).status();
        assert!(
            new.tcp_block && new.summary.contains("UDP cannot") && new.summary.contains("cannot signal"),
            "{}",
            new.summary
        );
        // Only what is enforced is listed: without a delegated cgroup, memory is capped only in a container.
        assert_eq!(
            caps(Some(6), false).resource_limits(),
            ["largest file a command may write (RLIMIT_FSIZE)"]
        );
        let mut all = caps(Some(6), true);
        all.cgroup = CgroupState::Active(vec!["memory".into(), "pids".into()]);
        let listed = all.status().resource_limits;
        assert_eq!(listed[0], "memory and processes per command (cgroup v2)");
        assert!(listed[2].starts_with("container backend:"), "{listed:?}");
        // A cgroup that could be used but was not set up enforces nothing.
        all.cgroup = CgroupState::Possible(vec!["memory".into()]);
        assert!(!all.resource_limits().iter().any(|l| l.contains("cgroup")));
        let none = caps(None, false).status();
        assert_eq!(none.level, SandboxLevel::None);
        assert!(none.summary.contains("every edit and command asks"));
    }

    #[test]
    fn the_environment_is_an_allowlist() {
        let setup = Setup {
            tmp: "/run/tmp".into(),
            env: [("CI".to_string(), "1".to_string()), ("TERM".to_string(), "xterm".to_string())].into(),
            port: Some(41234),
            ..Default::default()
        };
        let env: BTreeMap<_, _> = base_env(&setup, Path::new("/run/home"), "m1").into_iter().collect();
        let names: Vec<&str> = env.keys().map(String::as_str).collect();
        assert_eq!(names, ["CI", "HOME", "LANG", "PATH", "PATINA_CMD", "PORT", "TERM", "TMPDIR"]);
        assert_eq!(env["PORT"], "41234");
        assert_eq!(
            (
                env["HOME"].as_str(),
                env["TMPDIR"].as_str(),
                env["TERM"].as_str(),
                env["PATINA_CMD"].as_str()
            ),
            ("/run/home", "/run/tmp", "xterm", "m1")
        );
    }

    #[test]
    fn extra_paths_may_not_touch_patinas_own_folders() {
        let t = tempfile::tempdir().unwrap();
        let base = Setup {
            home: t.path().join("h"),
            tmp: t.path().join("t"),
            protected: vec!["/data/patina".into()],
            ..Default::default()
        };
        for bad in ["/data/patina", "/data/patina/patina.db", "/data", "/"] {
            let setup = Setup {
                extra_read: vec![bad.into()],
                ..base.clone()
            };
            assert!(build(SandboxLevel::None, &Caps::default(), setup).is_err(), "{bad}");
        }
        let ok = Setup {
            extra_write: vec!["/data/cache".into()],
            ..base.clone()
        };
        assert!(build(SandboxLevel::None, &Caps::default(), ok).is_ok());
    }
}
