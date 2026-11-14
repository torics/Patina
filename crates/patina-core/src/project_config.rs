//! `projects.toml`: registered project folders and their policy.
use crate::policy::PolicyMode;
use crate::provider_config::valid_id;
use serde::{Deserialize, Serialize};
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectsFile {
    pub schema_version: u32,
    #[serde(default, rename = "project")]
    pub projects: Vec<ProjectConfig>,
}

impl Default for ProjectsFile {
    fn default() -> Self {
        ProjectsFile { schema_version: 1, projects: vec![] }
    }
}

impl ProjectsFile {
    pub fn parse(text: &str) -> Result<ProjectsFile, String> {
        let f: ProjectsFile = toml::from_str(text).map_err(|e| format!("invalid projects.toml: {e}"))?;
        if f.schema_version > 1 {
            return Err(format!("projects.toml schema_version {} is newer than this build supports", f.schema_version));
        }
        f.validate()?;
        Ok(f)
    }

    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| e.to_string())
    }

    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for p in &self.projects {
            if !valid_id(&p.id) {
                return Err(format!("invalid project id {:?}: use letters, digits, - and _", p.id));
            }
            if !std::path::Path::new(&p.path).is_absolute() {
                return Err(format!("project path {:?} must be absolute", p.path));
            }
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
}
