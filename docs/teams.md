# Sharing a team

A team file holds a team and the agents it uses, in one TOML file. It names providers and models but holds no key, so it is safe to send to someone or keep in a repository.

## Export

In Settings, Teams, choose **Export** on the team. The file looks like this:

```toml
schema_version = 1

[team]
id = "review"
name = "Plan, build, review"
mode = "sequential"
members = ["planner", "builder", "reviewer"]

[[agent]]
id = "planner"
name = "Planner"
provider = "work-openai"
model = "the-model-you-chose"
tools = ["read_file", "list_files", "search"]
```

## Import

In Settings, Teams, choose **Import team** and pick the file. Patina shows each agent and its provider. Where the file names a provider that does not exist on this machine, choose one of yours for those agents; their model names stay as they are, so change them if your provider calls the model something else.

An import never overwrites: a team or agent whose id is already taken is refused, and nothing is created. Rename the team in the file, or remove the old one first.

## Where teams live

Each team is `teams/<id>.toml` and each agent `agents/<id>.toml` in the config folder (see [config.md](config.md)). Copying those files to another machine works too; Patina reads them on the next use.
