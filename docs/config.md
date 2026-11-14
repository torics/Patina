# Config reference

Files live in the config folder (`~/.config/patina/` on Linux, or `--config-dir`). They are plain TOML, edited by hand or through the UI; the daemon reloads them when they change. Every file carries `schema_version = 1`.

## config.toml

| Key | Default | Meaning |
| --- | --- | --- |
| `bind` | `"127.0.0.1"` | Listen address |
| `port` | `7878` | App port |
| `preview_port` | `7879` | Preview proxy port (used from M6) |
| `allowed_hosts` | `["localhost", "127.0.0.1", "[::1]"]` | Accepted `Host` names |
| `retention_days` | `0` | Prune runs older than this; 0 keeps all (used from M4) |

## providers.toml

```toml
schema_version = 1

[[provider]]
id = "home-ollama"
kind = "openai_compatible"   # openai_compatible | openai | anthropic | gemini | ollama
base_url = "http://192.168.1.20:11434/v1"
api_key_ref = "secret:home-ollama"   # or "env:SOME_VAR", or omitted
max_concurrency = 4
idle_timeout_s = 120

[provider.prices."some-model-id"]   # optional, USD per million tokens
input = 0.0
output = 0.0
```

A `secret:` reference points at a key stored encrypted in the database; set it in the UI. Cost is shown only for models with a price.

## agents/<id>.toml

```toml
schema_version = 1
id = "assistant"
name = "Assistant"
provider = "home-ollama"
model = "some-model-id"
system_prompt = ""
temperature = 0.2          # optional
max_output_tokens = 8000   # optional
context_window = 128000
tools = []
can_delegate_to = []
```
