//! The HTTP side: `/v1/models`, the `/v1/*` proxy, and verbatim passthrough paths.

use crate::auth::{self, Denied, Keys};
use crate::config::{Config, Key, Model, Passthrough};
use crate::effort;
use crate::mcp::Mcp;
use crate::router::{self, Activity, DecisionLog, InFlight, NoRoute};
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
    pub activity: Arc<Activity>,
    decisions: Option<DecisionLog>,
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
        let decisions = config
            .router
            .decision_log
            .as_deref()
            .map(|path| DecisionLog::open(path, config.router.log_prompts))
            .transpose()?;
        let activity = Activity::new(Duration::from_secs(config.router.forget_after_secs));
        Ok(AppState {
            config,
            keys,
            probes: Probes::new(probe_keys),
            mcp,
            activity,
            decisions,
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

/// Set the model's `request_overrides` on the request body, replacing what the client sent.
fn apply_overrides(req: &mut Value, overrides: &serde_json::Map<String, Value>) {
    if let Some(body) = req.as_object_mut() {
        for (k, v) in overrides {
            body.insert(k.clone(), v.clone());
        }
    }
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

/// The `/v1/models` entry for the routed id. Its window is the largest among the routed models serving now (the
/// router keeps a conversation off a model it has outgrown), its efforts are every level one of them takes (each
/// request is translated for the model it lands on), and `routes_to` lists its models, strongest first.
fn router_entry(config: &Config, up: &HashMap<String, bool>) -> Value {
    let mut routed: Vec<&Model> = config.models.iter().filter(|m| m.tier.is_some()).collect();
    routed.sort_by_key(|m| std::cmp::Reverse(m.tier));
    let serving: Vec<&Model> = routed
        .iter()
        .copied()
        .filter(|m| up.get(&m.upstream).copied().unwrap_or(false))
        .collect();
    let basis = if serving.is_empty() {
        &routed
    } else {
        &serving
    };
    let mut e = json!({
        "id": config.router.id,
        "object": "model",
        "created": 0,
        "owned_by": "maitred",
        "name": "Routed: maitred picks the model",
        "routes_to": routed.iter().map(|m| &m.id).collect::<Vec<_>>(),
    });
    if let Some(c) = basis.iter().filter_map(|m| m.context_length).max() {
        e["context_length"] = json!(c);
        e["max_model_len"] = json!(c);
    }
    let efforts: Vec<&str> = effort::LADDER
        .iter()
        .copied()
        .filter(|l| {
            basis
                .iter()
                .any(|m| m.reasoning_efforts.iter().any(|x| x == l))
        })
        .collect();
    if !efforts.is_empty() {
        e["reasoning_efforts"] = json!(efforts);
    }
    if serving.is_empty() {
        e["status"] = json!("unavailable");
        e["status_reason"] = json!(format!(
            "None of the models {} routes to is answering right now.",
            config.router.id
        ));
    } else {
        e["status"] = json!("serving");
    }
    e
}

/// `GET /v1/models`: every model, whether or not its server is up, with live status, led by the routed id when
/// it is on. A valid key narrows the list to the models it may use; no key lists them all (as Bifrost did).
pub async fn models(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let key = st.keys.identify(&headers).ok();
    let up = st.probes.snapshot(&st.config).await;
    let mut data: Vec<Value> = Vec::new();
    if router::is_router(&st.config, &st.config.router.id)
        && key.is_none_or(|k| auth::may_use(k, &st.config.router.id))
    {
        data.push(router_entry(&st.config, &up));
    }
    data.extend(
        st.config
            .models
            .iter()
            .filter(|m| !m.hidden)
            .filter(|m| key.is_none_or(|k| auth::may_use(k, &m.id)))
            .map(|m| model_entry(&st.config, m, &up)),
    );
    Json(json!({"object": "list", "data": data})).into_response()
}

/// `GET /v1/models/{id}`.
pub async fn model(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    if router::is_router(&st.config, &id) {
        let up = st.probes.snapshot(&st.config).await;
        return Json(router_entry(&st.config, &up)).into_response();
    }
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

/// What a request's `model` names.
enum Target<'a> {
    /// The routed id: maitred picks the model, from what the body looks like.
    Routed(router::Shape),
    Named(&'a Model),
}

/// The model to send a request to now; for the routed id, with the decision.
async fn pick<'a>(
    st: &'a AppState,
    target: &Target<'a>,
) -> Result<(&'a Model, Option<router::Decision<'a>>), NoRoute> {
    match target {
        Target::Named(m) => Ok((*m, None)),
        Target::Routed(shape) => {
            let up = st.probes.snapshot(&st.config).await;
            router::route(&st.config, &up, &st.activity, shape).map(|d| (d.model, Some(d)))
        }
    }
}

/// The 404 for a name that is no model, listing what the key may ask for.
fn unknown_model(config: &Config, key: &Key, name: &str) -> Response {
    let mut ids: Vec<&str> = config
        .models
        .iter()
        .filter(|m| !m.hidden)
        .filter(|m| auth::may_use(key, &m.id))
        .map(|m| m.id.as_str())
        .collect();
    if router::is_router(config, &config.router.id) && auth::may_use(key, &config.router.id) {
        ids.insert(0, &config.router.id);
    }
    error(
        StatusCode::NOT_FOUND,
        "invalid_request_error",
        "model_not_found",
        format!("Unknown model {name}. Available: {}.", ids.join(", ")),
    )
}

/// `POST /v1/{rest}`: authenticate, resolve (or route to) the model, translate its effort, forward, stream the
/// answer back.
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
    // The routed id is resolved per request, never as a fallback for a named model.
    let target = if router::is_router(&st.config, &name) {
        if req
            .get("previous_response_id")
            .is_some_and(|v| !v.is_null())
        {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "unsupported_parameter",
                format!(
                    "{name} cannot follow previous_response_id: the stored response lives on one server. \
                     Name the model instead."
                ),
            );
        }
        Target::Routed(router::shape(&key.name, &req))
    } else {
        match resolve(&st.config, &name) {
            Some(m) => Target::Named(m),
            None => return unknown_model(&st.config, key, &name),
        }
    };
    // A key is granted the id it asked for: the routed id itself, not whatever it routes to now.
    let granted = match &target {
        Target::Named(m) => m.id.as_str(),
        Target::Routed(_) => name.as_str(),
    };
    if !auth::may_use(key, granted) {
        let allowed = st
            .config
            .models
            .iter()
            .filter(|m| !m.hidden)
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

    // A routed request whose server turns out not to answer (its probe was stale) is routed once more.
    let attempts = if matches!(target, Target::Routed(_)) {
        2
    } else {
        1
    };
    let mut attempt = 1;
    loop {
        let (model, decision) = match pick(&st, &target).await {
            Ok(picked) => picked,
            Err(NoRoute::Unavailable(u)) => {
                tracing::info!(key = %key.name, model = %name, path = %rest, status = 503, ms = started.elapsed().as_millis() as u64, "unavailable: {}", u.message);
                return unavailable(u);
            }
            Err(NoRoute::TooLong(message)) => {
                tracing::info!(key = %key.name, model = %name, path = %rest, status = 400, "too long: {message}");
                return error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "context_length_exceeded",
                    message,
                );
            }
        };
        if let (Some(log), Some(d), Target::Routed(shape)) = (&st.decisions, &decision, &target) {
            log.write(&key.name, shape, d);
        }
        // The client's body is kept for another attempt, and taken on the last.
        let mut out_body = if attempt < attempts {
            req.clone()
        } else {
            std::mem::take(&mut req)
        };
        out_body["model"] = Value::String(model.upstream_model.clone());
        apply_overrides(&mut out_body, &model.request_overrides);
        let rewrites = effort::rewrite_request(
            &mut out_body,
            &model.reasoning_efforts,
            &model.effort_aliases,
        );
        let upstream = &st.config.upstreams[&model.upstream];
        let url = format!("{}/{}", upstream.base_url.trim_end_matches('/'), rest);
        let mut out = st.clients[&model.upstream]
            .post(&url)
            .header(header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&out_body).expect("serialize request"));
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
        let route = decision.as_ref().map_or("", |d| d.reason.as_str());

        let in_flight = st.activity.enter(&model.upstream);
        let resp = match out.send().await {
            Ok(r) => r,
            Err(e) if e.is_connect() => {
                st.probes.invalidate(&upstream.base_url);
                if attempt < attempts {
                    tracing::info!(key = %key.name, model = %model.id, path = %rest, "{} did not answer; routing again", model.display());
                    attempt += 1;
                    continue;
                }
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
            requested = %name,
            route = %route,
            path = %rest,
            status = resp.status().as_u16(),
            ms = started.elapsed().as_millis() as u64,
            effort = %effort_log.join(","),
            "request"
        );
        return relay(
            resp,
            Duration::from_secs(upstream.idle_timeout_secs),
            Some(in_flight),
        );
    }
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
        Ok(resp) => relay(resp, Duration::from_secs(p.timeout_secs), None),
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

/// Stream an upstream response back as it arrives, ending it if the upstream goes quiet for `idle`. The request
/// stays counted in flight until the stream ends or the client goes away.
fn relay(resp: reqwest::Response, idle: Duration, in_flight: Option<InFlight>) -> Response {
    let mut builder = Response::builder().status(resp.status());
    for (k, v) in resp.headers() {
        if !hop_by_hop(k) {
            builder = builder.header(k, v);
        }
    }
    let start = Some((resp.bytes_stream(), in_flight));
    let stream = futures_util::stream::unfold(start, move |state| async move {
        let (mut s, in_flight) = state?;
        match tokio::time::timeout(idle, s.next()).await {
            Ok(Some(Ok(chunk))) => Some((Ok::<Bytes, std::io::Error>(chunk), Some((s, in_flight)))),
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

    /// A fake upstream that answers `/models` and echoes a chat request's body back.
    async fn echo_upstream() -> String {
        echo_upstream_handle().await.0
    }

    /// The same, with the task serving it, so a test can take it down.
    async fn echo_upstream_handle() -> (String, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new()
            .route("/v1/models", axum::routing::get(|| async { "{}" }))
            .route(
                "/v1/chat/completions",
                axum::routing::post(|body: Bytes| async move { body }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, handle)
    }

    /// Three routed models: `down/top` (tier 4, never answering), `big/strong` (tier 3, one slot) at `big`, and
    /// `mid/plain` (tier 2) on an echo upstream; a key granted only `auto`, and one granted everything.
    async fn routed_state(big: String) -> Arc<AppState> {
        let mid = echo_upstream().await;
        let config: Config = serde_json::from_value(json!({
            "listen": "127.0.0.1:0",
            "upstreams": {
                "down": {"base_url": "http://127.0.0.1:1/v1"},
                "big": {"base_url": big, "slots": 1},
                "mid": {"base_url": mid}
            },
            "models": [
                {"id": "down/top", "upstream": "down", "upstream_model": "top", "tier": 4,
                 "context_length": 100000},
                {"id": "big/strong", "upstream": "big", "upstream_model": "strong", "tier": 3,
                 "context_length": 4000, "reasoning_efforts": ["none", "low", "high"]},
                {"id": "mid/plain", "upstream": "mid", "upstream_model": "plain", "tier": 2,
                 "context_length": 2000, "reasoning_efforts": ["none", "low", "medium", "xhigh"]},
                {"id": "mid/untiered", "upstream": "mid", "upstream_model": "untiered"}
            ],
            "router": {"move_below_tokens": 100},
            "keys": [
                {"name": "app", "env": "UNUSED", "models": ["auto"]},
                {"name": "all", "env": "UNUSED"}
            ]
        }))
        .unwrap();
        config.validate().unwrap();
        let keys = config.keys.clone();
        let mut st = AppState::new(config).unwrap();
        st.keys = Keys::new(vec![
            ("sk-app".to_string(), keys[0].clone()),
            ("sk-all".to_string(), keys[1].clone()),
        ]);
        Arc::new(st)
    }

    fn auto(messages: Value) -> Value {
        json!({"model": "auto", "messages": messages})
    }

    async fn post(st: &Arc<AppState>, secret: &str, body: Value) -> (StatusCode, Value) {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {secret}")).unwrap(),
        );
        let resp = v1(
            State(st.clone()),
            Path("chat/completions".to_string()),
            headers,
            Bytes::from(serde_json::to_vec(&body).unwrap()),
        )
        .await;
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn a_named_model_is_never_routed_elsewhere() {
        // auto would serve this on big or mid; named outright, down/top is refused as not loaded.
        let st = routed_state(echo_upstream().await).await;
        let (status, err) = post(
            &st,
            "sk-all",
            json!({"model": "down/top", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{err}");
        assert_eq!(err["error"]["code"], "model_not_loaded");
    }

    #[tokio::test]
    async fn hidden_models_leave_the_listing_but_stay_resolvable_and_served() {
        let loaded = echo_upstream().await;
        let config: Config = serde_json::from_value(json!({
            "listen": "127.0.0.1:0",
            "upstreams": {"embed": {"base_url": loaded}},
            "models": [
                {"id": "embed/bge-m3", "upstream": "embed", "upstream_model": "bge-m3", "hidden": true}
            ],
            "keys": [{"name": "all", "env": "UNUSED"}]
        }))
        .unwrap();
        config.validate().unwrap();
        let keys = config.keys.clone();
        let mut st = AppState::new(config).unwrap();
        st.keys = Keys::new(vec![("sk-all".to_string(), keys[0].clone())]);
        let st = Arc::new(st);

        // The listing omits the hidden model entirely.
        let resp = models(State(st.clone()), HeaderMap::new()).await;
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let listing: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(listing["data"], json!([]));

        // Asked for by name it is still described and served.
        let resp = model(State(st.clone()), Path("embed/bge-m3".to_string())).await;
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let entry: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(entry["id"], "embed/bge-m3");

        let (status, echoed) =
            post(&st, "sk-all", json!({"model": "bge-m3", "messages": []})).await;
        assert_eq!(status, StatusCode::OK, "{echoed}");
        assert_eq!(echoed["model"], "bge-m3");
    }

    #[tokio::test]
    async fn the_routed_id_moves_a_conversation_only_when_it_is_cheap_or_it_has_to() {
        let st = routed_state(echo_upstream().await).await;
        let landed = |echoed: &Value| echoed["model"].as_str().unwrap().to_string();

        // A new conversation gets the strongest serving model, its effort translated for it; a key granted only
        // the routed id may use it, and not the models behind it.
        let (status, echoed) = post(
            &st,
            "sk-app",
            json!({"model": "auto", "reasoning_effort": "xhigh",
                   "messages": [{"role": "user", "content": "first"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{echoed}");
        assert_eq!(landed(&echoed), "strong");
        assert_eq!(echoed["reasoning_effort"], "high");
        let (status, _) = post(
            &st,
            "sk-app",
            json!({"model": "big/strong", "messages": []}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // With big's one slot taken, a short conversation starts on mid; once the slot is free, its next user
        // message moves it up, since a short conversation is cheap to re-read.
        let busy = st.activity.enter("big");
        let opening = json!([{"role": "user", "content": "hi"}]);
        let (_, echoed) = post(&st, "sk-all", auto(opening)).await;
        assert_eq!(landed(&echoed), "plain");
        drop(busy);
        let (_, echoed) = post(
            &st,
            "sk-all",
            auto(json!([
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"},
                {"role": "user", "content": "more"}
            ])),
        )
        .await;
        assert_eq!(landed(&echoed), "strong");

        // A long conversation started on mid stays there, where it is cached, though big is free again.
        let long = "x".repeat(800);
        let busy = st.activity.enter("big");
        let (_, echoed) = post(
            &st,
            "sk-all",
            auto(json!([{"role": "user", "content": long}])),
        )
        .await;
        assert_eq!(landed(&echoed), "plain");
        drop(busy);
        let (_, echoed) = post(
            &st,
            "sk-all",
            auto(json!([
                {"role": "user", "content": long},
                {"role": "assistant", "content": "ok"},
                {"role": "user", "content": "go on"}
            ])),
        )
        .await;
        assert_eq!(landed(&echoed), "plain");

        // Mid tool loop a conversation stays on its model, however short.
        let busy = st.activity.enter("big");
        let (_, echoed) = post(
            &st,
            "sk-all",
            auto(json!([{"role": "user", "content": "run it"}])),
        )
        .await;
        assert_eq!(landed(&echoed), "plain");
        drop(busy);
        let (_, echoed) = post(
            &st,
            "sk-all",
            auto(json!([
                {"role": "user", "content": "run it"},
                {"role": "assistant", "tool_calls": [{"id": "1", "type": "function",
                    "function": {"name": "run", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "1", "content": "done"}
            ])),
        )
        .await;
        assert_eq!(landed(&echoed), "plain");

        // Every response has been relayed, so nothing is counted in flight.
        assert_eq!(st.activity.in_flight("big"), 0);
        assert_eq!(st.activity.in_flight("mid"), 0);
    }

    #[tokio::test]
    async fn the_routed_id_keeps_a_conversation_off_models_it_has_outgrown() {
        let st = routed_state(echo_upstream().await).await;
        let user = |chars: usize| auto(json!([{"role": "user", "content": "y".repeat(chars)}]));

        // About 2,250 tokens: too long for mid's 2,000, so big takes it even with its slot taken.
        let busy = st.activity.enter("big");
        let (status, echoed) = post(&st, "sk-all", user(9_000)).await;
        assert_eq!(status, StatusCode::OK, "{echoed}");
        assert_eq!(echoed["model"], "strong");
        drop(busy);

        // About 5,000: only down/top would take it, and it is not serving. Waiting will not help by itself.
        let (status, err) = post(&st, "sk-all", user(20_000)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{err}");
        assert_eq!(err["error"]["code"], "model_not_loaded");
        assert!(
            err["error"]["message"]
                .as_str()
                .unwrap()
                .contains("down/top"),
            "{err}"
        );

        // About 100,000: longer than any routed model takes.
        let (status, err) = post(&st, "sk-all", user(400_000)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
        assert_eq!(err["error"]["code"], "context_length_exceeded");

        // A stored Responses conversation lives on one server: the router cannot follow it.
        let (status, err) = post(
            &st,
            "sk-all",
            json!({"model": "auto", "previous_response_id": "resp_1", "input": "and?"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    }

    #[tokio::test]
    async fn a_routed_request_to_a_server_that_just_went_away_is_routed_again() {
        let (big, server) = echo_upstream_handle().await;
        let st = routed_state(big.clone()).await;
        // The probe saw big up; then it stops, inside the probe's cache window.
        assert!(st.probes.is_up(&big).await);
        server.abort();
        let _ = server.await;
        let (status, echoed) = post(
            &st,
            "sk-all",
            auto(json!([{"role": "user", "content": "hi"}])),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{echoed}");
        assert_eq!(echoed["model"], "plain");
    }

    #[tokio::test]
    async fn the_listing_leads_with_the_routed_id() {
        let st = routed_state(echo_upstream().await).await;
        let resp = models(State(st.clone()), HeaderMap::new()).await;
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let listing: Value = serde_json::from_slice(&bytes).unwrap();
        let first = &listing["data"][0];
        assert_eq!(first["id"], "auto");
        assert_eq!(
            first["routes_to"],
            json!(["down/top", "big/strong", "mid/plain"])
        );
        // The largest window and every effort level among the models serving now (down/top is not).
        assert_eq!(first["context_length"], 4000);
        assert_eq!(
            first["reasoning_efforts"],
            json!(["none", "low", "medium", "high", "xhigh"])
        );
        assert_eq!(first["status"], "serving");
    }

    #[test]
    fn overrides_replace_what_the_client_sent() {
        let overrides = json!({"response_format": {"type": "json_object"}, "temperature": 0})
            .as_object()
            .unwrap()
            .clone();
        let mut req = json!({"model": "m", "temperature": 0.7, "messages": []});
        apply_overrides(&mut req, &overrides);
        assert_eq!(req["temperature"], 0);
        assert_eq!(req["response_format"]["type"], "json_object");
        assert_eq!(req["messages"], json!([]), "other fields are kept");
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
