# llm-gateway

An OpenAI-compatible `/v1` front for the house's LLM servers (unholy, Radiance and gufo on son-of-anton,
llama-swap and the embedding server on oracle, the DeepSeek API). One binary, one JSON config.

- **Routing:** `model` names a configured model, as `provider/name` or as a bare name that matches exactly
  one. The request body passes through untouched except for the model name and the reasoning effort.
- **Keys:** virtual keys come from environment variables, sent as `Authorization: Bearer`, `x-bf-vk` or
  `x-api-key`. Each key can have:
  - an allowlist of models;
  - a fixed-window rate limit;
  - active hours in a time zone, with its own refusal message.
- **`GET /v1/models`:** lists every configured model whether or not its server is up. Each entry carries
  `context_length` (also as `max_model_len`), `reasoning_efforts` (the levels the server takes, weakest first),
  `default_reasoning_effort`, `reasoning_effort_aliases` (the explicit rewrites, e.g. `medium` -> `high`), and a
  live `status` (`serving` / `unavailable` + `status_reason`). Clients such
  as son-of-anton read the window and the effort vocabulary from here instead of per-model config.
- **Reasoning effort:**
  - a level the model doesn't take is rewritten (Chat `reasoning_effort`, Responses `reasoning.effort`,
    `chat_template_kwargs.reasoning_effort`);
  - the model's explicit aliases come first, then the nearest weaker level, then the weakest level above it;
  - `none` is never a clamp target for a level that asks for thinking (`minimal` on none/low/... becomes `low`).
- **Modes:** upstreams on one host that exclude each other (son-of-anton's `llm-mode`) are grouped into
  modes. A request for a model whose server is down gets a 503 (`type: model_unavailable`,
  `code: model_not_loaded`) that names what is serving and what to use instead. It adds `Retry-After` when
  waiting will help (a switch in progress).
- **Passthrough:** listed path prefixes (e.g. `/mcp`) are forwarded verbatim, headers included, to another
  server.
- **Streaming:** responses stream back as they arrive. Each upstream has a whole-request timeout and an idle
  timeout between chunks.

```sh
nix build                      # or: nix develop -c cargo build --release
llm-gateway --config gateway.json
```

The NixOS module that writes the config lives in the host repo (`/etc/nixos`, `modules/services/llm-gateway`).
