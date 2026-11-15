//! cgroup v2 limits for commands, where the daemon was given a cgroup subtree of its own.
//!
//! A systemd unit with `Delegate=yes` hands the daemon its cgroup. Patina then moves itself into a `daemon`
//! child, because a cgroup that holds processes cannot give controllers to its children, and gives each
//! command a cgroup of its own under `commands`, with `memory.max`, `pids.max` and `cpu.max` set from the
//! project's limits. Stopping a command kills its whole cgroup, which also reaches processes that left the
//! command's process group and cleared its marker.
//!
//! Nothing here happens unless the daemon's cgroup is delegated to it and holds no other process. Every step
//! that fails is undone, and the daemon then runs without cgroup limits and says so.
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(not(target_os = "linux"))]
pub use other::*;

/// What `detect` found.
#[derive(Debug, Clone, PartialEq)]
pub enum CgroupState {
    /// This process gives each command its own cgroup with these controllers.
    Active(Vec<String>),
    /// This process has not set anything up, but its cgroup is delegated and has these controllers: a daemon
    /// started the same way would use them.
    Possible(Vec<String>),
    /// No cgroup limits, and why.
    Unavailable(String),
}

impl Default for CgroupState {
    fn default() -> Self {
        CgroupState::Unavailable("not checked".into())
    }
}

/// The controllers Patina uses, in the order they are listed.
pub const WANTED: [&str; 3] = ["memory", "pids", "cpu"];

/// "memory, processes and CPU" for the controllers given.
pub fn describe(controllers: &[String]) -> String {
    let names: Vec<&str> = WANTED
        .iter()
        .filter(|c| controllers.iter().any(|have| have == *c))
        .map(|c| match *c {
            "memory" => "memory",
            "pids" => "processes",
            _ => "CPU",
        })
        .collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{CgroupState, WANTED};
    use patina_core::sandbox::ResourceLimits;
    use rustix::fd::{AsRawFd, OwnedFd};
    use rustix::fs::{Access, Mode, OFlags};
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    const MOUNT: &str = "/sys/fs/cgroup";

    /// The daemon's cgroup tree, once it was set up.
    #[derive(Debug)]
    pub struct Tree {
        own: PathBuf,
        commands: PathBuf,
        controllers: Vec<String>,
    }

    static TREE: OnceLock<Result<Tree, String>> = OnceLock::new();

    fn read(path: &Path) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
    }

    fn write(path: &Path, value: &str) -> Result<(), String> {
        std::fs::write(path, value).map_err(|e| format!("cannot write {value:?} to {}: {e}", path.display()))
    }

    /// The daemon's own cgroup folder and the controllers it may hand out, if it was delegated. Changes nothing.
    pub fn probe() -> Result<(PathBuf, Vec<String>), String> {
        if !Path::new(MOUNT).join("cgroup.controllers").exists() {
            return Err("this host does not use the cgroup v2 unified hierarchy".into());
        }
        let own_line = read(Path::new("/proc/self/cgroup"))?
            .lines()
            .find_map(|l| l.strip_prefix("0::").map(str::to_string))
            .ok_or("this process is not in a cgroup v2 hierarchy")?;
        let relative = own_line.trim().trim_start_matches('/');
        if relative.is_empty() {
            return Err("this process runs in the root cgroup, which is not delegated to it".into());
        }
        let own = Path::new(MOUNT).join(relative);
        for file in ["", "cgroup.procs", "cgroup.subtree_control"] {
            let path = own.join(file);
            if rustix::fs::access(&path, Access::WRITE_OK).is_err() {
                return Err(format!(
                    "its cgroup {} is not delegated to it (run it as a systemd unit with Delegate=yes)",
                    own.display()
                ));
            }
        }
        let available = read(&own.join("cgroup.controllers"))?;
        let controllers: Vec<String> = WANTED
            .iter()
            .filter(|c| available.split_whitespace().any(|a| a == **c))
            .map(|c| c.to_string())
            .collect();
        if controllers.is_empty() {
            return Err(format!(
                "its cgroup {} has none of the memory, pids and cpu controllers",
                own.display()
            ));
        }
        let me = std::process::id().to_string();
        let others = read(&own.join("cgroup.procs"))?.lines().filter(|p| p.trim() != me).count();
        if others > 0 {
            return Err(format!(
                "other processes share its cgroup {}, so it is not Patina's to divide",
                own.display()
            ));
        }
        Ok((own, controllers))
    }

    /// Set up the daemon's cgroup tree, once per process. Later calls return the first outcome.
    pub fn init() -> Result<&'static Tree, String> {
        TREE.get_or_init(setup).as_ref().map_err(String::clone)
    }

    /// The tree, if `init` set one up.
    pub fn active() -> Option<&'static Tree> {
        TREE.get().and_then(|t| t.as_ref().ok())
    }

    /// What this process has, or could have.
    pub fn detect() -> CgroupState {
        match TREE.get() {
            Some(Ok(t)) => CgroupState::Active(t.controllers.clone()),
            Some(Err(e)) => CgroupState::Unavailable(e.clone()),
            None => match probe() {
                Ok((_, controllers)) => CgroupState::Possible(controllers),
                Err(e) => CgroupState::Unavailable(e),
            },
        }
    }

    fn setup() -> Result<Tree, String> {
        let (own, controllers) = probe()?;
        let daemon = own.join("daemon");
        let commands = own.join("commands");
        let enable: String = controllers.iter().map(|c| format!("+{c} ")).collect();
        let disable: String = controllers.iter().map(|c| format!("-{c} ")).collect();
        let me = std::process::id().to_string();
        // Each step, and the steps that undo what came before it when it fails.
        let undo = |moved: bool, enabled: bool| {
            let _ = std::fs::remove_dir(&commands);
            if enabled {
                let _ = std::fs::write(own.join("cgroup.subtree_control"), disable.trim());
            }
            if moved {
                let _ = std::fs::write(own.join("cgroup.procs"), &me);
            }
            let _ = std::fs::remove_dir(&daemon);
        };
        mkdir(&daemon)?;
        if let Err(e) = write(&daemon.join("cgroup.procs"), &me) {
            undo(false, false);
            return Err(e);
        }
        if let Err(e) = write(&own.join("cgroup.subtree_control"), enable.trim()) {
            undo(true, false);
            return Err(e);
        }
        let commands_ready = mkdir(&commands).and_then(|_| {
            remove_leftovers(&commands);
            write(&commands.join("cgroup.subtree_control"), enable.trim())
        });
        if let Err(e) = commands_ready {
            undo(true, true);
            return Err(e);
        }
        Ok(Tree {
            own,
            commands,
            controllers,
        })
    }

    fn mkdir(path: &Path) -> Result<(), String> {
        match std::fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(format!("cannot create {}: {e}", path.display())),
        }
    }

    /// Kill and remove the cgroups of commands that an earlier daemon in the same cgroup left behind.
    fn remove_leftovers(commands: &Path) {
        let Ok(entries) = std::fs::read_dir(commands) else { return };
        for dir in entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
            kill_dir(&dir);
            remove_when_empty(&dir, Duration::from_secs(2));
        }
    }

    fn kill_dir(dir: &Path) {
        if std::fs::write(dir.join("cgroup.kill"), "1").is_ok() {
            return;
        }
        // Before Linux 5.14 there is no cgroup.kill: kill each process, a few times over in case one forks.
        for _ in 0..5 {
            let Ok(procs) = std::fs::read_to_string(dir.join("cgroup.procs")) else {
                return;
            };
            let mut any = false;
            for pid in procs.lines().filter_map(|l| l.trim().parse::<i32>().ok()) {
                if let Some(p) = rustix::process::Pid::from_raw(pid) {
                    let _ = rustix::process::kill_process(p, rustix::process::Signal::KILL);
                    any = true;
                }
            }
            if !any {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn populated(dir: &Path) -> bool {
        std::fs::read_to_string(dir.join("cgroup.events"))
            .map(|e| e.lines().any(|l| l.trim() == "populated 1"))
            .unwrap_or(false)
    }

    /// Remove a cgroup once its processes are gone. Returns whether it is gone.
    fn remove_when_empty(dir: &Path, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        loop {
            match std::fs::remove_dir(dir) {
                Ok(()) => return true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                Err(_) => return false,
            }
        }
    }

    impl Tree {
        pub fn controllers(&self) -> &[String] {
            &self.controllers
        }

        /// The daemon's own cgroup folder.
        pub fn own(&self) -> &Path {
            &self.own
        }

        /// The folder that holds one cgroup per running command.
        pub fn commands(&self) -> &Path {
            &self.commands
        }

        fn has(&self, controller: &str) -> bool {
            self.controllers.iter().any(|c| c == controller)
        }

        /// A new cgroup for one command, with its limits set. The command joins it as it starts.
        pub fn leaf(&self, name: &str, limits: &ResourceLimits) -> Result<Leaf, String> {
            let safe: String = name
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            let dir = self.commands.join(safe);
            std::fs::create_dir(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            let configured = (|| {
                if self.has("memory")
                    && let Some(bytes) = limits.memory_bytes()
                {
                    write(&dir.join("memory.max"), &bytes.to_string())?;
                    // Without this a command that runs out of memory pages the host to a halt instead of stopping.
                    // Not every kernel has swap accounting, so a missing file is fine.
                    if dir.join("memory.swap.max").exists() {
                        write(&dir.join("memory.swap.max"), "0")?;
                    }
                }
                if self.has("pids") && limits.processes > 0 {
                    write(&dir.join("pids.max"), &limits.processes.to_string())?;
                }
                if self.has("cpu") && limits.cpus > 0.0 {
                    let period = 100_000u64;
                    let quota = ((limits.cpus * period as f64) as u64).max(1000);
                    write(&dir.join("cpu.max"), &format!("{quota} {period}"))?;
                }
                rustix::fs::open(dir.join("cgroup.procs"), OFlags::WRONLY | OFlags::CLOEXEC, Mode::empty())
                    .map_err(|e| format!("cannot open {}: {e}", dir.join("cgroup.procs").display()))
            })();
            match configured {
                Ok(procs) => Ok(Leaf { dir, procs }),
                Err(e) => {
                    let _ = std::fs::remove_dir(&dir);
                    Err(e)
                }
            }
        }
    }

    /// One command's cgroup. Dropping it kills what is left in it and removes it.
    #[derive(Debug)]
    pub struct Leaf {
        dir: PathBuf,
        procs: OwnedFd,
    }

    impl Leaf {
        /// The open `cgroup.procs` of this cgroup. Writing "0" to it moves the writer in, which the child does
        /// between fork and exec, so the command never runs outside its cgroup.
        pub fn procs_fd(&self) -> i32 {
            self.procs.as_raw_fd()
        }

        pub fn dir(&self) -> &Path {
            &self.dir
        }

        /// Kill every process in the cgroup, wherever it went.
        pub fn kill(&self) {
            kill_dir(&self.dir);
        }

        /// Whether any process is still in the cgroup.
        pub fn populated(&self) -> bool {
            populated(&self.dir)
        }
    }

    impl Drop for Leaf {
        fn drop(&mut self) {
            if self.populated() {
                kill_dir(&self.dir);
            }
            if !remove_when_empty(&self.dir, Duration::from_millis(200)) {
                // A killed process can take a moment to leave; remove the cgroup once it has.
                let dir = self.dir.clone();
                std::thread::spawn(move || {
                    kill_dir(&dir);
                    remove_when_empty(&dir, Duration::from_secs(10));
                });
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod other {
    use super::CgroupState;
    use patina_core::sandbox::ResourceLimits;

    /// cgroups exist only on Linux.
    #[derive(Debug)]
    pub struct Tree;

    #[derive(Debug)]
    pub struct Leaf;

    pub fn init() -> Result<&'static Tree, String> {
        Err("cgroups exist only on Linux".into())
    }

    pub fn active() -> Option<&'static Tree> {
        None
    }

    pub fn detect() -> CgroupState {
        CgroupState::Unavailable("cgroups exist only on Linux".into())
    }

    impl Tree {
        pub fn controllers(&self) -> &[String] {
            &[]
        }
        pub fn own(&self) -> &std::path::Path {
            std::path::Path::new("")
        }
        pub fn leaf(&self, _name: &str, _limits: &ResourceLimits) -> Result<Leaf, String> {
            Err("cgroups exist only on Linux".into())
        }
    }

    impl Leaf {
        pub fn kill(&self) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_controllers_are_named_for_people() {
        let names = |c: &[&str]| describe(&c.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(names(&["memory", "pids", "cpu"]), "memory, processes and CPU");
        assert_eq!(names(&["cpu", "memory"]), "memory and CPU");
        assert_eq!(names(&["pids"]), "processes");
        assert_eq!(names(&[]), "");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn detection_changes_nothing() {
        // Whatever the host, looking does not move this process or create anything.
        let before = std::fs::read_to_string("/proc/self/cgroup").unwrap();
        let _ = detect();
        assert_eq!(std::fs::read_to_string("/proc/self/cgroup").unwrap(), before);
        assert!(active().is_none());
    }
}
