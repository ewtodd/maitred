//! Which upstreams are answering, and what to tell a caller whose model is not being served.

use crate::config::{Config, Model};
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
        // A fresh connection for every probe. cpp-httplib servers (unholy, eklow, llama.cpp) close a
        // keep-alive connection after 5 s idle, and a probe comes due just after PROBE_TTL, also 5 s:
        // a pooled connection was being reused as the server closed it, and the upstream read as down
        // for the next 5 s while it answered in a millisecond.
        let client = reqwest::Client::builder()
            .connect_timeout(PROBE_TIMEOUT)
            .timeout(PROBE_TIMEOUT)
            .pool_max_idle_per_host(0)
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
    // Modes can share an upstream (one card server beside either of two others): what is up only means
    // the wanted mode is loading while no other mode on the host has an upstream of its own up.
    let rival_up = config
        .modes
        .iter()
        .filter(|m| m.name != wanted.name && m.host == wanted.host)
        .flat_map(|m| &m.upstreams)
        .any(|u| is_up(u) && !wanted.upstreams.contains(u));
    if !rival_up && wanted.upstreams.iter().any(|u| is_up(u)) {
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
    // With a shared upstream a mode that is not loaded looks half up; the mode that is loaded is all up.
    let other = serving
        .iter()
        .find(|m| m.upstreams.iter().all(|u| is_up(u)))
        .unwrap_or(&serving[0]);
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

pub fn hint(config: &Config, wanted_label: &str) -> String {
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

    /// A server that answers only the first request on each connection and then closes it, as a
    /// keep-alive connection behaves when the server times it out as it is reused.
    async fn one_request_per_connection() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let mut got = Vec::new();
                    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = sock.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        got.extend_from_slice(&buf[..n]);
                    }
                    let body = b"{\"data\":[]}";
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body).await;
                    // The next request on this connection gets no answer.
                    let _ = sock.read(&mut buf).await;
                });
            }
        });
        format!("http://{addr}/v1")
    }

    #[tokio::test]
    async fn probes_never_reuse_a_connection() {
        let base = one_request_per_connection().await;
        let probes = Probes::new(HashMap::new());
        for i in 0..3 {
            probes.invalidate(&base);
            assert!(probes.is_up(&base).await, "probe {i} on a stale connection");
        }
    }

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
                {"name": "ds4", "label": "DeepSeek-V4-Flash", "host": "son-of-anton", "upstreams": ["unholy"]},
                {"name": "qwen", "label": "the Qwen models", "host": "son-of-anton", "upstreams": ["radiance", "gufo"]}
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

    /// `config()` with a third mode that shares radiance with the Qwen pair.
    fn shared_config() -> Config {
        serde_json::from_value(serde_json::json!({
            "listen": "127.0.0.1:0",
            "switch_hint": "ask the admin to switch son-of-anton to {mode}",
            "upstreams": {
                "unholy": {"base_url": "http://a/v1"},
                "radiance": {"base_url": "http://b/v1"},
                "gufo": {"base_url": "http://c/v1"},
                "fork": {"base_url": "http://d/v1"}
            },
            "modes": [
                {"name": "ds4", "label": "DeepSeek-V4-Flash", "host": "son-of-anton", "upstreams": ["unholy"]},
                {"name": "qwen", "label": "the Qwen models", "host": "son-of-anton", "upstreams": ["radiance", "gufo"]},
                {"name": "dual", "label": "V4.1 and the 27B", "host": "son-of-anton", "upstreams": ["radiance", "fork"]}
            ],
            "models": [
                {"id": "unholy/dsv4-flash", "upstream": "unholy", "upstream_model": "dsv4-flash", "display_name": "DeepSeek-V4-Flash"},
                {"id": "radiance/qwen3.8-27b", "upstream": "radiance", "upstream_model": "qwen3.8-27b", "display_name": "Qwen3.8-27B"},
                {"id": "gufo/qwen3.8-flash-next", "upstream": "gufo", "upstream_model": "qwen3.8-flash-next", "display_name": "Qwen3.8-Flash-Next"},
                {"id": "fork/deepseek-v4.1-flash", "upstream": "fork", "upstream_model": "deepseek-v4.1-flash", "display_name": "DeepSeek-V4.1-Flash"}
            ],
            "keys": []
        }))
        .unwrap()
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

    #[test]
    fn a_shared_upstream_does_not_make_the_mode_that_is_not_loaded_look_like_it_is_loading() {
        let c = shared_config();
        // llm-mode dual: Flash-Next's mode has radiance up, but the fork says dual is what is loaded.
        let u = explain(
            &c,
            &c.models[2],
            &up(&[
                ("unholy", false),
                ("radiance", true),
                ("gufo", false),
                ("fork", true),
            ]),
        );
        assert_eq!(
            u.message,
            "Qwen3.8-Flash-Next (gufo/qwen3.8-flash-next) is not loaded: son-of-anton is serving V4.1 and the 27B right now. \
             Use Qwen3.8-27B (radiance/qwen3.8-27b) or DeepSeek-V4.1-Flash (fork/deepseek-v4.1-flash) instead, or ask the \
             admin to switch son-of-anton to the Qwen models."
        );
        assert_eq!(u.retry_after, None);
        // And the other way round.
        let u = explain(
            &c,
            &c.models[3],
            &up(&[
                ("unholy", false),
                ("radiance", true),
                ("gufo", true),
                ("fork", false),
            ]),
        );
        assert!(
            u.message
                .contains("is not loaded: son-of-anton is serving the Qwen models right now.")
        );
        assert!(
            u.message
                .ends_with("ask the admin to switch son-of-anton to V4.1 and the 27B.")
        );
    }

    #[test]
    fn the_mode_named_as_serving_is_the_one_that_is_all_up() {
        let c = shared_config();
        // The Qwen pair comes first and has radiance up; dual is what is loaded.
        let u = explain(
            &c,
            &c.models[0],
            &up(&[
                ("unholy", false),
                ("radiance", true),
                ("gufo", false),
                ("fork", true),
            ]),
        );
        assert!(u.message.contains("serving V4.1 and the 27B right now"));
    }

    #[test]
    fn a_shared_upstream_alone_still_says_wait() {
        let c = shared_config();
        // Radiance up and neither gufo yet: either mode may be on its way.
        for model in &c.models[2..4] {
            let u = explain(
                &c,
                model,
                &up(&[
                    ("unholy", false),
                    ("radiance", true),
                    ("gufo", false),
                    ("fork", false),
                ]),
            );
            assert!(u.message.contains("still loading"), "{}", u.message);
        }
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
    fn nothing_serving_says_switching() {
        let c = config();
        let u = explain(&c, &c.models[0], &up(&[]));
        assert!(u.message.contains("not serving any model right now"));
        assert_eq!(u.retry_after, Some(120));
    }
}
