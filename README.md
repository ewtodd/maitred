# maitred

An OpenAI-compatible `/v1` front for self-hosted LLM servers, built for a box that has more models than GPU
memory. One binary, one JSON config.

Like a maître d', it checks your reservation (a virtual key) and seats you at the server that serves your
model. When that model isn't being served, because the GPUs are running a different set right now, it tells
you what is being served and how to get yours, instead of a connection error:

```json
{"error": {"type": "model_unavailable", "code": "model_not_loaded", "message":
  "Qwen3.8-27B (dense/qwen3.8-27b) is not loaded: gpu-box is serving DeepSeek-V4-Flash right now. Use
   DeepSeek-V4-Flash (big/dsv4-flash) instead, or ask the admin to switch gpu-box to the Qwen models."}}
```

## What it does

- **Routing.** A request's `model` names a configured model, either as `provider/name` or as a bare name
  that matches exactly one model. The body goes to that model's server untouched, except for the model name
  (rewritten to the server's own) and the reasoning effort (below). Any `POST /v1/...` path works: chat
  completions, completions, Responses, embeddings. Responses stream back as they arrive.
- **Virtual keys.** Each consumer gets its own key, read from an environment variable. A key is sent as
  `Authorization: Bearer`, `x-api-key` or `x-bf-vk`, the last for Bifrost compatibility. Per key:
  - an allowlist of models;
  - a fixed-window rate limit;
  - active hours in a time zone, with its own refusal message.
- **Model listing.** `GET /v1/models` lists every configured model, whether or not its server is up. Each
  entry carries:
  - `context_length` (also as `max_model_len`);
  - `reasoning_efforts`, weakest first, and `default_reasoning_effort`;
  - `reasoning_effort_aliases`;
  - a live `status` (`serving` or `unavailable`, the latter with a `status_reason`).

  Clients can read the window and the effort vocabulary from here instead of per-model configuration;
  [son-of-anton](https://github.com/ewtodd/son-of-anton) does. Without a key, or with an unknown one,
  everything is listed; with a valid key, only the models it may use.
- **Reasoning-effort translation.** Servers disagree on the effort vocabulary: a Qwen template might take
  none/low/medium/xhigh, while DeepSeek takes none/low/high/max. A level the model doesn't take is
  rewritten in this order:
  1. the model's explicit aliases;
  2. the nearest weaker level;
  3. the weakest level above.

  `none` is never the target for a level that asks for thinking. Chat's `reasoning_effort`, Responses'
  `reasoning.effort` and `chat_template_kwargs.reasoning_effort` are all handled.
- **Modes.** Upstreams on one host that exclude each other are grouped into modes, for example "one big
  model" and "two smaller ones". When a model's server is down, maitred probes the others to work out what
  to say:
  - the other mode is serving: use its models, or ask the admin to switch;
  - the wanted mode is partly up: still loading, with `Retry-After`;
  - nothing is up: switching or restarting, with `Retry-After`.
- **Picking the model (`auto`).** Give models a `tier` (higher is stronger), and one more id, `auto` unless
  set, leaves the choice to maitred. It picks among the tiered models whose servers answer and whose windows
  fit the conversation: the strongest with a free slot (`slots`), then the strongest busy one, with ties going
  to the one with fewer requests in flight. Moving a conversation makes the new model re-read all of it, so
  the choice is cache-aware:
  - a conversation is recognised by its opening: the key, the instructions, and everything up to the first
    user message;
  - inside a tool loop it stays on its model;
  - at a new user message, a conversation shorter than `move_below_tokens` takes the best model on offer,
    while a longer one stays where it is cached;
  - it moves anyway when its model stops answering or it outgrows the model's window.

  A scheduled job that asks for `auto` runs on whatever is up, with its effort translated for that model. It
  is not a fallback: a request that names a model still gets that model or the not-loaded error. A routed
  request whose server turns out to be down is routed once more. `/v1/models` lists `auto` first,
  with the largest window and every effort level among the routed models serving now, and `routes_to`
  naming them. `decision_log` records each decision as a JSON line. A Responses request with
  `previous_response_id` cannot be routed: the stored response lives on one server.
- **MCP gateway.** `/mcp` is one MCP endpoint (Streamable HTTP, JSON responses) in front of any number of
  stdio MCP servers:
  - each server is started on first use, kept running, and restarted if it exits;
  - a caller sees only the tools its key is granted, named `server-tool`. That is Bifrost's naming, so
    clients written against Bifrost's MCP gateway keep their tool names;
  - servers get a clean environment: none of the gateway's keys, only what the config passes.
- **Passthrough.** Listed path prefixes are forwarded verbatim, headers included, to another server.
- **Timeouts.** Each upstream has a connect timeout, a whole-request timeout, and an idle timeout between
  streamed chunks. A long cold prefill can be given an hour.

## Run it

```sh
nix run github:ewtodd/maitred -- --config config.json    # or: cargo build --release
```

The config path can also come from `MAITRED_CONFIG`. Secrets are never in the file: each key and each
upstream API key names an environment variable. [`examples/config.json`](examples/config.json) is a
complete example: one GPU box with two modes and three tiered models behind `auto`, an embedding server, a
hosted API, three keys, an MCP server and a passthrough.

`GET /health` answers `ok`.

## Configuration

| Field | Meaning |
| --- | --- |
| `listen` | `host:port` to listen on. |
| `upstreams.<name>.base_url` | The server's base URL including `/v1`. |
| `upstreams.<name>.api_key_env` | Variable holding the upstream's key, sent as `Authorization: Bearer`. Optional. |
| `upstreams.<name>.timeout_secs` | Whole-request timeout (default 1800). |
| `upstreams.<name>.idle_timeout_secs` | Longest silence between streamed chunks (default 300). |
| `upstreams.<name>.connect_timeout_secs` | Default 5. |
| `upstreams.<name>.slots` | Requests the server runs at once. `auto` counts its models as full while this many are in flight; a named request still goes there and queues. Unset: never full. |
| `models[].id` | The id clients use and `/v1/models` lists, conventionally `provider/name`. |
| `models[].upstream` | Key into `upstreams`. |
| `models[].upstream_model` | The name the server serves it under. |
| `models[].display_name` | Human name used in error messages. |
| `models[].hidden` | Omitted from the `/v1/models` listing; still resolvable and served by an explicit request. |
| `models[].tier` | Strength for `auto`, higher is stronger; models of one tier are interchangeable to it. Only tiered models are routed to. |
| `models[].context_length` | Listed as `context_length` and `max_model_len`. |
| `models[].max_completion_tokens` | Listed when set. |
| `models[].reasoning_efforts` | Levels the server takes, weakest first. Empty means not a reasoning model; the field passes through untouched. |
| `models[].default_reasoning_effort` | The level the server applies when none is sent. Listed only. |
| `models[].effort_aliases` | Explicit rewrites applied before clamping, e.g. `{"medium": "high"}`. |
| `models[].extra` | Extra fields merged into the model's `/v1/models` entry. |
| `models[].request_overrides` | Top-level fields set on every request for the model, replacing what the client sent. For example, a `response_format` that forces JSON; two ids can then serve one upstream model with different settings. |
| `modes[]` | `name`, `label` (used in messages), `host` (modes on one host exclude each other), `upstreams`. |
| `switch_hint` | What an unavailable-model error suggests about the mode; `{mode}` becomes the wanted mode's label. Default `ask the admin to switch to {mode}`. |
| `router.id` | The routed id (default `auto`), on when some model has a `tier`. Must not be a model id; keys name it like one. |
| `router.move_below_tokens` | A conversation estimated below this (default 16384) may move to another model at a new user message; a longer one stays where it is cached. |
| `router.forget_after_secs` | How long a quiet conversation's model is remembered (default 21600, six hours). |
| `router.decision_log` | A file each routing decision is appended to, one JSON object per line. Under the NixOS module, put it in `/var/lib/maitred`. |
| `router.log_prompts` | Also write the last user message's text into the decision log. Default false. |
| `keys[].name`, `keys[].env` | The key's name (in logs and messages) and the variable holding its value. |
| `keys[].models` | Model ids it may use; default `["*"]`. |
| `keys[].rate_limit` | `{requests, per_secs}`. |
| `keys[].active_hours` | `{start, end, timezone, message}`; hours 0–23, and `end < start` wraps past midnight. |
| `keys[].mcp` | MCP grants: server name to its tool names (unprefixed), `["*"]` for all. A key without grants gets no `/mcp`. |
| `mcp.<name>.command`, `.args` | A stdio MCP server; `<name>` is its tools' prefix. |
| `mcp.<name>.env` | Variables set for it. It also gets `PATH`, `HOME`, `LANG`, `TZ` and the CA-bundle variables, and nothing else. |
| `mcp.<name>.pass_env` | Variables passed through from the gateway's environment, e.g. an API key the server needs. |
| `mcp.<name>.timeout_secs` | Longest a tool call may take (default 300). |
| `passthrough[]` | `{prefix, target, timeout_secs}`; `target` is an origin such as `http://127.0.0.1:4012`, and the path is kept. |

The config is checked at startup. maitred refuses to start on:

- unknown upstreams;
- duplicate model ids;
- a router id that is also a model id;
- `slots` of 0;
- aliases or defaults outside the model's efforts;
- key model lists naming unknown models;
- MCP grants naming unknown servers;
- bad time zones.

## Errors

Every error has the OpenAI shape: `{"error": {"message", "type", "code"}}`.

| Status | `code` | When |
| --- | --- | --- |
| 400 | `invalid_json`, `missing_model` | The body isn't JSON or has no `model`. |
| 400 | `context_length_exceeded`, `unsupported_parameter` | A routed conversation longer than every tiered model's window, or a routed Responses request with `previous_response_id`. |
| 401 | `missing_api_key`, `invalid_api_key` | No key, or an unknown one. |
| 403 | `key_inactive`, `model_not_allowed`, `mcp_not_allowed` | Outside the key's hours (with its message), a model it may not use (the message lists what it can), or `/mcp` for a key with no MCP grants. |
| 404 | `model_not_found` | Unknown model (the message lists the configured ones). |
| 429 | `rate_limit_exceeded` | With `Retry-After`. |
| 502 | `upstream_error` | The upstream failed mid-request. |
| 503 | `model_not_loaded` (type `model_unavailable`) | The model's server isn't up, or no routed model that fits the conversation is. The message says what to use instead. `Retry-After` is set only when waiting will help. |
| 504 | `upstream_timeout` | The upstream timed out. |

A client should treat a `model_not_loaded` without `Retry-After` as "fall back or tell the user", not as an
overload to retry.

## NixOS

```nix
inputs.maitred.url = "github:ewtodd/maitred";

# in a host's modules:
imports = [ inputs.maitred.nixosModules.default ];
services.maitred = {
  enable = true;
  openFirewall = true;
  environmentFiles = [ config.age.secrets.maitred-keys.path ];   # MAITRED_KEY_ME=..., ...
  settings = builtins.fromJSON (builtins.readFile ./maitred.json);  # or write it as Nix
};
```

The service runs as a `DynamicUser` with systemd hardening. Its package is built with the host's nixpkgs.

## Development

```sh
nix develop -c cargo test
```

## License

MIT.
