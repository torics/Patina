//! The container backend: one Docker or Podman container per run, with the worktree mounted at `/workspace`.
//! Commands run through `exec`. The container is removed when the run ends.
use super::Setup;
use crate::process::MARKER_VAR;
use async_trait::async_trait;
use patina_core::sandbox::{Prepared, Sandbox, SandboxError, SandboxLevel};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

const WORKSPACE: &str = "/workspace";
const HOME_IN_CONTAINER: &str = "/tmp/patina-home";

/// The environment of the runtime's own client program: enough to find the daemon or the user's containers.
fn client_env() -> Vec<(String, String)> {
    [
        "PATH",
        "HOME",
        "DOCKER_HOST",
        "DOCKER_CONFIG",
        "XDG_RUNTIME_DIR",
        "USERPROFILE",
        "SystemRoot",
    ]
    .iter()
    .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
    .collect()
}

async fn client(runtime: &str, args: &[String], limit: Duration) -> Result<String, String> {
    let mut cmd = Command::new(runtime);
    cmd.args(args)
        .env_clear()
        .envs(client_env())
        .stdin(Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(limit, cmd.output()).await {
        Err(_) => Err(format!("{runtime} did not answer within {} seconds", limit.as_secs())),
        Ok(Err(e)) => Err(format!("cannot run {runtime}: {e}")),
        Ok(Ok(out)) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()),
        Ok(Ok(out)) => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
    }
}

/// The first of `docker` and `podman` whose server answers, with its version.
pub async fn detect_runtime() -> Option<(String, String)> {
    for runtime in ["docker", "podman"] {
        let args = ["version".to_string(), "--format".to_string(), "{{.Server.Version}}".to_string()];
        // Podman has no server; its client version is the one that counts.
        let args = if runtime == "podman" {
            vec!["version".to_string(), "--format".to_string(), "{{.Client.Version}}".to_string()]
        } else {
            args.to_vec()
        };
        if let Ok(version) = client(runtime, &args, Duration::from_secs(5)).await
            && !version.is_empty()
        {
            return Some((runtime.to_string(), version));
        }
    }
    None
}

/// The name of a run's container. The tail of the run id is its random part.
pub fn container_name(run_id: &str) -> String {
    let hex: Vec<char> = run_id.chars().filter(|c| *c != '-').collect();
    let tail: String = hex[hex.len().saturating_sub(8)..].iter().collect();
    format!("patina-{tail}")
}

/// Remove a container if it exists, with everything running in it. Not finding it is not an error.
pub async fn remove_container(runtime: &str, name: &str) {
    let args = ["rm".to_string(), "-f".to_string(), name.to_string()];
    if let Err(e) = client(runtime, &args, Duration::from_secs(60)).await
        && !e.to_lowercase().contains("no such container")
    {
        tracing::warn!(container = %name, error = %e, "could not remove the container");
    }
}

pub struct ContainerSandbox {
    setup: Setup,
    runtime: String,
    image: String,
    started: tokio::sync::Mutex<bool>,
}

impl ContainerSandbox {
    pub fn new(runtime: Option<(String, String)>, setup: Setup) -> Result<ContainerSandbox, SandboxError> {
        let (runtime, _) = runtime.ok_or_else(|| SandboxError::Setup("no container runtime answered (docker or podman)".into()))?;
        let image = setup
            .container_image
            .clone()
            .filter(|i| !i.trim().is_empty())
            .ok_or_else(|| SandboxError::Setup("the project has no container_image".into()))?;
        if setup.container_name.is_empty() {
            return Err(SandboxError::Setup("the container has no name".into()));
        }
        Ok(ContainerSandbox {
            setup,
            runtime,
            image,
            started: tokio::sync::Mutex::new(false),
        })
    }

    /// The arguments of the `run` that starts the container. Nothing of the host is mounted but the worktree
    /// and the project's extra folders.
    pub fn run_args(&self) -> Vec<String> {
        let mut args: Vec<String> = ["run", "-d", "--rm", "--init", "--name", &self.setup.container_name]
            .iter()
            .map(|s| s.to_string())
            .collect();
        args.push("-v".into());
        args.push(format!("{}:{WORKSPACE}", self.setup.worktree.display()));
        for p in &self.setup.extra_read {
            args.push("-v".into());
            args.push(format!("{0}:{0}:ro", p.display()));
        }
        for p in &self.setup.extra_write {
            args.push("-v".into());
            args.push(format!("{0}:{0}", p.display()));
        }
        args.extend(["-w".to_string(), WORKSPACE.to_string()]);
        // No Linux capabilities, and no way to gain any through a setuid program, whoever the user inside is.
        args.extend([
            "--cap-drop".to_string(),
            "ALL".to_string(),
            "--security-opt".to_string(),
            "no-new-privileges".to_string(),
        ]);
        if !self.setup.network {
            args.extend(["--network".to_string(), "none".to_string()]);
        } else if let Some(port) = self.setup.port {
            // The preview reaches the dev server here, on the host's loopback only. A server inside must listen
            // on all of the container's addresses, not only on its loopback.
            args.extend(["-p".to_string(), format!("127.0.0.1:{port}:{port}")]);
        }
        // Files the command creates in the worktree should belong to the owner, not to root.
        #[cfg(unix)]
        if self.runtime == "podman" {
            args.push("--userns=keep-id".into());
        } else {
            args.extend([
                "--user".to_string(),
                format!("{}:{}", rustix::process::getuid().as_raw(), rustix::process::getgid().as_raw()),
            ]);
        }
        args.push(self.image.clone());
        // Stay alive until the run ends, with a shell every image has.
        args.extend(["sh".to_string(), "-c".to_string(), "while :; do sleep 3600; done".to_string()]);
        args
    }

    /// The command's environment inside the container. `PATH` is the image's own unless the project sets it.
    fn exec_env(&self, marker: &str) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = vec![("LANG".into(), "C.UTF-8".into()), ("TERM".into(), "dumb".into())];
        env.extend(self.setup.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        env.push(("HOME".into(), HOME_IN_CONTAINER.into()));
        if let Some(port) = self.setup.port {
            env.push(("PORT".into(), port.to_string()));
        }
        env.push((MARKER_VAR.into(), marker.into()));
        env
    }
}

#[async_trait]
impl Sandbox for ContainerSandbox {
    fn level(&self) -> SandboxLevel {
        SandboxLevel::Container
    }
    fn describe(&self) -> String {
        format!(
            "{} container from {}: commands see only this run's worktree{}; the network is {}",
            self.runtime,
            self.image,
            if self.setup.extra_read.is_empty() && self.setup.extra_write.is_empty() {
                ""
            } else {
                " and the project's sandbox folders"
            },
            if self.setup.network { "on" } else { "off" },
        )
    }

    async fn ensure_ready(&self) -> Result<(), SandboxError> {
        let mut started = self.started.lock().await;
        if *started {
            return Ok(());
        }
        // A container of this name can only be a leftover of this run from before Patina stopped.
        remove_container(&self.runtime, &self.setup.container_name).await;
        // Pulling an image can take minutes the first time.
        client(&self.runtime, &self.run_args(), Duration::from_secs(600))
            .await
            .map_err(|e| SandboxError::Setup(format!("the container did not start: {e}")))?;
        let mkdir = [
            "exec".to_string(),
            self.setup.container_name.clone(),
            "mkdir".to_string(),
            "-p".to_string(),
            HOME_IN_CONTAINER.to_string(),
        ];
        client(&self.runtime, &mkdir, Duration::from_secs(30))
            .await
            .map_err(|e| SandboxError::Setup(format!("the container is not usable: {e}")))?;
        *started = true;
        Ok(())
    }

    fn prepare(&self, command: &str, marker: &str) -> Result<Prepared, SandboxError> {
        let mut args: Vec<String> = vec!["exec".into(), "-w".into(), WORKSPACE.into()];
        for (k, v) in self.exec_env(marker) {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        args.extend([
            self.setup.container_name.clone(),
            "sh".to_string(),
            "-c".to_string(),
            command.to_string(),
        ]);
        Ok(Prepared {
            program: PathBuf::from(&self.runtime),
            args,
            env: client_env(),
            cwd: None,
        })
    }

    /// Signals from the host reach only the client program, so the processes are found and killed inside.
    async fn kill(&self, marker: &str) {
        if !*self.started.lock().await {
            return;
        }
        let script = format!(
            "for p in /proc/[0-9]*; do if tr '\\0' '\\n' < \"$p/environ\" 2>/dev/null | grep -qx '{MARKER_VAR}={marker}'; then kill -9 \"${{p#/proc/}}\" 2>/dev/null; fi; done; true"
        );
        let args = [
            "exec".to_string(),
            self.setup.container_name.clone(),
            "sh".to_string(),
            "-c".to_string(),
            script,
        ];
        if let Err(e) = client(&self.runtime, &args, Duration::from_secs(20)).await {
            tracing::warn!(error = %e, "could not kill inside the container");
        }
    }

    async fn shutdown(&self) {
        let mut started = self.started.lock().await;
        if *started {
            let args = ["rm".to_string(), "-f".to_string(), self.setup.container_name.clone()];
            if let Err(e) = client(&self.runtime, &args, Duration::from_secs(60)).await {
                tracing::warn!(container = %self.setup.container_name, error = %e, "could not remove the container");
            }
            *started = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(network: bool) -> ContainerSandbox {
        let setup = Setup {
            worktree: "/data/worktrees/r1".into(),
            network,
            env: [("CI".to_string(), "1".to_string())].into(),
            extra_read: vec!["/srv/toolchain".into()],
            container_image: Some("example/image:1".into()),
            container_name: "patina-r1".into(),
            ..Default::default()
        };
        ContainerSandbox::new(Some(("docker".into(), "27".into())), setup).unwrap()
    }

    #[test]
    fn the_container_mounts_only_the_worktree_and_the_projects_folders() {
        let args = sandbox(false).run_args();
        let mounts: Vec<&String> = args
            .iter()
            .zip(args.iter().skip(1))
            .filter(|(flag, _)| *flag == "-v")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(mounts, ["/data/worktrees/r1:/workspace", "/srv/toolchain:/srv/toolchain:ro"]);
        assert!(args.windows(2).any(|w| w == ["--network", "none"]));
        assert!(!args.iter().any(|a| a == "--privileged" || a.contains("docker.sock")));
        assert!(
            args.windows(2).any(|w| w == ["--cap-drop", "ALL"]) && args.windows(2).any(|w| w == ["--security-opt", "no-new-privileges"])
        );
        assert!(!sandbox(true).run_args().iter().any(|a| a == "--network"));
    }

    #[test]
    fn the_preview_port_is_published_on_loopback_and_given_as_port() {
        let with_port = |network| {
            let mut s = sandbox(network);
            s.setup.port = Some(41234);
            s
        };
        let args = with_port(true).run_args();
        assert!(args.windows(2).any(|w| w == ["-p", "127.0.0.1:41234:41234"]), "{args:?}");
        // With the network off there is nothing to publish to.
        assert!(!with_port(false).run_args().iter().any(|a| a == "-p"));
        let p = with_port(true).prepare("npm run dev", "m1").unwrap();
        assert!(p.args.windows(2).any(|w| w == ["-e", "PORT=41234"]), "{:?}", p.args);
    }

    #[test]
    fn a_command_runs_through_exec_with_only_the_listed_environment() {
        let p = sandbox(true).prepare("make test", "m1").unwrap();
        assert_eq!(p.program, PathBuf::from("docker"));
        let env: Vec<&str> = p
            .args
            .iter()
            .zip(p.args.iter().skip(1))
            .filter(|(flag, _)| *flag == "-e")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(env, ["LANG=C.UTF-8", "TERM=dumb", "CI=1", "HOME=/tmp/patina-home", "PATINA_CMD=m1"]);
        assert_eq!(p.args[p.args.len() - 4..], ["patina-r1", "sh", "-c", "make test"]);
    }

    #[test]
    fn a_container_needs_a_runtime_and_an_image() {
        let no_image = Setup {
            container_name: "c".into(),
            ..Default::default()
        };
        assert!(ContainerSandbox::new(Some(("docker".into(), "27".into())), no_image).is_err());
        let with_image = Setup {
            container_name: "c".into(),
            container_image: Some("i".into()),
            ..Default::default()
        };
        assert!(ContainerSandbox::new(None, with_image).is_err());
    }
}
