# Adding a provider

A provider is an endpoint that serves models: a cloud API, a router such as OpenRouter, or a model server on your network. Patina names no provider or model of its own; you add the ones you use.

## In Settings

1. **Add provider**, then **Start from a preset**: OpenAI, Anthropic, Google Gemini, OpenRouter, Groq, Ollama, or Custom OpenAI-compatible. A preset fills in the protocol and base URL; change either if you need to.
2. Give it a short name. Agents refer to it by this name.
3. Paste the API key, or leave it empty for an endpoint that needs none, such as a local Ollama. The key is stored encrypted in the database, is never shown again (only its last four characters), and never reaches a model, a command or a log.
4. Save, then **Test connection**. Patina sends a one-line request and reports the time to the first token, or the provider's own error.

Any endpoint that speaks the OpenAI chat completions API works as **Custom OpenAI-compatible** with its base URL, which usually ends in `/v1`: vLLM, llama.cpp's server, LM Studio, LiteLLM, or a hosted service that offers that API.

## A key from the environment

To keep a key out of the database, start the daemon with it in an environment variable and point the provider at it in `providers.toml`:

```toml
[[provider]]
id = "work-openai"
kind = "openai"
api_key_ref = "env:WORK_OPENAI_KEY"
```

Settings then shows "Key: from $WORK_OPENAI_KEY", and whether the variable is set.

## Models, limits and prices

When you make an agent, its model list comes from the provider when the provider can list them; otherwise type the model's name. Set the agent's `context_window` to what the model takes in, so long runs are compacted in time.

`max_concurrency` (default 4) caps the provider's requests at once across all runs, and `idle_timeout_s` (default 120) ends a stream that goes quiet. To see what runs cost, give each model a price in USD per million tokens:

```toml
[provider.prices."model-name"]
input = 3.0
output = 15.0
```

A run's cost limit holds only for models with a price. See [config.md](config.md) for every setting.
