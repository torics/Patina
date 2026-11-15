//! `projects.toml`: registered project folders and their policy.
use crate::policy::PolicyMode;
use crate::provider_config::valid_id;
use crate::sandbox::SandboxChoice;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use ts_rs::TS;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct ProjectConfig {
    pub id: String,
    pub name: String,
    /// Absolute path of a folder that is the top level of a git repository.
    pub path: String,
    #[serde(default)]
    pub policy: PolicyMode,
    /// Which sandbox contains this project's commands. `auto` picks the strongest one available.
    #[serde(default)]
    pub sandbox: SandboxChoice,
    /// Whether commands may use the network. On by default, because builds need package registries.
    #[serde(default = "yes")]
    pub network: bool,
    /// Commands that run without asking under `auto_edit`. `*` matches any run of characters.
    #[serde(default)]
    pub allow_commands: Vec<String>,
    /// Extra environment variables for commands. Never put a secret here: the agent can read them.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Extra folders commands may read and execute from, such as a toolchain outside the system paths.
    #[serde(default)]
    pub sandbox_read: Vec<String>,
    /// Extra folders commands may write to, such as a shared package cache.
    #[serde(default)]
    pub sandbox_write: Vec<String>,
    /// The image for the container backend.
    #[serde(default)]
    pub container_image: Option<String>,
    /// Gitignored files and folders to copy from the project into each run's worktree.
    #[serde(default)]
    pub worktree_copy: Vec<String>,
    /// The entries of `worktree_copy` that hold credentials, such as `.env`. Each one is an explicit opt-in:
    /// the agent can read the copy, and its content can reach a model provider.
    #[serde(default)]
    pub worktree_copy_secrets: Vec<String>,
    /// A command to run in each new worktree before the agent starts, inside the sandbox.
    #[serde(default)]
    pub worktree_setup: Option<String>,
    /// The command that starts the project's dev server for Preview, run in a run's worktree inside the sandbox.
    /// `{port}` is replaced by the run's preview port, which is also in `PORT`.
    #[serde(default)]
    pub dev_command: Option<String>,
}

impl Default for ProjectConfig {
    /// The same defaults a file gets: the default policy, an automatic sandbox, and the network on.
    fn default() -> Self {
        ProjectConfig {
            id: String::new(),
            name: String::new(),
            path: String::new(),
            policy: PolicyMode::default(),
            sandbox: SandboxChoice::default(),
            network: true,
            allow_commands: vec![],
            env: BTreeMap::new(),
            sandbox_read: vec![],
            sandbox_write: vec![],
            container_image: None,
            worktree_copy: vec![],
            worktree_copy_secrets: vec![],
            worktree_setup: None,
            dev_command: None,
        }
    }
}

fn yes() -> bool {
    true
}

/// File names that usually hold credentials. Copying one into a worktree needs an explicit opt-in.
pub fn looks_like_credentials(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    name == ".env"
        || name.starts_with(".env.")
        || name.ends_with(".env")
        || [".pem", ".key", ".p12", ".pfx", ".keystore", ".jks"]
            .iter()
            .any(|ext| name.ends_with(ext))
        || [
            ".npmrc",
            ".netrc",
            ".pypirc",
            ".git-credentials",
            "id_rsa",
            "id_ed25519",
            "id_ecdsa",
            "id_dsa",
        ]
        .contains(&name.as_str())
        || name.contains("secret")
        || name.contains("credential")
}

impl ProjectConfig {
    /// A relative path with no `..`, no leading `/` and no empty or `.git` component.
    fn relative_ok(path: &str) -> bool {
        !path.is_empty() && !path.starts_with('/') && path.split('/').all(|c| !c.is_empty() && c != "." && c != ".." && c != ".git")
    }

    pub fn validate(&self) -> Result<(), String> {
        let id = &self.id;
        if !valid_id(id) {
            return Err(format!("invalid project id {id:?}: use letters, digits, - and _"));
        }
        if !std::path::Path::new(&self.path).is_absolute() {
            return Err(format!("project path {:?} must be absolute", self.path));
        }
        for name in self.env.keys() {
            let ok = !name.is_empty()
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !ok {
                return Err(format!("project {id}: {name:?} is not a valid environment variable name"));
            }
            if ["HOME", "TMPDIR"].contains(&name.as_str()) || name.starts_with("PATINA_") {
                return Err(format!("project {id}: {name} is set by Patina and cannot be changed"));
            }
        }
        for p in self.sandbox_read.iter().chain(&self.sandbox_write) {
            if !std::path::Path::new(p).is_absolute() || p.split('/').any(|c| c == "..") {
                return Err(format!("project {id}: sandbox path {p:?} must be absolute, without `..`"));
            }
        }
        for p in self.worktree_copy.iter().chain(&self.worktree_copy_secrets) {
            if !Self::relative_ok(p) {
                return Err(format!(
                    "project {id}: worktree_copy entry {p:?} must be a path inside the project, without `..`"
                ));
            }
        }
        if let Some(extra) = self.worktree_copy_secrets.iter().find(|p| !self.worktree_copy.contains(p)) {
            return Err(format!(
                "project {id}: {extra:?} is in worktree_copy_secrets but not in worktree_copy"
            ));
        }
        if let Some(risky) = self
            .worktree_copy
            .iter()
            .find(|p| looks_like_credentials(p) && !self.worktree_copy_secrets.contains(p))
        {
            return Err(format!(
                "project {id}: {risky:?} looks like a file with credentials. The agent can read a copied file and its content can reach a model provider. \
                 To copy it anyway, also list it in worktree_copy_secrets"
            ));
        }
        if self.sandbox == SandboxChoice::Container && self.container_image.as_deref().is_none_or(|i| i.trim().is_empty()) {
            return Err(format!("project {id}: the container sandbox needs container_image"));
        }
        if self.worktree_setup.as_deref().is_some_and(|c| c.trim().is_empty()) {
            return Err(format!("project {id}: worktree_setup is empty"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectsFile {
    pub schema_version: u32,
    #[serde(default, rename = "project")]
    pub projects: Vec<ProjectConfig>,
}

impl Default for ProjectsFile {
    fn default() -> Self {
        ProjectsFile {
            schema_version: 1,
            projects: vec![],
        }
    }
}

impl ProjectsFile {
    pub fn parse(text: &str) -> Result<ProjectsFile, String> {
        let f: ProjectsFile = toml::from_str(text).map_err(|e| format!("invalid projects.toml: {e}"))?;
        if f.schema_version > 1 {
            return Err(format!(
                "projects.toml schema_version {} is newer than this build supports",
                f.schema_version
            ));
        }
        f.validate()?;
        Ok(f)
    }

    /// The file holds only what differs from the defaults, so a simple project stays four lines long.
    /// The API, by contrast, always sends every setting.
    pub fn to_toml(&self) -> Result<String, String> {
        let mut doc = toml::Table::try_from(self).map_err(|e| e.to_string())?;
        let defaults = toml::Table::try_from(ProjectConfig::default()).map_err(|e| e.to_string())?;
        if let Some(toml::Value::Array(projects)) = doc.get_mut("project") {
            for project in projects.iter_mut().filter_map(toml::Value::as_table_mut) {
                project.retain(|key, value| matches!(key, "id" | "name" | "path" | "policy") || defaults.get(key) != Some(value));
            }
        }
        toml::to_string_pretty(&doc).map_err(|e| e.to_string())
    }

    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for p in &self.projects {
            p.validate()?;
            if !seen.insert(p.id.as_str()) {
                return Err(format!("duplicate project id {:?}", p.id));
            }
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&ProjectConfig> {
        self.projects.iter().find(|p| p.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_defaults_and_rejects_bad_entries() {
        let f = ProjectsFile::parse("schema_version = 1\n[[project]]\nid = \"site\"\nname = \"Site\"\npath = \"/srv/site\"\n").unwrap();
        assert_eq!(f.get("site").unwrap().policy, PolicyMode::AutoEdit);
        assert_eq!(ProjectsFile::parse(&f.to_toml().unwrap()).unwrap(), f);
        assert!(ProjectsFile::parse("schema_version = 1\n[[project]]\nid = \"a\"\nname = \"A\"\npath = \"relative\"\n").is_err());
        assert!(ProjectsFile::parse("schema_version = 1\n[[project]]\nid = \"a/b\"\nname = \"A\"\npath = \"/x\"\n").is_err());
        assert!(ProjectsFile::parse("schema_version = 1\n[[project]]\nid=\"a\"\nname=\"A\"\npath=\"/x\"\npolicy=\"yolo\"\n").is_err());
    }

    fn project(extra: &str) -> Result<ProjectsFile, String> {
        ProjectsFile::parse(&format!(
            "schema_version = 1\n[[project]]\nid = \"site\"\nname = \"Site\"\npath = \"/srv/site\"\n{extra}"
        ))
    }

    #[test]
    fn sandbox_settings_default_to_auto_with_network_and_stay_out_of_the_file() {
        let f = project("").unwrap();
        let p = f.get("site").unwrap();
        assert_eq!((p.sandbox, p.network, p.allow_commands.len()), (SandboxChoice::Auto, true, 0));
        // A project made in code gets the same defaults as one read from a file.
        assert_eq!(
            ProjectConfig {
                id: "site".into(),
                name: "Site".into(),
                path: "/srv/site".into(),
                ..Default::default()
            },
            *p
        );
        let text = f.to_toml().unwrap();
        for key in ["sandbox", "network", "allow_commands", "env", "worktree"] {
            assert!(!text.contains(key), "{key} should not be written when it is the default:\n{text}");
        }
        // The API sends every setting, so a client never has to know the defaults.
        let sent = serde_json::to_value(p).unwrap();
        assert_eq!(
            (&sent["sandbox"], &sent["network"], &sent["allow_commands"], &sent["worktree_setup"]),
            (
                &serde_json::json!("auto"),
                &true.into(),
                &serde_json::json!([]),
                &serde_json::Value::Null
            )
        );
        let full = project("sandbox = \"container\"\ncontainer_image = \"img\"\nnetwork = false\nallow_commands = [\"make *\"]\nworktree_setup = \"make deps\"\n[project.env]\nCI = \"1\"\n").unwrap();
        assert_eq!(ProjectsFile::parse(&full.to_toml().unwrap()).unwrap(), full);
        assert_eq!(full.get("site").unwrap().env["CI"], "1");
    }

    #[test]
    fn rejects_unsafe_sandbox_and_copy_settings() {
        for bad in [
            "[project.env]\nHOME = \"/root\"\n",
            "[project.env]\nPATINA_MASTER_KEY = \"x\"\n",
            "[project.env]\n\"A B\" = \"x\"\n",
            "sandbox_read = [\"relative\"]\n",
            "sandbox_write = [\"/a/../b\"]\n",
            "worktree_copy = [\"../outside\"]\n",
            "worktree_copy = [\"/etc/passwd\"]\n",
            "worktree_copy = [\".git/config\"]\n",
            "worktree_copy_secrets = [\".env\"]\n",
            "sandbox = \"container\"\n",
            "worktree_setup = \" \"\n",
        ] {
            assert!(project(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn copying_a_credentials_file_needs_an_explicit_opt_in() {
        for name in [
            ".env",
            ".env.local",
            "prod.env",
            "certs/server.pem",
            "deploy/id_rsa",
            ".npmrc",
            "config/secrets.toml",
            "aws/credentials",
        ] {
            assert!(looks_like_credentials(name), "{name}");
            let err = project(&format!("worktree_copy = [\"{name}\"]\n")).unwrap_err();
            assert!(err.contains("credentials") && err.contains("worktree_copy_secrets"), "{err}");
            project(&format!("worktree_copy = [\"{name}\"]\nworktree_copy_secrets = [\"{name}\"]\n")).unwrap();
        }
        for name in ["node_modules", "config/local.toml", "environment.md", "keyboard.txt"] {
            assert!(!looks_like_credentials(name), "{name}");
            project(&format!("worktree_copy = [\"{name}\"]\n")).unwrap();
        }
    }
}
