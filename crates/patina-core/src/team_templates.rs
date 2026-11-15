//! The built-in team templates. They name no provider or model: the owner picks those when creating a team.
use crate::agent_config::AgentConfig;
use crate::api::{TemplateAgentView, TemplateInput, TemplateView};
use crate::provider_config::valid_id;
use crate::team_config::{TeamConfig, TeamMode};

const READ: [&str; 3] = ["list_files", "read_file", "search"];
const ALL: [&str; 6] = ["list_files", "read_file", "search", "edit_file", "write_file", "run_command"];

struct AgentTemplate {
    id: &'static str,
    name: &'static str,
    role: &'static str,
    tools: &'static [&'static str],
    system_prompt: &'static str,
}

struct Template {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    mode: TeamMode,
    members: &'static [&'static str],
    synthesizer: Option<&'static str>,
    distinct_models: &'static [&'static str],
    agents: &'static [AgentTemplate],
}

const BUILDER: AgentTemplate = AgentTemplate {
    id: "builder",
    name: "Builder",
    role: "Implements the change and makes the build and tests pass",
    tools: &ALL,
    system_prompt: "You implement changes in this project. Read the relevant code before you change it, keep \
changes small and in the project's style, and run the project's build and tests when you can. Finish with a short \
summary of what you changed and anything left open.",
};

const TEMPLATES: [Template; 3] = [
    Template {
        id: "solo-builder",
        name: "Solo builder",
        description: "One agent that reads, edits and runs commands.",
        mode: TeamMode::Single,
        members: &["builder"],
        synthesizer: None,
        distinct_models: &[],
        agents: &[BUILDER],
    },
    Template {
        id: "plan-build-review",
        name: "Plan, build, review",
        description: "A planner writes a plan without changing files, a builder carries it out, and a reviewer checks the result.",
        mode: TeamMode::Sequential,
        members: &["planner", "builder", "reviewer"],
        synthesizer: None,
        distinct_models: &[],
        agents: &[
            AgentTemplate {
                id: "planner",
                name: "Planner",
                role: "Reads the code and writes a step-by-step plan",
                tools: &READ,
                system_prompt: "You plan changes; you do not make them. Read what you need, then write a short, \
numbered plan that names the files to change and how, and the checks that will show the change works. Another agent \
will carry out your plan and sees only your final message.",
            },
            BUILDER,
            AgentTemplate {
                id: "reviewer",
                name: "Reviewer",
                role: "Reviews the change and runs the checks",
                tools: &["list_files", "read_file", "search", "run_command"],
                system_prompt: "You review a change another agent just made. Read the changed files and run the \
project's tests. Report what is wrong or missing, most important first, and say plainly when the change is good. \
Do not change files.",
            },
        ],
    },
    Template {
        id: "compare-models",
        name: "Compare models",
        description: "Two agents on different models answer the same request without changing files, then a judge compares their answers.",
        mode: TeamMode::Parallel,
        members: &["model-a", "model-b"],
        synthesizer: Some("judge"),
        distinct_models: &["model-a", "model-b"],
        agents: &[
            AgentTemplate {
                id: "model-a",
                name: "Model A",
                role: "Answers the request on the first model",
                tools: &READ,
                system_prompt: "Answer the owner's request as well as you can. You can read the project but not \
change it. End with your complete answer.",
            },
            AgentTemplate {
                id: "model-b",
                name: "Model B",
                role: "Answers the request on the second model",
                tools: &READ,
                system_prompt: "Answer the owner's request as well as you can. You can read the project but not \
change it. End with your complete answer.",
            },
            AgentTemplate {
                id: "judge",
                name: "Judge",
                role: "Compares the answers and picks or merges the best",
                tools: &READ,
                system_prompt: "You receive several answers to the same request. Check them against the project \
where you can, say where they differ and which is right, and give the owner one final answer that keeps the best of \
each.",
            },
        ],
    },
];

pub fn list() -> Vec<TemplateView> {
    TEMPLATES
        .iter()
        .map(|t| TemplateView {
            id: t.id.into(),
            name: t.name.into(),
            description: t.description.into(),
            mode: t.mode,
            agents: t
                .agents
                .iter()
                .map(|a| TemplateAgentView {
                    id: a.id.into(),
                    name: a.name.into(),
                    role: a.role.into(),
                    tools: a.tools.iter().map(|t| t.to_string()).collect(),
                })
                .collect(),
            distinct_models: t.distinct_models.iter().map(|s| s.to_string()).collect(),
        })
        .collect()
}

/// The team and agents a template makes with the owner's choices. Agent ids are `<team id>-<template agent id>`,
/// so the same template can be used for several teams.
pub fn instantiate(template_id: &str, input: &TemplateInput) -> Result<(TeamConfig, Vec<AgentConfig>), String> {
    let t = TEMPLATES
        .iter()
        .find(|t| t.id == template_id)
        .ok_or_else(|| format!("there is no template {template_id:?}"))?;
    let team_id = input.team_id.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(t.id);
    if !valid_id(team_id) || team_id.len() > 40 {
        return Err(format!("invalid team id {team_id:?}: use up to 40 letters, digits, - and _"));
    }
    if let Some(unknown) = input.models.keys().find(|k| !t.agents.iter().any(|a| a.id == k.as_str())) {
        return Err(format!("the template has no agent {unknown:?}"));
    }
    let id = |a: &str| format!("{team_id}-{a}");
    let agents: Vec<AgentConfig> = t
        .agents
        .iter()
        .map(|a| {
            let choice = input.models.get(a.id);
            AgentConfig {
                schema_version: 1,
                id: id(a.id),
                name: a.name.into(),
                role: a.role.into(),
                provider: choice.map_or(&input.provider, |c| &c.provider).trim().to_string(),
                model: choice.map_or(&input.model, |c| &c.model).trim().to_string(),
                system_prompt: a.system_prompt.into(),
                temperature: None,
                max_output_tokens: None,
                context_window: 128_000,
                tools: a.tools.iter().map(|t| t.to_string()).collect(),
                can_delegate_to: vec![],
            }
        })
        .collect();
    for a in &agents {
        a.validate()?;
    }
    let team = TeamConfig {
        schema_version: 1,
        id: team_id.into(),
        name: t.name.into(),
        mode: t.mode,
        members: t.members.iter().map(|m| id(m)).collect(),
        synthesizer: t.synthesizer.map(id).unwrap_or_default(),
    };
    team.validate()?;
    Ok((team, agents))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ModelChoice;

    fn input() -> TemplateInput {
        TemplateInput {
            team_id: None,
            provider: "local".into(),
            model: "m1".into(),
            models: Default::default(),
            overwrite: false,
        }
    }

    #[test]
    fn the_brief_names_three_templates_and_every_one_makes_a_valid_team() {
        let all = list();
        let modes: Vec<_> = all.iter().map(|t| (t.id.as_str(), t.mode)).collect();
        assert_eq!(
            modes,
            [
                ("solo-builder", TeamMode::Single),
                ("plan-build-review", TeamMode::Sequential),
                ("compare-models", TeamMode::Parallel)
            ]
        );
        for t in &all {
            let (team, agents) = instantiate(&t.id, &input()).unwrap();
            for a in team.agents() {
                assert!(agents.iter().any(|x| x.id == a), "{a} has an agent");
            }
            assert!(agents.iter().all(|a| a.provider == "local" && a.model == "m1"));
        }
    }

    #[test]
    fn the_owner_picks_models_per_agent_and_the_team_id() {
        let mut i = input();
        i.team_id = Some("cmp".into());
        i.models.insert(
            "model-b".into(),
            ModelChoice {
                provider: "cloud".into(),
                model: "m2".into(),
            },
        );
        let (team, agents) = instantiate("compare-models", &i).unwrap();
        assert_eq!(team.id, "cmp");
        assert_eq!(team.members, ["cmp-model-a", "cmp-model-b"]);
        assert_eq!(team.synthesizer, "cmp-judge");
        let b = agents.iter().find(|a| a.id == "cmp-model-b").unwrap();
        assert_eq!((b.provider.as_str(), b.model.as_str()), ("cloud", "m2"));
        // Members of a parallel team are read-only, so the template gives them only read tools.
        assert!(agents.iter().all(|a| a.tools.iter().all(|t| READ.contains(&t.as_str()))));

        i.models.insert(
            "nobody".into(),
            ModelChoice {
                provider: "x".into(),
                model: "y".into(),
            },
        );
        assert!(instantiate("compare-models", &i).unwrap_err().contains("no agent"));
        assert!(instantiate("nope", &input()).is_err());
        let mut bad = input();
        bad.team_id = Some("../x".into());
        assert!(instantiate("solo-builder", &bad).is_err());
        let mut no_model = input();
        no_model.model = " ".into();
        assert!(instantiate("solo-builder", &no_model).is_err());
    }
}
