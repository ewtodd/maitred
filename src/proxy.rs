//! The HTTP side: `/v1/models`, the `/v1/*` proxy, and verbatim passthrough paths.

use crate::auth::{self, Denied, Keys};
use crate::config::{Config, Model, Passthrough};
use crate::effort;
use crate::mcp::Mcp;
use crate::status::{self, Probes, Unavailable};
use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct AppState {
    pub config: Config,
    pub keys: Keys,
    pub probes: Probes,
    pub mcp: Mcp,
    clients: HashMap<String, reqwest::Client>,
    upstream_keys: HashMap<String, String>,
    passthrough_client: reqwest::Client,
}

impl AppState {
    pub fn new(config: Config) -> Result<AppState, String> {
        let mut clients = HashMap::new();
        let mut upstream_keys = HashMap::new();
        for (name, u) in &config.upstreams {
            let client = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(u.connect_timeout_secs))
                .timeout(Duration::from_secs(u.timeout_secs))
                .build()
                .map_err(|e| format!("upstream {name}: {e}"))?;
            clients.insert(name.clone(), client);
            if let Some(env) = &u.api_key_env {
                match std::env::var(env) {
                    Ok(v) if !v.is_empty() => {
                        upstream_keys.insert(name.clone(), v);
                    }
                    _ => tracing::warn!(upstream = %name, env = %env, "upstream API key not set"),
                }
            }
        }
        let keys = Keys::from_env(&config.keys);
        let passthrough_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| format!("passthrough client: {e}"))?;
        let mcp = Mcp::new(&config.mcp);
        let probe_keys = upstream_keys
            .iter()
            .map(|(name, key)| (config.upstreams[name].base_url.clone(), key.clone()))
            .collect();
        Ok(AppState {
            config,
            keys,
            probes: Probes::new(probe_keys),
            mcp,
            clients,
            upstream_keys,
            passthrough_client,
        })
    }
}

pub fn error(status: StatusCode, kind: &str, code: &str, message: impl Into<String>) -> Response {
    let body =
        json!({"error": {"message": message.into(), "type": kind, "code": code, "param": null}});
    (status, Json(body)).into_response()
}

fn with_retry_after(mut r: Response, secs: u64) -> Response {
    r.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(secs));
    r
}

pub fn denied(d: Denied) -> Response {
    match d {
        Denied::Missing => error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "missing_api_key",
            "No API key: send it as `Authorization: Bearer <key>`.",
        ),
        Denied::Unknown => error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid_api_key",
            "Unknown API key.",
        ),
        Denied::Inactive(message) => error(
            StatusCode::FORBIDDEN,
            "permission_error",
            "key_inactive",
            message,
        ),
        Denied::Model { key, allowed } => error(
            StatusCode::FORBIDDEN,
            "permission_error",
            "model_not_allowed",
            format!(
                "The {key} key cannot use this model. It can use: {}.",
                allowed.join(", ")
            ),
        ),
        Denied::RateLimited { retry_after } => with_retry_after(
            error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "rate_limit_exceeded",
                format!("Rate limit reached; try again in {retry_after} s."),
            ),
            retry_after,
        ),
    }
}

/// The 503 for a model whose server is not up. `model_not_loaded` is the code clients key on.
fn unavailable(u: Unavailable) -> Response {
    let r = error(
        StatusCode::SERVICE_UNAVAILABLE,
        "model_unavailable",
        "model_not_loaded",
        u.message,
    );
    match u.retry_after {
        Some(s) => with_retry_after(r, s),
        None => r,
    }
}

/// A model by its id (`provider/name`), or by a bare name that matches exactly one model.
pub fn resolve<'a>(config: &'a Config, name: &str) -> Option<&'a Model> {
    if let Some(m) = config.models.iter().find(|m| m.id == name) {
        return Some(m);
    }
    let mut matches = config.models.iter().filter(|m| {
        m.upstream_model == name || m.id.rsplit_once('/').map(|(_, n)| n) == Some(name)
    });
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

fn model_entry(config: &Config, m: &Model, up: &HashMap<String, bool>) -> Value {
    let serving = up.get(&m.upstream).copied().unwrap_or(false);
    let mut e = json!({"id": m.id, "object": "model", "created": 0, "owned_by": m.upstream});
    if let Some(n) = &m.display_name {
        e["name"] = json!(n);
    }
    if let Some(c) = m.context_length {
        // Under the names son-of-anton, vLLM-style and gufo-style clients look for.
        e["context_length"] = json!(c);
        e["max_model_len"] = json!(c);
    }
    if let Some(c) = m.max_completion_tokens {
        e["max_completion_tokens"] = json!(c);
    }
    if !m.reasoning_efforts.is_empty() {
        e["reasoning_efforts"] = json!(m.reasoning_efforts);
    }
    if let Some(d) = &m.default_reasoning_effort {
        e["default_reasoning_effort"] = json!(d);
    }
    if !m.effort_aliases.is_empty() {
        // Sorted, so the listing is stable.
        let aliases: std::collections::BTreeMap<_, _> = m.effort_aliases.iter().collect();
        e["reasoning_effort_aliases"] = json!(aliases);
    }
    e["status"] = json!(if serving { "serving" } else { "unavailable" });
    if !serving {
        e["status_reason"] = json!(status::explain(config, m, up).message);
    }
    for (k, v) in &m.extra {
        e[k] = v.clone();
    }
    e
}

/// `GET /v1/models`: every model, whether or not its server is up, with live status. A valid key narrows the
/// list to the models it may use; no key lists them all (as Bifrost did).
pub async fn models(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let key = st.keys.identify(&headers).ok();
    let up = st.probes.snapshot(&st.config).await;
    let data: Vec<Value> = st
        .config
        .models
        .iter()
        .filter(|m| key.is_none_or(|k| auth::may_use(k, &m.id)))
        .map(|m| model_entry(&st.config, m, &up))
        .collect();
    Json(json!({"object": "list", "data": data})).into_response()
}

/// `GET /v1/models/{id}`.
pub async fn model(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let Some(m) = resolve(&st.config, &id) else {
        return error(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "model_not_found",
            format!("Unknown model {id}."),
        );
    };
    let up = st.probes.snapshot(&st.config).await;
    Json(model_entry(&st.config, m, &up)).into_response()
}

/// `POST /v1/{rest}`: authenticate, resolve the model, translate its effort, forward, stream the answer back.
pub async fn v1(
    State(st): State<Arc<AppState>>,
    Path(rest): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    let key = match st.keys.identify(&headers) {
        Ok(k) => k,
        Err(d) => return denied(d),
    };
    let mut req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_json",
                format!("The request body is not JSON: {e}"),
            );
        }
    };
    let Some(name) = req.get("model").and_then(Value::as_str).map(str::to_string) else {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "missing_model",
            "The request names no model.",
        );
    };
    let Some(model) = resolve(&st.config, &name) else {
        let ids: Vec<&str> = st
            .config
            .models
            .iter()
            .filter(|m| auth::may_use(key, &m.id))
            .map(|m| m.id.as_str())
            .collect();
        return error(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "model_not_found",
            format!("Unknown model {name}. Available: {}.", ids.join(", ")),
        );
    };
    if !auth::may_use(key, &model.id) {
        let allowed = st
            .config
            .models
            .iter()
            .filter(|m| auth::may_use(key, &m.id))
            .map(|m| m.id.clone())
            .collect();
        return denied(Denied::Model {
            key: key.name.clone(),
            allowed,
        });
    }
    if let Err(d) = st.keys.admit(key, chrono::Utc::now()) {
        return denied(d);
    }

    req["model"] = Value::String(model.upstream_model.clone());
    let rewrites =
        effort::rewrite_request(&mut req, &model.reasoning_efforts, &model.effort_aliases);
    let upstream = &st.config.upstreams[&model.upstream];
    let url = format!("{}/{}", upstream.base_url.trim_end_matches('/'), rest);
    let mut out = st.clients[&model.upstream]
        .post(&url)
        .header(header::CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(&req).expect("serialize request"));
    for h in [header::ACCEPT, header::USER_AGENT] {
        if let Some(v) = headers.get(&h) {
            out = out.header(h, v);
        }
    }
    if let Some(k) = st.upstream_keys.get(&model.upstream) {
        out = out.bearer_auth(k);
    }
    let effort_log: Vec<String> = rewrites
        .iter()
        .map(|r| format!("{} {}->{}", r.field, r.from, r.to))
        .collect();

    let resp = match out.send().await {
        Ok(r) => r,
        Err(e) if e.is_connect() => {
            st.probes.invalidate(&upstream.base_url);
            let up = st.probes.snapshot(&st.config).await;
            let u = status::explain(&st.config, model, &up);
            tracing::info!(key = %key.name, model = %model.id, path = %rest, status = 503, ms = started.elapsed().as_millis() as u64, "unavailable: {}", u.message);
            return unavailable(u);
        }
        Err(e) if e.is_timeout() => {
            tracing::warn!(key = %key.name, model = %model.id, path = %rest, "upstream timed out: {e}");
            return error(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_error",
                "upstream_timeout",
                format!("{} did not answer in time.", model.display()),
            );
        }
        Err(e) => {
            tracing::warn!(key = %key.name, model = %model.id, path = %rest, "upstream error: {e}");
            return error(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "upstream_error",
                format!("{} failed: {e}", model.display()),
            );
        }
    };
    tracing::info!(
        key = %key.name,
        model = %model.id,
        path = %rest,
        status = resp.status().as_u16(),
        ms = started.elapsed().as_millis() as u64,
        effort = %effort_log.join(","),
        "request"
    );
    relay(resp, Duration::from_secs(upstream.idle_timeout_secs))
}

/// Anything else: a configured passthrough prefix (e.g. `/mcp` to Bifrost), or 404.
pub async fn fallback(State(st): State<Arc<AppState>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    match st
        .config
        .passthrough
        .iter()
        .find(|p| path.starts_with(&p.prefix))
    {
        Some(p) => passthrough(&st, p, req).await,
        None => error(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "not_found",
            format!("No route for {path}."),
        ),
    }
}

async fn passthrough(st: &AppState, p: &Passthrough, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|x| x.as_str())
        .unwrap_or("/");
    let url = format!("{}{}", p.target.trim_end_matches('/'), path);
    let mut out = st.passthrough_client.request(parts.method.clone(), &url);
    for (k, v) in &parts.headers {
        if !hop_by_hop(k) && k != header::HOST {
            out = out.header(k, v);
        }
    }
    out = out.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    match out.send().await {
        Ok(resp) => relay(resp, Duration::from_secs(p.timeout_secs)),
        Err(e) => error(
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "upstream_error",
            format!("{} failed: {e}", p.prefix),
        ),
    }
}

fn hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
    )
}

/// Stream an upstream response back as it arrives, ending it if the upstream goes quiet for `idle`.
fn relay(resp: reqwest::Response, idle: Duration) -> Response {
    let mut builder = Response::builder().status(resp.status());
    for (k, v) in resp.headers() {
        if !hop_by_hop(k) {
            builder = builder.header(k, v);
        }
    }
    let stream = futures_util::stream::unfold(Some(resp.bytes_stream()), move |state| async move {
        let mut s = state?;
        match tokio::time::timeout(idle, s.next()).await {
            Ok(Some(Ok(chunk))) => Some((Ok::<Bytes, std::io::Error>(chunk), Some(s))),
            Ok(Some(Err(e))) => Some((Err(std::io::Error::other(e)), None)),
            Ok(None) => None,
            Err(_) => Some((
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "upstream went quiet",
                )),
                None,
            )),
        }
    });
    builder.body(Body::from_stream(stream)).unwrap_or_else(|e| {
        error(
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "upstream_error",
            format!("bad upstream response: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        serde_json::from_value(json!({
            "listen": "127.0.0.1:0",
            "upstreams": {"unholy": {"base_url": "http://a/v1"}, "radiance": {"base_url": "http://b/v1"}, "embed": {"base_url": "http://c/v1"}},
            "models": [
                {"id": "unholy/dsv4-flash", "upstream": "unholy", "upstream_model": "dsv4-flash", "context_length": 524288,
                 "reasoning_efforts": ["none", "low", "high", "max"], "default_reasoning_effort": "high",
                 "effort_aliases": {"medium": "high"}},
                {"id": "radiance/qwen3.8-27b", "upstream": "radiance", "upstream_model": "qwen3.8-27b"},
                {"id": "embed/bge-m3", "upstream": "embed", "upstream_model": "bge-m3"}
            ],
            "keys": []
        }))
        .unwrap()
    }

    #[test]
    fn resolves_ids_and_unambiguous_bare_names() {
        let c = config();
        assert_eq!(
            resolve(&c, "unholy/dsv4-flash").unwrap().id,
            "unholy/dsv4-flash"
        );
        assert_eq!(resolve(&c, "dsv4-flash").unwrap().id, "unholy/dsv4-flash");
        assert_eq!(resolve(&c, "bge-m3").unwrap().id, "embed/bge-m3");
        assert!(resolve(&c, "gpt-4").is_none());
    }

    #[test]
    fn model_entries_carry_window_efforts_and_status() {
        let c = config();
        let up: HashMap<String, bool> = [("unholy".to_string(), true)].into();
        let e = model_entry(&c, &c.models[0], &up);
        assert_eq!(e["context_length"], 524288);
        assert_eq!(e["max_model_len"], 524288);
        assert_eq!(
            e["reasoning_efforts"],
            json!(["none", "low", "high", "max"])
        );
        assert_eq!(e["default_reasoning_effort"], "high");
        assert_eq!(e["reasoning_effort_aliases"], json!({"medium": "high"}));
        assert_eq!(e["status"], "serving");
        let e = model_entry(&c, &c.models[1], &up);
        assert_eq!(e["status"], "unavailable");
        assert!(
            e["status_reason"].as_str().unwrap().contains("Qwen")
                || e["status_reason"].as_str().unwrap().contains("qwen3.8-27b")
        );
    }
}
