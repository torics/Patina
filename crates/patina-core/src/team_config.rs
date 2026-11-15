//! `teams/*.toml`: one team per file, and the bundle format that carries a team with its agents.
use crate::agent_config::AgentConfig;
use crate::provider_config::valid_id;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum TeamMode {
    /// One agent runs the loop.
    #[default]
    Single,
    /// The first member leads and hands tasks to the others with a `delegate` tool.
    Delegate,
    /// Members run in order; each gets the prompt and the final messages of the ones before it.
    Sequential,
    /// Members run at once, read-only, on the same prompt; then the synthesizer gets every final message.
    Parallel,
}

impl TeamMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TeamMode::Single => "single",
            TeamMode::Delegate => "delegate",
            TeamMode::Sequential => "sequential",
            TeamMode::Parallel => "parallel",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct TeamConfig {
    #[serde(default = "one")]
    pub schema_version: u32,
    pub id: String,
    /// Shown in the UI. Defaults to the id.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub mode: TeamMode,
    /// Agent ids. In `delegate` mode the first one leads.
    pub members: Vec<String>,
    /// The agent that combines the members' answers. Required in `parallel` mode and empty otherwise.
    #[serde(default)]
    pub synthesizer: String,
}

fn one() -> u32 {
    1
}

impl TeamConfig {
    pub fn parse(text: &str) -> Result<TeamConfig, String> {
        let t: TeamConfig = toml::from_str(text).map_err(|e| format!("invalid team file: {e}"))?;
        t.validate()?;
        Ok(t)
    }

    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| e.to_string())
    }

    pub fn display_name(&self) -> &str {
        if self.name.trim().is_empty() { &self.id } else { &self.name }
    }

    /// Every agent the team names: members, then the synthesizer.
    pub fn agents(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.members.iter().map(String::as_str).collect();
        if !self.synthesizer.is_empty() {
            out.push(&self.synthesizer);
        }
        out
    }

    /// The rules a team must meet on its own. Whether its agents exist is checked where agents are known.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err(format!("unsupported team schema_version {}", self.schema_version));
        }
        if !valid_id(&self.id) {
            return Err(format!("invalid team id {:?}: use letters, digits, - and _", self.id));
        }
        if self.members.is_empty() {
            return Err("a team needs at least one member".into());
        }
        for (i, m) in self.members.iter().enumerate() {
            if !valid_id(m) {
                return Err(format!("invalid member {m:?}: use agent ids"));
            }
            if self.members[..i].contains(m) {
                return Err(format!("{m:?} is a member twice"));
            }
        }
        match self.mode {
            TeamMode::Single if self.members.len() != 1 => {
                return Err("a team in single mode has exactly one member".into());
            }
            TeamMode::Parallel => {
                if self.synthesizer.is_empty() {
                    return Err("a team in parallel mode needs a synthesizer".into());
                }
                if !valid_id(&self.synthesizer) {
                    return Err(format!("invalid synthesizer {:?}: use an agent id", self.synthesizer));
                }
                if self.members.contains(&self.synthesizer) {
                    return Err("the synthesizer cannot also be a member: members cannot change files, the synthesizer can".into());
                }
            }
            _ if !self.synthesizer.is_empty() => {
                return Err("only a team in parallel mode has a synthesizer".into());
            }
            _ => {}
        }
        Ok(())
    }
}

/// A team with the agents it uses, as one file for export and import. It names providers by id and never
/// carries a secret.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamBundle {
    #[serde(default = "one")]
    pub schema_version: u32,
    pub team: TeamConfig,
    #[serde(default, rename = "agent")]
    pub agents: Vec<AgentConfig>,
}

impl TeamBundle {
    pub fn parse(text: &str) -> Result<TeamBundle, String> {
        let b: TeamBundle = toml::from_str(text).map_err(|e| format!("invalid team bundle: {e}"))?;
        if b.schema_version != 1 {
            return Err(format!("unsupported bundle schema_version {}", b.schema_version));
        }
        b.team.validate()?;
        for (i, a) in b.agents.iter().enumerate() {
            a.validate()?;
            if b.agents[..i].iter().any(|o| o.id == a.id) {
                return Err(format!("agent {:?} appears twice in the bundle", a.id));
            }
        }
        Ok(b)
    }

    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRIEF: &str = r#"
schema_version = 1
id = "plan-build-review"
mode = "sequential"   # single | delegate | sequential | parallel
members = ["planner", "builder", "reviewer"]
synthesizer = ""      # required when mode is parallel
"#;

    #[test]
    fn parses_the_brief_example_and_round_trips() {
        let t = TeamConfig::parse(BRIEF).unwrap();
        assert_eq!(t.mode, TeamMode::Sequential);
        assert_eq!(t.display_name(), "plan-build-review");
        assert_eq!(TeamConfig::parse(&t.to_toml().unwrap()).unwrap(), t);
    }

    #[test]
    fn rejects_teams_that_cannot_run() {
        for (text, why) in [
            (BRIEF.replace("\"plan-build-review\"", "\"../x\""), "invalid team id"),
            (BRIEF.replace(r#"["planner", "builder", "reviewer"]"#, "[]"), "at least one member"),
            (BRIEF.replace("\"reviewer\"]", "\"planner\"]"), "member twice"),
            (BRIEF.replace("\"sequential\"", "\"single\""), "exactly one member"),
            (BRIEF.replace("\"sequential\"", "\"parallel\""), "needs a synthesizer"),
            (
                BRIEF
                    .replace("\"sequential\"", "\"parallel\"")
                    .replace("synthesizer = \"\"", "synthesizer = \"builder\""),
                "cannot also be a member",
            ),
            (
                BRIEF.replace("synthesizer = \"\"", "synthesizer = \"judge\""),
                "only a team in parallel",
            ),
            (BRIEF.replace("\"sequential\"", "\"round_robin\""), "invalid team file"),
        ] {
            let err = TeamConfig::parse(&text).unwrap_err();
            assert!(err.contains(why), "{err}");
        }
    }

    #[test]
    fn a_bundle_carries_the_team_and_its_agents() {
        let text = r#"
schema_version = 1
[team]
id = "pair"
mode = "delegate"
members = ["lead", "helper"]

[[agent]]
id = "lead"
name = "Lead"
provider = "local"
model = "m1"
can_delegate_to = ["helper"]

[[agent]]
id = "helper"
name = "Helper"
provider = "local"
model = "m2"
"#;
        let b = TeamBundle::parse(text).unwrap();
        assert_eq!(b.agents.len(), 2);
        assert_eq!(b.team.agents(), ["lead", "helper"]);
        assert_eq!(TeamBundle::parse(&b.to_toml().unwrap()).unwrap(), b);
        let twice = text.replace("id = \"helper\"\nname", "id = \"lead\"\nname");
        assert!(TeamBundle::parse(&twice).unwrap_err().contains("twice"));
    }
}
