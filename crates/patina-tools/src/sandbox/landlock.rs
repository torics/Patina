//! The Landlock backend (Linux). A command is started through a small helper mode of the Patina binary:
//! the helper restricts itself with Landlock and `no_new_privs`, then replaces itself with the shell. The
//! restriction is inherited by everything the shell starts and can never be lifted.
//!
//! Landlock confines file access (and TCP from ABI 4). It does not hide process ids, and it is not a defense
//! against a kernel exploit.
use super::{Setup, base_env, overlaps, shell};
use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetError, RulesetStatus,
    Scope, path_beneath_rules,
};
use patina_core::sandbox::{Prepared, Sandbox, SandboxError, SandboxLevel};
use std::path::{Path, PathBuf};

/// The subcommand of the Patina binary that applies a policy and runs a program.
pub const HELPER_COMMAND: &str = "sandbox-exec";

/// System and toolchain locations commands may read and execute from. Missing ones are skipped.
const SYSTEM_READ: [&str; 12] = [
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/etc", "/opt", "/nix", "/proc", "/sys",
];
/// Device files commands may read and write.
const DEVICES: [&str; 6] = ["/dev/null", "/dev/zero", "/dev/full", "/dev/random", "/dev/urandom", "/dev/tty"];

/// The file access rights are requested as of this ABI; a kernel that knows fewer enforces the ones it knows.
const FS_ABI: ABI = ABI::V5;

/// The highest Landlock ABI this kernel supports, up to 6, found by asking for each version's rights as a hard
/// requirement. Creating a ruleset restricts nothing. `None` means no Landlock.
pub fn probe_abi() -> Option<u32> {
    let hard = || Ruleset::default().set_compatibility(CompatLevel::HardRequirement);
    let fs = |abi: ABI| hard().handle_access(AccessFs::from_all(abi)).and_then(|r| r.create()).is_ok();
    if !fs(ABI::V1) {
        return None;
    }
    let net = hard().handle_access(AccessNet::from_all(ABI::V4)).and_then(|r| r.create()).is_ok();
    let scope = hard()
        .handle_access(AccessFs::from_all(ABI::V1))
        .and_then(|r| r.scope(Scope::from_all(ABI::V6)))
        .and_then(|r| r.create())
        .is_ok();
    // Each ABI includes the ones before it, so stop at the first one that is missing.
    let steps = [fs(ABI::V2), fs(ABI::V3), net, fs(ABI::V5), scope];
    Some(1 + steps.iter().take_while(|ok| **ok).count() as u32)
}

/// What a command may touch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Policy {
    /// Folders with full access. Each must exist.
    pub rw: Vec<PathBuf>,
    /// Folders and files with read and execute access.
    pub ro: Vec<PathBuf>,
    /// Device files with read and write access.
    pub dev: Vec<PathBuf>,
    /// Deny TCP connect and bind. Needs ABI 4; fails closed without it.
    pub block_tcp: bool,
}

/// Restrict the current process, for good. On success nothing outside the policy can be opened any more.
pub fn restrict(policy: &Policy) -> Result<(), String> {
    if let Some(missing) = policy.rw.iter().find(|p| !p.is_dir()) {
        return Err(format!("{} does not exist", missing.display()));
    }
    let build = || -> Result<RulesetStatus, RulesetError> {
        let mut ruleset = Ruleset::default().handle_access(AccessFs::from_all(FS_ABI))?;
        if policy.block_tcp {
            // The owner asked for this, so it must hold or the command must not run.
            ruleset = ruleset
                .set_compatibility(CompatLevel::HardRequirement)
                .handle_access(AccessNet::from_all(ABI::V4))?
                .set_compatibility(CompatLevel::BestEffort);
        }
        // From ABI 6: no signals to, and no abstract socket connections with, processes outside the sandbox.
        let created = ruleset
            .scope(Scope::from_all(ABI::V6))?
            .create()?
            .add_rules(path_beneath_rules(&policy.rw, AccessFs::from_all(FS_ABI)))?
            .add_rules(path_beneath_rules(&policy.ro, AccessFs::from_read(FS_ABI)))?
            .add_rules(path_beneath_rules(&policy.dev, AccessFs::from_file(FS_ABI)))?;
        let status = created.restrict_self()?;
        if !status.no_new_privs {
            return Ok(RulesetStatus::NotEnforced);
        }
        Ok(status.ruleset)
    };
    match build() {
        Ok(RulesetStatus::NotEnforced) => return Err("Landlock is not enforced on this kernel".into()),
        Ok(_) => {}
        Err(e) => return Err(e.to_string()),
    }
    drop_capabilities()
}

/// Give up every capability. Landlock does not take capabilities away, and a process that keeps them (the
/// daemon running as root, for example) could still read other processes' memory and environment through
/// `/proc`. With `no_new_privs` set, nothing the command starts can get them back.
fn drop_capabilities() -> Result<(), String> {
    use rustix::thread::{CapabilitySet, CapabilitySets};
    // The bounding set first, while the capability to change it is still held. An unprivileged process cannot
    // change it and does not need to: `no_new_privs` already stops any gain.
    for bit in 0..64u32 {
        let _ = rustix::thread::remove_capability_from_bounding_set(CapabilitySet::from_bits_retain(1 << bit));
    }
    let _ = rustix::thread::clear_ambient_capability_set();
    let none = CapabilitySets {
        effective: CapabilitySet::empty(),
        permitted: CapabilitySet::empty(),
        inheritable: CapabilitySet::empty(),
    };
    rustix::thread::set_capabilities(None, none).map_err(|e| format!("cannot drop capabilities: {e}"))?;
    let now = rustix::thread::capabilities(None).map_err(|e| format!("cannot read capabilities: {e}"))?;
    if !now.effective.is_empty() || !now.permitted.is_empty() || !now.inheritable.is_empty() {
        return Err("capabilities are still held after dropping them".into());
    }
    Ok(())
}

/// The helper: apply the policy, then become `program`. Returns only on failure, with the reason.
pub fn exec_restricted(policy: &Policy, program: &Path, args: &[String]) -> String {
    use std::os::unix::process::CommandExt;
    if let Err(e) = restrict(policy) {
        return format!("the sandbox could not be set up, so the command was not run: {e}");
    }
    let err = std::process::Command::new(program).args(args).exec();
    format!("cannot start {}: {err}", program.display())
}

pub struct LandlockSandbox {
    setup: Setup,
    policy: Policy,
    abi: u32,
}

impl LandlockSandbox {
    pub fn new(abi: Option<u32>, setup: Setup) -> Result<LandlockSandbox, SandboxError> {
        let abi = abi.ok_or_else(|| SandboxError::Setup("this host does not support Landlock".into()))?;
        if !setup.network && abi < 4 {
            return Err(SandboxError::Setup(format!(
                "the project turns the network off, but this kernel's Landlock (ABI {abi}) cannot block TCP; that needs ABI 4, or the container sandbox"
            )));
        }
        let real_home = std::env::var_os("HOME").map(PathBuf::from);
        let mut ro: Vec<PathBuf> = SYSTEM_READ.iter().map(PathBuf::from).collect();
        // Toolchains often live outside the system folders: every folder on PATH is readable and executable.
        let path = setup
            .env
            .get("PATH")
            .cloned()
            .or_else(|| std::env::var("PATH").ok())
            .unwrap_or_default();
        for dir in std::env::split_paths(&path) {
            let safe = dir.is_absolute()
                && dir != Path::new("/")
                && !setup.protected.iter().any(|own| overlaps(&dir, own))
                // A PATH entry that is the home folder, or contains it, would open the whole home.
                && !real_home.as_ref().is_some_and(|home| home.starts_with(&dir));
            if safe && !ro.contains(&dir) {
                ro.push(dir);
            }
        }
        ro.extend(setup.extra_read.iter().cloned());
        let mut rw = vec![setup.worktree.clone(), setup.home.clone(), setup.tmp.clone()];
        rw.extend(setup.extra_write.iter().cloned());
        let policy = Policy {
            rw,
            ro,
            dev: DEVICES.iter().map(PathBuf::from).collect(),
            block_tcp: !setup.network,
        };
        Ok(LandlockSandbox { setup, policy, abi })
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }
}

/// The helper's command line for a policy and a program.
pub fn helper_args(policy: &Policy, program: &Path, args: &[String]) -> Vec<String> {
    let mut out = vec![HELPER_COMMAND.to_string()];
    for (flag, paths) in [("--rw", &policy.rw), ("--ro", &policy.ro), ("--dev", &policy.dev)] {
        for p in paths {
            out.push(flag.to_string());
            out.push(p.to_string_lossy().into_owned());
        }
    }
    if policy.block_tcp {
        out.push("--block-tcp".into());
    }
    out.push("--".into());
    out.push(program.to_string_lossy().into_owned());
    out.extend(args.iter().cloned());
    out
}

impl Sandbox for LandlockSandbox {
    fn level(&self) -> SandboxLevel {
        SandboxLevel::Landlock
    }
    fn describe(&self) -> String {
        format!(
            "Landlock (ABI {}{}): commands can write only to this run's worktree, home and temp folders{}{}",
            self.abi,
            if self.abi >= 6 { " or later" } else { "" },
            if self.setup.extra_write.is_empty() {
                ""
            } else {
                " and the project's sandbox_write folders"
            },
            if self.policy.block_tcp {
                "; TCP is blocked, UDP is not"
            } else {
                "; the network is open"
            },
        )
    }
    fn prepare(&self, command: &str, marker: &str) -> Result<Prepared, SandboxError> {
        let (sh, sh_args) = shell(command);
        Ok(Prepared {
            program: self.setup.helper.clone(),
            args: helper_args(&self.policy, &sh, &sh_args),
            env: base_env(&self.setup, &self.setup.home, marker),
            cwd: Some(self.setup.worktree.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(path: &str, network: bool) -> Setup {
        Setup {
            worktree: "/data/patina/worktrees/r1".into(),
            home: "/data/patina/sandbox/r1/home".into(),
            tmp: "/data/patina/sandbox/r1/tmp".into(),
            helper: "/usr/bin/patina".into(),
            network,
            env: [("PATH".to_string(), path.to_string())].into(),
            extra_read: vec!["/srv/toolchain".into()],
            extra_write: vec!["/srv/cache".into()],
            protected: vec!["/data/patina".into(), "/home/owner/.config/patina".into()],
            ..Default::default()
        }
    }

    #[test]
    fn the_policy_grants_the_run_folders_and_toolchains_and_nothing_of_patinas() {
        let s = LandlockSandbox::new(
            Some(6),
            setup("/usr/bin:/home/owner/.cargo/bin:relative/bin:/:/data/patina/bin:/data", true),
        )
        .unwrap();
        let p = s.policy();
        assert_eq!(
            p.rw,
            [
                "/data/patina/worktrees/r1",
                "/data/patina/sandbox/r1/home",
                "/data/patina/sandbox/r1/tmp",
                "/srv/cache"
            ]
            .map(PathBuf::from)
        );
        assert!(
            p.ro.contains(&PathBuf::from("/usr"))
                && p.ro.contains(&PathBuf::from("/home/owner/.cargo/bin"))
                && p.ro.contains(&PathBuf::from("/srv/toolchain"))
        );
        for never in [
            "relative/bin",
            "/",
            "/data/patina/bin",
            "/data",
            "/home",
            "/home/owner",
            "/root",
            "/tmp",
            "/var",
        ] {
            assert!(
                !p.ro.contains(&PathBuf::from(never)) && !p.rw.contains(&PathBuf::from(never)),
                "{never}"
            );
        }
        assert!(!p.block_tcp);
    }

    #[test]
    fn turning_the_network_off_needs_abi_4_and_fails_closed_without_it() {
        assert!(LandlockSandbox::new(Some(4), setup("/usr/bin", false)).unwrap().policy().block_tcp);
        let err = LandlockSandbox::new(Some(3), setup("/usr/bin", false)).err().unwrap().to_string();
        assert!(err.contains("cannot block TCP"), "{err}");
        assert!(LandlockSandbox::new(None, setup("/usr/bin", true)).is_err());
    }

    #[test]
    fn the_helper_command_carries_the_whole_policy_and_a_private_home() {
        let s = LandlockSandbox::new(Some(4), setup("/usr/bin", false)).unwrap();
        let p = s.prepare("make test", "m1").unwrap();
        assert_eq!(p.program, PathBuf::from("/usr/bin/patina"));
        assert_eq!(p.args[0], "sandbox-exec");
        let tail: Vec<&str> = p.args[p.args.len() - 5..].iter().map(String::as_str).collect();
        assert_eq!(tail, ["--block-tcp", "--", "/bin/sh", "-c", "make test"]);
        assert!(p.args.windows(2).any(|w| w == ["--rw", "/data/patina/worktrees/r1"]));
        assert!(p.env.contains(&("HOME".to_string(), "/data/patina/sandbox/r1/home".to_string())));
        assert_eq!(p.cwd, Some(PathBuf::from("/data/patina/worktrees/r1")));
    }
}
