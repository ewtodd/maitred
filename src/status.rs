//! Which upstreams are answering, and what to tell a caller whose model is not being served.

use crate::config::{Config, Mode, Model};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const PROBE_TTL: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

pub struct Probes {
    client: reqwest::Client,
    /// Upstream API keys by base URL: a hosted API answers a keyless `/models` with 401.
    keys: HashMap<String, String>,
    cache: Mutex<HashMap<String, (Instant, bool)>>,
}

impl Probes {
    pub fn new(keys: HashMap<String, String>) -> Probes {
        let client = reqwest::Client::builder()
            .connect_timeout(PROBE_TIMEOUT)
            .timeout(PROBE_TIMEOUT)
            .build()
            .expect("probe client");
        Probes {
            client,
            keys,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Whether `GET {base_url}/models` answers, cached for a few seconds.
    pub async fn is_up(&self, base_url: &str) -> bool {
        if let Some((at, up)) = self.cache.lock().unwrap().get(base_url)
            && at.elapsed() < PROBE_TTL
        {
            return *up;
        }
        let url = format!("{}/models", base_url.trim_end_matches('/'));
        let mut req = self.client.get(&url);
        if let Some(key) = self.keys.get(base_url) {
            req = req.bearer_auth(key);
        }
        let up = matches!(req.send().await, Ok(r) if r.status().is_success());
        self.cache
            .lock()
            .unwrap()
            .insert(base_url.to_string(), (Instant::now(), up));
        up
    }

    /// Forget a cached result (after a request to that upstream failed).
    pub fn invalidate(&self, base_url: &str) {
        self.cache.lock().unwrap().remove(base_url);
    }

    /// Probe every upstream at once.
    pub async fn snapshot(&self, config: &Config) -> HashMap<String, bool> {
        let probes = config
            .upstreams
            .iter()
            .map(|(name, u)| async move { (name.clone(), self.is_up(&u.base_url).await) });
        futures_util::future::join_all(probes)
            .await
            .into_iter()
            .collect()
    }
}

/// Why a model is not available and what the caller can do, plus a `Retry-After` when waiting helps.
#[derive(Debug, PartialEq)]
pub struct Unavailable {
    pub message: String,
    pub retry_after: Option<u64>,
}

/// Compose the message for `model`, whose upstream is not answering, given which upstreams are (`up`).
pub fn explain(config: &Config, model: &Model, up: &HashMap<String, bool>) -> Unavailable {
    let is_up = |u: &str| up.get(u).copied().unwrap_or(false);
    let wanted = config
        .modes
        .iter()
        .find(|m| m.upstreams.contains(&model.upstream));
    let Some(wanted) = wanted else {
        return Unavailable {
            message: format!(
                "{} ({}) is not answering right now; try again later.",
                model.display(),
                model.id
            ),
            retry_after: Some(60),
        };
    };
    let host = &wanted.host;
    if wanted.upstreams.iter().any(|u| is_up(u)) {
        return Unavailable {
            message: format!(
                "{} ({}) is still loading on {host} (it was just switched or restarted); try again in a minute or two.",
                model.display(),
                model.id
            ),
            retry_after: Some(60),
        };
    }
    let serving: Vec<_> = config
        .modes
        .iter()
        .filter(|m| {
            m.name != wanted.name && m.host == wanted.host && m.upstreams.iter().any(|u| is_up(u))
        })
        .collect();
    if serving.is_empty() {
        return Unavailable {
            message: format!(
                "{} ({}) is not available: {host} is not serving any model right now (it may be switching or restarting). \
                 Try again in a few minutes, or {}.",
                model.display(),
                model.id,
                hint(config, &wanted.label)
            ),
            retry_after: Some(120),
        };
    }
    let other = serving[0];
    let instead: Vec<String> = config
        .models
        .iter()
        .filter(|m| other.upstreams.contains(&m.upstream) && is_up(&m.upstream))
        .map(|m| format!("{} ({})", m.display(), m.id))
        .collect();
    Unavailable {
        message: format!(
            "{} ({}) is not loaded: {host} is serving {} right now. Use {} instead, or {}.",
            model.display(),
            model.id,
            other.label,
            or_list(&instead),
            hint(config, &wanted.label)
        ),
        retry_after: None,
    }
}

/// The model `default_id` names right now: the `default_model` of the mode being served. Not a fallback: a
/// request for a specific model never lands here; this id is defined as whatever is loaded. Modes are tried in
/// config order, so with two hosts the first one serving wins.
pub fn serving_default<'a>(
    config: &'a Config,
    up: &HashMap<String, bool>,
) -> Result<&'a Model, Unavailable> {
    let is_up = |u: &str| up.get(u).copied().unwrap_or(false);
    let defaults: Vec<(&Mode, &Model)> = config
        .modes
        .iter()
        .filter_map(|mode| {
            let id = mode.default_model.as_ref()?;
            config
                .models
                .iter()
                .find(|m| &m.id == id)
                .map(|m| (mode, m))
        })
        .collect();
    if let Some((_, m)) = defaults.iter().find(|(_, m)| is_up(&m.upstream)) {
        return Ok(m);
    }
    // A mode whose other servers answer but whose default does not yet is still coming up.
    if let Some((mode, m)) = defaults
        .iter()
        .find(|(mode, _)| mode.upstreams.iter().any(|u| is_up(u)))
    {
        return Err(Unavailable {
            message: format!(
                "{} is {} ({}), which is still loading on {} (it was just switched or restarted); \
                 try again in a minute or two.",
                config.default_id,
                m.display(),
                m.id,
                mode.host
            ),
            retry_after: Some(60),
        });
    }
    let host = defaults
        .first()
        .map(|(mode, _)| mode.host.as_str())
        .unwrap_or("the server");
    Err(Unavailable {
        message: format!(
            "{} has nothing to serve: {host} is not serving any model right now (it may be switching or \
             restarting). Try again in a few minutes.",
            config.default_id
        ),
        retry_after: Some(120),
    })
}

/// Whether `name` is the default id and some mode names a model for it.
pub fn is_default(config: &Config, name: &str) -> bool {
    name == config.default_id && config.modes.iter().any(|m| m.default_model.is_some())
}

fn hint(config: &Config, wanted_label: &str) -> String {
    config.switch_hint.replace("{mode}", wanted_label)
}

fn or_list(items: &[String]) -> String {
    match items {
        [] => "another model".to_string(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} or {last}", init.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        serde_json::from_value(serde_json::json!({
            "listen": "127.0.0.1:0",
            "switch_hint": "ask the admin to switch son-of-anton to {mode}",
            "upstreams": {
                "unholy": {"base_url": "http://a/v1"},
                "radiance": {"base_url": "http://b/v1"},
                "gufo": {"base_url": "http://c/v1"}
            },
            "modes": [
                {"name": "ds4", "label": "DeepSeek-V4-Flash", "host": "son-of-anton", "upstreams": ["unholy"],
                 "default_model": "unholy/dsv4-flash"},
                {"name": "qwen", "label": "the Qwen models", "host": "son-of-anton", "upstreams": ["radiance", "gufo"],
                 "default_model": "radiance/qwen3.8-27b"}
            ],
            "models": [
                {"id": "unholy/dsv4-flash", "upstream": "unholy", "upstream_model": "dsv4-flash", "display_name": "DeepSeek-V4-Flash"},
                {"id": "radiance/qwen3.8-27b", "upstream": "radiance", "upstream_model": "qwen3.8-27b", "display_name": "Qwen3.8-27B"},
                {"id": "gufo/qwen3.8-flash-next", "upstream": "gufo", "upstream_model": "qwen3.8-flash-next", "display_name": "Qwen3.8-Flash-Next"}
            ],
            "keys": []
        }))
        .unwrap()
    }

    fn up(list: &[(&str, bool)]) -> HashMap<String, bool> {
        list.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn other_mode_serving_suggests_its_models_and_a_switch() {
        let c = config();
        let u = explain(
            &c,
            &c.models[1],
            &up(&[("unholy", true), ("radiance", false), ("gufo", false)]),
        );
        assert_eq!(
            u.message,
            "Qwen3.8-27B (radiance/qwen3.8-27b) is not loaded: son-of-anton is serving DeepSeek-V4-Flash right now. \
             Use DeepSeek-V4-Flash (unholy/dsv4-flash) instead, or ask the admin to switch son-of-anton to the Qwen models."
        );
        assert_eq!(u.retry_after, None);
        let u = explain(
            &c,
            &c.models[0],
            &up(&[("unholy", false), ("radiance", true), ("gufo", true)]),
        );
        assert!(u.message.contains("Use Qwen3.8-27B (radiance/qwen3.8-27b) or Qwen3.8-Flash-Next (gufo/qwen3.8-flash-next) instead"));
        assert!(
            u.message
                .ends_with("ask the admin to switch son-of-anton to DeepSeek-V4-Flash.")
        );
    }

    #[test]
    fn half_loaded_mode_says_wait() {
        let c = config();
        let u = explain(
            &c,
            &c.models[2],
            &up(&[("unholy", false), ("radiance", true), ("gufo", false)]),
        );
        assert!(u.message.contains("still loading"));
        assert_eq!(u.retry_after, Some(60));
    }

    #[tokio::test]
    async fn the_probe_sends_the_upstream_key() {
        use axum::http::{HeaderMap, StatusCode};
        // A hosted API: /models needs the key.
        let app = axum::Router::new().route(
            "/v1/models",
            axum::routing::get(|h: HeaderMap| async move {
                match h.get("authorization").and_then(|v| v.to_str().ok()) {
                    Some("Bearer sk-up") => StatusCode::OK,
                    _ => StatusCode::UNAUTHORIZED,
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        assert!(!Probes::new(HashMap::new()).is_up(&base).await);
        let keyed = Probes::new([(base.clone(), "sk-up".to_string())].into());
        assert!(keyed.is_up(&base).await);
    }

    #[test]
    fn the_default_is_whatever_mode_is_loaded() {
        let c = config();
        let ds4 = up(&[("unholy", true), ("radiance", false), ("gufo", false)]);
        assert_eq!(serving_default(&c, &ds4).unwrap().id, "unholy/dsv4-flash");
        let qwen = up(&[("unholy", false), ("radiance", true), ("gufo", true)]);
        assert_eq!(
            serving_default(&c, &qwen).unwrap().id,
            "radiance/qwen3.8-27b"
        );
    }

    #[test]
    fn the_default_waits_while_its_mode_loads_and_while_nothing_is_up() {
        let c = config();
        // Qwen mode is coming up: gufo answers, its default (radiance) not yet.
        let u = serving_default(
            &c,
            &up(&[("unholy", false), ("radiance", false), ("gufo", true)]),
        )
        .unwrap_err();
        assert!(u.message.contains("still loading"), "{}", u.message);
        assert_eq!(u.retry_after, Some(60));
        let u = serving_default(&c, &up(&[])).unwrap_err();
        assert!(u.message.contains("not serving any model"), "{}", u.message);
        assert_eq!(u.retry_after, Some(120));
    }

    #[test]
    fn only_the_default_id_with_mode_defaults_is_the_default() {
        let c = config();
        assert!(is_default(&c, "default"));
        assert!(!is_default(&c, "unholy/dsv4-flash"));
        let mut bare = config();
        for m in &mut bare.modes {
            m.default_model = None;
        }
        assert!(
            !is_default(&bare, "default"),
            "no mode names one: the id means nothing"
        );
    }

    #[test]
    fn nothing_serving_says_switching() {
        let c = config();
        let u = explain(&c, &c.models[0], &up(&[]));
        assert!(u.message.contains("not serving any model right now"));
        assert_eq!(u.retry_after, Some(120));
    }
}
