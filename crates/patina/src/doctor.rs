//! `patina doctor`: check what Patina depends on and say plainly what is and is not in place.
//! It changes nothing: it creates no folder, writes no file and starts no container.
use patina_core::agent_config::AgentConfig;
use patina_core::project_config::ProjectsFile;
use patina_core::provider_config::ProvidersFile;
use patina_core::sandbox::SandboxLevel;
use patina_core::{Config, Paths};
use patina_tools::cgroup::{CgroupState, describe};
use patina_tools::git;
use patina_tools::sandbox::Caps;
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    /// Works, with something the owner should know.
    Note,
    /// Something Patina needs is missing or broken.
    Problem,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub version: String,
    pub checks: Vec<Check>,
    pub sandbox: patina_core::sandbox::SandboxStatus,
}

impl Report {
    pub fn healthy(&self) -> bool {
        self.checks.iter().all(|c| c.status != Status::Problem)
    }

    pub fn print(&self, out: &mut dyn std::io::Write) -> std::io::Result<()> {
        writeln!(out, "patina {}", self.version)?;
        for c in &self.checks {
            let mark = match c.status {
                Status::Ok => "ok     ",
                Status::Note => "note   ",
                Status::Problem => "PROBLEM",
            };
            let mut lines = c.detail.lines();
            writeln!(out, "{mark}  {}: {}", c.name, lines.next().unwrap_or(""))?;
            for line in lines {
                writeln!(out, "           {line}")?;
            }
        }
        let problems = self.checks.iter().filter(|c| c.status == Status::Problem).count();
        match problems {
            0 => writeln!(out, "No problems found."),
            n => writeln!(out, "{n} problem(s) found."),
        }
    }
}

struct Checks(Vec<Check>);

impl Checks {
    fn add(&mut self, name: &str, status: Status, detail: impl Into<String>) {
        self.0.push(Check {
            name: name.into(),
            status,
            detail: detail.into(),
        });
    }
}

/// What the sandbox line says. Each protection is named only if the kernel was seen to provide it.
fn sandbox_checks(c: &mut Checks, caps: &Caps) {
    match caps.landlock_abi {
        Some(abi) => {
            let shown = if abi >= 6 { format!("{abi} or later") } else { abi.to_string() };
            c.add(
                "sandbox",
                Status::Ok,
                format!("Landlock, ABI {shown}. Commands can write only to their run's worktree and its own home and temp folders."),
            );
            if abi >= 4 {
                c.add(
                    "sandbox network",
                    Status::Ok,
                    "Landlock TCP rules (ABI 4): a project with `network = false` gets TCP blocked. UDP is not blocked, so this is not network isolation.",
                );
            } else {
                c.add(
                    "sandbox network",
                    Status::Note,
                    "This kernel's Landlock has no TCP rules (they need ABI 4). A project with `network = false` is refused under Landlock; use the container sandbox for it.",
                );
            }
            if abi >= 6 {
                c.add("sandbox signals", Status::Ok, "Landlock scoping (ABI 6): commands cannot signal processes outside the sandbox.");
            } else {
                c.add("sandbox signals", Status::Note, "Commands can still signal your other processes (blocking that needs Landlock ABI 6).");
            }
        }
        None if caps.runtime.is_some() => c.add(
            "sandbox",
            Status::Note,
            format!("{}A project with a `container_image` runs its commands in a container; any other project asks before every edit and command.", patina_tools::sandbox::no_landlock()),
        ),
        None => c.add(
            "sandbox",
            Status::Note,
            format!("{}No sandbox is available. Every edit and command asks for approval, and autopilot and `patina run --yes` are refused.", patina_tools::sandbox::no_landlock()),
        ),
    }
    match &caps.runtime {
        Some((name, version)) => c.add("container runtime", Status::Ok, format!("{name} {version}")),
        None => c.add(
            "container runtime",
            Status::Note,
            "none answered (docker or podman). The container sandbox is not available.",
        ),
    }
    let file_size = if cfg!(unix) {
        " Every command gets the project's cap on the size of a file it writes (RLIMIT_FSIZE)."
    } else {
        ""
    };
    let container = if caps.runtime.is_some() {
        " Projects on the container backend get their limits on the run's container."
    } else {
        ""
    };
    match &caps.cgroup {
        CgroupState::Active(controllers) => c.add(
            "resource limits",
            Status::Ok,
            format!("{} per command through cgroup v2.{file_size}{container}", describe(controllers)),
        ),
        CgroupState::Possible(controllers) => c.add(
            "resource limits",
            Status::Ok,
            format!(
                "This process's cgroup is delegated, so a daemon started the same way caps {} per command through cgroup v2.{file_size}{container}",
                describe(controllers)
            ),
        ),
        CgroupState::Unavailable(why) => c.add(
            "resource limits",
            Status::Note,
            format!(
                "No cgroup v2 limits: {why}. Memory, processes and CPU are not capped for commands outside a container; run the daemon as a systemd unit with Delegate=yes to cap them.{file_size}{container}"
            ),
        ),
    }
}

async fn port_check(c: &mut Checks, name: &str, bind: &str, port: u16) {
    match tokio::net::TcpListener::bind((bind, port)).await {
        Ok(_) => c.add(name, Status::Ok, format!("{bind}:{port} is free")),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => c.add(
            name,
            Status::Note,
            format!("{bind}:{port} is in use. That is expected while the daemon is running."),
        ),
        Err(e) => c.add(name, Status::Problem, format!("cannot bind {bind}:{port}: {e}")),
    }
}

/// Read a file that may be absent. `Ok(None)` when it does not exist.
fn read(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

fn config_checks(c: &mut Checks, paths: &Paths) -> (Config, ProjectsFile) {
    let cfg = match read(&paths.config_file()).and_then(|t| t.map(|t| Config::parse(&t).map_err(|e| e.to_string())).transpose()) {
        Ok(Some(cfg)) => {
            c.add("config.toml", Status::Ok, paths.config_file().display().to_string());
            cfg
        }
        Ok(None) => {
            c.add("config.toml", Status::Ok, "not created yet; the defaults apply");
            Config::default()
        }
        Err(e) => {
            c.add("config.toml", Status::Problem, e);
            Config::default()
        }
    };
    let providers = paths.config_dir.join("providers.toml");
    match read(&providers).and_then(|t| t.map(|t| ProvidersFile::parse(&t).map_err(|e| e.to_string())).transpose()) {
        Ok(Some(f)) => c.add("providers.toml", Status::Ok, format!("{} provider(s)", f.providers.len())),
        Ok(None) => c.add("providers.toml", Status::Note, "no provider is set up yet"),
        Err(e) => c.add("providers.toml", Status::Problem, e),
    }
    let projects = match read(&paths.config_dir.join("projects.toml")).and_then(|t| t.map(|t| ProjectsFile::parse(&t)).transpose()) {
        Ok(Some(f)) => {
            c.add("projects.toml", Status::Ok, format!("{} project(s)", f.projects.len()));
            f
        }
        Ok(None) => {
            c.add("projects.toml", Status::Ok, "no project is registered yet");
            ProjectsFile::default()
        }
        Err(e) => {
            c.add("projects.toml", Status::Problem, e);
            ProjectsFile::default()
        }
    };
    let (mut good, mut bad) = (0, vec![]);
    if let Ok(dir) = std::fs::read_dir(paths.config_dir.join("agents")) {
        for entry in dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            match read(&path).and_then(|t| AgentConfig::parse(&t.unwrap_or_default())) {
                Ok(_) => good += 1,
                Err(e) => bad.push(format!("{}: {e}", path.display())),
            }
        }
    }
    if bad.is_empty() {
        c.add("agents", Status::Ok, format!("{good} agent file(s)"));
    } else {
        c.add("agents", Status::Problem, bad.join("\n"));
    }
    (cfg, projects)
}

async fn project_checks(c: &mut Checks, projects: &ProjectsFile, caps: &Caps) {
    for p in &projects.projects {
        let name = format!("project {}", p.id);
        if !git::is_repo_root(Path::new(&p.path)).await {
            c.add(
                &name,
                Status::Problem,
                format!("{} is not the top level of a git repository", p.path),
            );
            continue;
        }
        match caps.resolve(p.sandbox, p.container_image.is_some()) {
            Err(e) => c.add(
                &name,
                Status::Problem,
                format!("its sandbox setting cannot be met, so its runs are refused: {e}"),
            ),
            Ok(SandboxLevel::None) => c.add(&name, Status::Note, "no sandbox: every edit and command asks for approval"),
            Ok(level) => {
                let network = if p.network { "network on" } else { "network off" };
                c.add(
                    &name,
                    Status::Ok,
                    format!("sandbox {}, policy {}, {network}", level.as_str(), policy_name(p.policy)),
                )
            }
        }
    }
}

fn policy_name(p: patina_core::policy::PolicyMode) -> &'static str {
    match p {
        patina_core::policy::PolicyMode::ConfirmAll => "confirm_all",
        patina_core::policy::PolicyMode::AutoEdit => "auto_edit",
        patina_core::policy::PolicyMode::Autopilot => "autopilot",
    }
}

/// The sub-folders of `dir`, by name. Empty when the folder does not exist.
fn folders(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Worktrees and sandbox folders on disk that no run owns, and runs whose worktree is gone.
async fn worktree_checks(c: &mut Checks, paths: &Paths) {
    let db_file = paths.database_file();
    let on_disk = folders(&paths.data_dir.join("worktrees"));
    let private = folders(&paths.data_dir.join("sandbox"));
    if !db_file.exists() {
        if on_disk.is_empty() && private.is_empty() {
            c.add("worktrees", Status::Ok, "no data yet");
        } else {
            c.add(
                "worktrees",
                Status::Problem,
                format!("{} holds run folders but no database", paths.data_dir.display()),
            );
        }
        return;
    }
    let rows = match patina_store::Db::open(&db_file) {
        Ok(db) => patina_store::runs::list_runs(&db, i64::MAX).await.map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    let rows = match rows {
        Ok(r) => r,
        Err(e) => return c.add("database", Status::Problem, format!("cannot read {}: {e}", db_file.display())),
    };
    c.add("database", Status::Ok, format!("{} run(s)", rows.len()));
    // A run owns its folders for as long as it has a worktree: until it is merged or discarded.
    let owners: HashSet<&str> = rows.iter().filter(|r| r.worktree_path.is_some()).map(|r| r.id.as_str()).collect();
    let mut found = vec![];
    for (kind, names) in [("worktrees", &on_disk), ("sandbox", &private)] {
        for name in names.iter().filter(|n| !owners.contains(n.as_str())) {
            found.push(format!("orphan: {}", paths.data_dir.join(kind).join(name).display()));
        }
    }
    for r in rows
        .iter()
        .filter(|r| r.worktree_path.as_deref().is_some_and(|w| !Path::new(w).is_dir()))
    {
        found.push(format!(
            "run {} has lost its worktree {}",
            r.id,
            r.worktree_path.as_deref().unwrap_or("")
        ));
    }
    if found.is_empty() {
        c.add("worktrees", Status::Ok, format!("{} in use, no orphans", owners.len()));
    } else {
        found.push("An orphan folder belongs to no run and can be deleted. In its project, `git worktree prune` forgets it.".into());
        c.add("worktrees", Status::Note, found.join("\n"));
    }
}

pub async fn report(paths: &Paths) -> Report {
    let mut c = Checks(vec![]);
    c.add(
        "folders",
        Status::Ok,
        format!("config {}\ndata {}", paths.config_dir.display(), paths.data_dir.display()),
    );
    match git::version().await {
        Some(v) => c.add("git", Status::Ok, v),
        None => c.add("git", Status::Problem, "git was not found on PATH. Projects need it."),
    }
    let caps = Caps::detect().await;
    sandbox_checks(&mut c, &caps);
    let (cfg, projects) = config_checks(&mut c, paths);
    port_check(&mut c, "port", &cfg.bind, cfg.port).await;
    // The preview always listens on loopback; 0 turns it off.
    if cfg.preview_port != 0 {
        port_check(&mut c, "preview port", "127.0.0.1", cfg.preview_port).await;
    }
    project_checks(&mut c, &projects, &caps).await;
    worktree_checks(&mut c, paths).await;
    Report {
        version: env!("CARGO_PKG_VERSION").to_string(),
        checks: c.0,
        sandbox: caps.status(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(caps: Caps) -> String {
        let mut c = Checks(vec![]);
        sandbox_checks(&mut c, &caps);
        c.0.iter().map(|c| format!("{:?} {}: {}\n", c.status, c.name, c.detail)).collect()
    }

    #[test]
    fn the_sandbox_lines_name_only_what_the_kernel_provides() {
        let old = lines(Caps {
            landlock_abi: Some(3),
            runtime: None,
            cgroup: CgroupState::Unavailable("its cgroup is not delegated to it".into()),
        });
        assert!(old.contains("Ok sandbox: Landlock, ABI 3."), "{old}");
        assert!(
            old.contains("Note sandbox network: This kernel's Landlock has no TCP rules"),
            "{old}"
        );
        assert!(old.contains("Note sandbox signals"), "{old}");
        let new = lines(Caps {
            landlock_abi: Some(6),
            runtime: Some(("docker".into(), "27.1".into())),
            cgroup: CgroupState::Active(vec!["memory".into(), "pids".into(), "cpu".into()]),
        });
        assert!(
            new.contains("ABI 6 or later") && new.contains("UDP is not blocked") && new.contains("Ok container runtime: docker 27.1"),
            "{new}"
        );
        let none = lines(Caps::default());
        assert!(
            none.contains("Note sandbox: ") && none.contains("No sandbox is available") && none.contains("`patina run --yes` are refused"),
            "{none}"
        );
        if cfg!(not(target_os = "linux")) {
            assert!(none.contains("not available on this operating system"), "{none}");
        }
        // Limits are named only where they are enforced.
        assert!(
            old.contains("Note resource limits: No cgroup v2 limits: its cgroup is not delegated") && !old.contains("container backend"),
            "{old}"
        );
        assert!(
            new.contains("Ok resource limits: memory, processes and CPU per command through cgroup v2.")
                && new.contains("on the run's container"),
            "{new}"
        );
    }

    #[tokio::test]
    async fn broken_config_and_orphaned_folders_are_reported() {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::resolve(Some(&t.path().join("config")), Some(&t.path().join("data"))).unwrap();
        // Nothing exists yet: that is fine, and doctor creates nothing.
        let fresh = report(&paths).await;
        assert!(
            fresh.checks.iter().filter(|c| c.name != "git").all(|c| c.status != Status::Problem),
            "{:?}",
            fresh.checks
        );
        assert!(!paths.config_dir.exists() && !paths.data_dir.exists());

        paths.ensure().unwrap();
        std::fs::create_dir_all(paths.config_dir.join("agents")).unwrap();
        std::fs::write(paths.config_dir.join("agents/bad.toml"), "not toml at all [").unwrap();
        std::fs::write(
            paths.config_dir.join("projects.toml"),
            format!(
                "schema_version = 1\n[[project]]\nid = \"gone\"\nname = \"Gone\"\npath = '{}'\n",
                t.path().join("missing").display()
            ),
        )
        .unwrap();
        patina_store::Db::open(&paths.database_file()).unwrap();
        std::fs::create_dir_all(paths.data_dir.join("worktrees/abandoned")).unwrap();
        std::fs::create_dir_all(paths.data_dir.join("sandbox/abandoned/home")).unwrap();
        let r = report(&paths).await;
        let find = |name: &str| {
            r.checks
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("no check {name}: {:?}", r.checks))
        };
        assert_eq!(find("agents").status, Status::Problem);
        assert!(find("agents").detail.contains("bad.toml"));
        assert_eq!(find("project gone").status, Status::Problem);
        assert!(find("project gone").detail.contains("not the top level of a git repository"));
        let w = find("worktrees");
        assert_eq!(w.status, Status::Note);
        assert_eq!(w.detail.matches("orphan: ").count(), 2, "{}", w.detail);
        assert!(!r.healthy());
        let mut text = Vec::new();
        r.print(&mut text).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(text.contains("PROBLEM  agents:") && text.contains("2 problem(s) found."), "{text}");
    }
}
