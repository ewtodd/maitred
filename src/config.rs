//! The gateway's configuration: a JSON file (written by the NixOS module) plus secrets from the environment.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `host:port` to listen on.
    pub listen: String,
    /// Inference servers, by name (the `provider` part of a model id).
    pub upstreams: HashMap<String, Upstream>,
    /// Every model the gateway offers. Listed in `/v1/models` whether or not its server is up.
    pub models: Vec<Model>,
    /// Groups of upstreams that run together and exclude each other (son-of-anton's `llm-mode`).
    #[serde(default)]
    pub modes: Vec<Mode>,
    /// What an unavailable-model error tells the caller to do about the mode, e.g.
    /// "ask the admin to switch son-of-anton to {mode}". `{mode}` is the wanted mode's label.
    #[serde(default = "default_switch_hint")]
    pub switch_hint: String,
    pub keys: Vec<Key>,
    /// Paths forwarded verbatim (headers included) to another server, e.g. `/mcp` to Bifrost.
    #[serde(default)]
    pub passthrough: Vec<Passthrough>,
    /// stdio MCP servers aggregated at `/mcp`, by name. A tool `t` of server `s` is offered as `s-t`.
    #[serde(default)]
    pub mcp: HashMap<String, McpServer>,
}

fn default_switch_hint() -> String {
    "ask the admin to switch to {mode}".to_string()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    /// The program, started once and kept running; restarted on the next call after it exits.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment for the server, on top of `PATH`, `HOME`, `LANG`, `TZ` and the CA-bundle variables. Nothing
    /// else of the gateway's environment (its keys) is passed.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Variables passed through from the gateway's environment, e.g. an API key the server needs.
    #[serde(default)]
    pub pass_env: Vec<String>,
    /// Longest a `tools/call` may take.
    #[serde(default = "default_tool_timeout")]
    pub timeout_secs: u64,
}

fn default_tool_timeout() -> u64 {
    300
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    /// Base URL including `/v1`, e.g. `http://10.0.0.5:8200/v1`.
    pub base_url: String,
    /// Environment variable holding the upstream's API key (sent as `Authorization: Bearer`).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Whole-request timeout. A cold 1M-token prefill takes ~20 min on unholy.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Longest silence allowed between two chunks of a streamed response.
    #[serde(default = "default_idle")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_connect")]
    pub connect_timeout_secs: u64,
}

fn default_timeout() -> u64 {
    1800
}
fn default_idle() -> u64 {
    300
}
fn default_connect() -> u64 {
    5
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    /// The id clients use and `/v1/models` lists, `provider/name` (Bifrost's convention).
    pub id: String,
    /// Key into `upstreams`.
    pub upstream: String,
    /// The name the upstream serves it under (its `--served-model-name`).
    pub upstream_model: String,
    /// Human name for error messages, e.g. "DeepSeek-V4-Flash".
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub max_completion_tokens: Option<u64>,
    /// The `reasoning_effort` values the server accepts, weakest first. Empty: not a reasoning model
    /// (the field is passed through untouched).
    #[serde(default)]
    pub reasoning_efforts: Vec<String>,
    #[serde(default)]
    pub default_reasoning_effort: Option<String>,
    /// Explicit rewrites applied before clamping, e.g. `{"medium": "high"}`.
    #[serde(default)]
    pub effort_aliases: HashMap<String, String>,
    /// Extra fields merged into this model's `/v1/models` entry (capabilities, modalities, ...).
    #[serde(default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Model {
    pub fn display(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.id)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mode {
    pub name: String,
    /// Human name, e.g. "DeepSeek-V4-Flash" or "the Qwen models".
    pub label: String,
    /// The machine whose GPUs the modes share, e.g. "son-of-anton". Modes on the same host exclude each other.
    #[serde(default = "default_host")]
    pub host: String,
    pub upstreams: Vec<String>,
}

fn default_host() -> String {
    "the server".to_string()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Key {
    pub name: String,
    /// Environment variable holding the key's secret value.
    pub env: String,
    /// Model ids this key may use; `["*"]` for all.
    #[serde(default = "all_models")]
    pub models: Vec<String>,
    #[serde(default)]
    pub rate_limit: Option<RateLimit>,
    #[serde(default)]
    pub active_hours: Option<ActiveHours>,
    /// MCP tools this key may use: server name -> tool names (unprefixed), `["*"]` for all of them. A key
    /// without grants gets no `/mcp`.
    #[serde(default)]
    pub mcp: HashMap<String, Vec<String>>,
}

fn all_models() -> Vec<String> {
    vec!["*".to_string()]
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    pub requests: u32,
    pub per_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveHours {
    /// Hour the key turns on (0-23), local to `timezone`.
    pub start: u32,
    /// Hour it turns off; smaller than `start` wraps past midnight.
    pub end: u32,
    pub timezone: String,
    /// What a refusal outside the window says.
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Passthrough {
    pub prefix: String,
    /// Origin to forward to, e.g. `http://127.0.0.1:4012`; the request path is kept.
    pub target: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let config: Config =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        let mut ids = std::collections::HashSet::new();
        for m in &self.models {
            if !self.upstreams.contains_key(&m.upstream) {
                return Err(format!("model {}: unknown upstream {}", m.id, m.upstream));
            }
            if !ids.insert(m.id.as_str()) {
                return Err(format!("model {} listed twice", m.id));
            }
            for (from, to) in &m.effort_aliases {
                if !m.reasoning_efforts.contains(to) {
                    return Err(format!(
                        "model {}: effort alias {from} -> {to}, which it does not take",
                        m.id
                    ));
                }
            }
            if let Some(d) = &m.default_reasoning_effort
                && !m.reasoning_efforts.contains(d)
            {
                return Err(format!(
                    "model {}: default effort {d} is not one it takes",
                    m.id
                ));
            }
        }
        for mode in &self.modes {
            for u in &mode.upstreams {
                if !self.upstreams.contains_key(u) {
                    return Err(format!("mode {}: unknown upstream {u}", mode.name));
                }
            }
        }
        for k in &self.keys {
            for m in &k.models {
                if m != "*" && !ids.contains(m.as_str()) {
                    return Err(format!("key {}: unknown model {m}", k.name));
                }
            }
            for server in k.mcp.keys() {
                if !self.mcp.contains_key(server) {
                    return Err(format!("key {}: unknown MCP server {server}", k.name));
                }
            }
            if let Some(h) = &k.active_hours {
                if h.start > 23 || h.end > 23 {
                    return Err(format!("key {}: active hours must be 0-23", k.name));
                }
                h.timezone
                    .parse::<chrono_tz::Tz>()
                    .map_err(|_| format!("key {}: unknown time zone {}", k.name, h.timezone))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_config_loads_and_validates() {
        let config: Config = serde_json::from_str(include_str!("../examples/config.json")).unwrap();
        config.validate().unwrap();
        // Every key's model list and every mode's upstreams resolve (validate checks), and each mode
        // has at least one model to suggest.
        for mode in &config.modes {
            assert!(
                config
                    .models
                    .iter()
                    .any(|m| mode.upstreams.contains(&m.upstream)),
                "mode {} serves no model",
                mode.name
            );
        }
    }
}
