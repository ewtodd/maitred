//! `/mcp`: one MCP endpoint (Streamable HTTP, JSON responses) in front of several stdio MCP servers.
//!
//! Each configured server is started on first use and kept running; requests to it are multiplexed over its
//! stdin/stdout by JSON-RPC id, and a server that exits is started again on the next call. A caller sees the
//! tools its key is granted, named `server-tool` (Bifrost's convention, so clients written against Bifrost's
//! gateway keep their tool names), and a call is routed back to the server by that prefix.

use crate::config::{Key, McpServer};
use crate::proxy::{AppState, denied, error};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, oneshot};

/// The version this gateway speaks to its stdio servers, and offers a caller that asks for one it doesn't know.
const PROTOCOL_VERSION: &str = "2025-06-18";
const KNOWN_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const SETUP_TIMEOUT: Duration = Duration::from_secs(60);
/// Environment a server always gets from the gateway's own. Nothing else is passed (the gateway's keys stay).
const BASE_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "TZ",
    "SSL_CERT_FILE",
    "NIX_SSL_CERT_FILE",
];

type Reply = Result<Value, Value>;
/// A running server process and the tools it listed at startup.
type Running = (Arc<Conn>, Arc<Vec<Value>>);

pub struct Mcp {
    servers: BTreeMap<String, Server>,
}

struct Server {
    name: String,
    config: McpServer,
    /// The running process and the tools it listed at startup.
    running: Mutex<Option<Running>>,
}

/// One server process: its stdin, and the replies its stdout reader hands back by id.
struct Conn {
    stdin: Mutex<ChildStdin>,
    pending: std::sync::Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
    next_id: AtomicU64,
    alive: AtomicBool,
    _child: Child,
}

impl Mcp {
    pub fn new(servers: &HashMap<String, McpServer>) -> Mcp {
        Mcp {
            servers: servers
                .iter()
                .map(|(name, config)| {
                    let server = Server {
                        name: name.clone(),
                        config: config.clone(),
                        running: Mutex::new(None),
                    };
                    (name.clone(), server)
                })
                .collect(),
        }
    }

    /// The tools `key` may use, prefixed with their server's name. A server that fails to start is left out.
    async fn tools(&self, key: &Key) -> Vec<Value> {
        let mut out = Vec::new();
        for (name, grant) in key.mcp.iter().collect::<BTreeMap<_, _>>() {
            let Some(server) = self.servers.get(name) else {
                continue;
            };
            let tools = match server.connect().await {
                Ok((_, tools)) => tools,
                Err(e) => {
                    tracing::warn!(mcp = %name, "{e}");
                    continue;
                }
            };
            for tool in tools.iter() {
                let Some(tool_name) = tool.get("name").and_then(Value::as_str) else {
                    continue;
                };
                if granted(grant, tool_name) {
                    let mut t = tool.clone();
                    t["name"] = Value::String(format!("{name}-{tool_name}"));
                    out.push(t);
                }
            }
        }
        out
    }

    /// The server and unprefixed tool behind `prefixed`, if `key` may call it. The longest matching server name
    /// wins, so server names may themselves contain `-`.
    fn route<'a>(&'a self, key: &Key, prefixed: &'a str) -> Option<(&'a Server, &'a str)> {
        let (server, tool) = self
            .servers
            .values()
            .filter_map(|s| {
                prefixed
                    .strip_prefix(s.name.as_str())
                    .and_then(|rest| rest.strip_prefix('-'))
                    .map(|tool| (s, tool))
            })
            .max_by_key(|(s, _)| s.name.len())?;
        let grant = key.mcp.get(&server.name)?;
        granted(grant, tool).then_some((server, tool))
    }
}

fn granted(grant: &[String], tool: &str) -> bool {
    grant.iter().any(|g| g == "*" || g == tool)
}

impl Server {
    /// The running process and its tools, starting it (and handshaking) if it isn't running.
    async fn connect(&self) -> Result<Running, String> {
        let mut running = self.running.lock().await;
        if let Some((conn, tools)) = running.as_ref()
            && conn.alive.load(Ordering::Acquire)
        {
            return Ok((conn.clone(), tools.clone()));
        }
        let conn = Conn::spawn(&self.name, &self.config)?;
        let tools = Arc::new(handshake(&conn).await?);
        tracing::info!(mcp = %self.name, tools = tools.len(), "MCP server started");
        *running = Some((conn.clone(), tools.clone()));
        Ok((conn, tools))
    }

    async fn call(&self, tool: &str, arguments: Value) -> Reply {
        let (conn, _) = self.connect().await.map_err(|e| rpc_error(-32603, &e))?;
        conn.request(
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
            Duration::from_secs(self.config.timeout_secs),
        )
        .await
    }
}

/// `initialize`, `notifications/initialized`, then every page of `tools/list`.
async fn handshake(conn: &Conn) -> Result<Vec<Value>, String> {
    let hello = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {"name": "maitred", "version": env!("CARGO_PKG_VERSION")}
    });
    conn.request("initialize", hello, SETUP_TIMEOUT)
        .await
        .map_err(|e| format!("initialize failed: {e}"))?;
    conn.notify("notifications/initialized", json!({})).await?;
    let mut tools = Vec::new();
    let mut cursor: Option<Value> = None;
    loop {
        let params = match &cursor {
            Some(c) => json!({"cursor": c}),
            None => json!({}),
        };
        let page = conn
            .request("tools/list", params, SETUP_TIMEOUT)
            .await
            .map_err(|e| format!("tools/list failed: {e}"))?;
        if let Some(list) = page.get("tools").and_then(Value::as_array) {
            tools.extend(list.iter().cloned());
        }
        cursor = page.get("nextCursor").filter(|c| !c.is_null()).cloned();
        if cursor.is_none() {
            return Ok(tools);
        }
    }
}

impl Conn {
    fn spawn(name: &str, config: &McpServer) -> Result<Arc<Conn>, String> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let passed = BASE_ENV
            .iter()
            .copied()
            .chain(config.pass_env.iter().map(String::as_str));
        for var in passed {
            if let Some(v) = std::env::var_os(var) {
                cmd.env(var, v);
            }
        }
        cmd.envs(&config.env);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", config.command))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let server = name.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(mcp = %server, "{line}");
            }
        });

        let conn = Arc::new(Conn {
            stdin: Mutex::new(stdin),
            pending: std::sync::Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            _child: child,
        });
        tokio::spawn(read_replies(name.to_string(), stdout, conn.clone()));
        Ok(conn)
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Reply {
        if !self.alive.load(Ordering::Acquire) {
            return Err(rpc_error(-32603, "the MCP server has exited"));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = self.write(&msg).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(rpc_error(-32603, &e));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => Err(rpc_error(-32603, "the MCP server exited mid-call")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                let cancel = json!({"requestId": id, "reason": "timeout"});
                let _ = self.notify("notifications/cancelled", cancel).await;
                Err(rpc_error(
                    -32603,
                    &format!("the MCP server took longer than {} s", timeout.as_secs()),
                ))
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    async fn write(&self, msg: &Value) -> Result<(), String> {
        let mut line = serde_json::to_vec(msg).expect("serialize");
        line.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(&line)
            .await
            .map_err(|e| format!("write to MCP server: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("write to MCP server: {e}"))
    }
}

/// Hand each reply on the server's stdout to its waiting request; refuse the server's own requests (sampling,
/// roots, elicitation are not offered); ignore notifications. On exit, fail everything still waiting.
async fn read_replies(server: String, stdout: ChildStdout, conn: Arc<Conn>) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            tracing::debug!(mcp = %server, "non-JSON line on stdout");
            continue;
        };
        if msg.get("method").is_some() {
            if let Some(id) = msg.get("id").cloned() {
                let reply = json!({"jsonrpc": "2.0", "id": id,
                    "error": rpc_error(-32601, "not supported by maitred")});
                let _ = conn.write(&reply).await;
            }
            continue;
        }
        let Some(id) = msg.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let waiting = conn.pending.lock().unwrap().remove(&id);
        if let Some(tx) = waiting {
            let reply = match msg.get("error") {
                Some(e) => Err(e.clone()),
                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = tx.send(reply);
        }
    }
    conn.alive.store(false, Ordering::Release);
    conn.pending.lock().unwrap().clear();
    tracing::warn!(mcp = %server, "MCP server exited");
}

fn rpc_error(code: i64, message: &str) -> Value {
    json!({"code": code, "message": message})
}

fn rpc_reply(id: Value, reply: Reply) -> Response {
    let body = match reply {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
    };
    Json(body).into_response()
}

/// `POST /mcp`: one JSON-RPC message per request, answered with a JSON body (no SSE stream, no session).
pub async fn post(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Response {
    let key = match st.keys.identify(&headers) {
        Ok(k) => k,
        Err(d) => return denied(d),
    };
    if key.mcp.is_empty() {
        return error(
            StatusCode::FORBIDDEN,
            "permission_error",
            "mcp_not_allowed",
            format!("The {} key has no MCP tools.", key.name),
        );
    }
    if let Err(d) = st.keys.admit(key, chrono::Utc::now()) {
        return denied(d);
    }
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            let body = json!({"jsonrpc": "2.0", "id": null,
                "error": rpc_error(-32700, &format!("parse error: {e}"))});
            return (StatusCode::BAD_REQUEST, Json(body)).into_response();
        }
    };
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let Some(id) = msg.get("id").cloned() else {
        // A notification (initialized, cancelled) or a response: nothing to answer.
        return StatusCode::ACCEPTED.into_response();
    };
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => {
            let version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .filter(|v| KNOWN_VERSIONS.contains(v))
                .unwrap_or(PROTOCOL_VERSION);
            rpc_reply(
                id,
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "maitred", "version": env!("CARGO_PKG_VERSION")}
                })),
            )
        }
        "ping" => rpc_reply(id, Ok(json!({}))),
        "tools/list" => rpc_reply(id, Ok(json!({"tools": st.mcp.tools(key).await}))),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let Some((server, tool)) = st.mcp.route(key, name) else {
                return rpc_reply(id, Err(rpc_error(-32602, &format!("Unknown tool: {name}"))));
            };
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            let started = std::time::Instant::now();
            let reply = server.call(tool, arguments).await;
            tracing::info!(
                key = %key.name,
                tool = %name,
                ok = reply.is_ok(),
                ms = started.elapsed().as_millis() as u64,
                "mcp call"
            );
            rpc_reply(id, reply)
        }
        "resources/list" => rpc_reply(id, Ok(json!({"resources": []}))),
        "resources/templates/list" => rpc_reply(id, Ok(json!({"resourceTemplates": []}))),
        "prompts/list" => rpc_reply(id, Ok(json!({"prompts": []}))),
        _ => rpc_reply(
            id,
            Err(rpc_error(-32601, &format!("Method not found: {method}"))),
        ),
    }
}

/// `GET`/`DELETE /mcp`: there is no server-initiated stream and no session to end.
pub async fn other() -> Response {
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Keys;
    use crate::config::Config;

    /// A stdio MCP server in sh: answers initialize, lists `echo` and `secret`, and answers any call.
    const FAKE: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}},{"name":"secret","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"called"}]}}\n' "$id" ;;
  esac
done
"#;

    async fn serve() -> String {
        let config: Config = serde_json::from_value(json!({
            "listen": "127.0.0.1:0",
            "upstreams": {},
            "models": [],
            "keys": [],
            "mcp": {"fake": {"command": "sh", "args": ["-c", FAKE]}}
        }))
        .unwrap();
        let mut state = AppState::new(config).unwrap();
        let key = |name: &str, mcp: Value| -> Key {
            serde_json::from_value(json!({"name": name, "env": "", "mcp": mcp})).unwrap()
        };
        state.keys = Keys::new(vec![
            ("sk-a".into(), key("a", json!({"fake": ["echo"]}))),
            ("sk-b".into(), key("b", json!({}))),
        ]);
        let app = axum::Router::new()
            .route("/mcp", axum::routing::post(post).get(other))
            .with_state(Arc::new(state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    async fn rpc(url: &str, key: &str, body: Value) -> (u16, Value) {
        let r = reqwest::Client::new()
            .post(url)
            .bearer_auth(key)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&body).unwrap())
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        let bytes = r.bytes().await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn aggregates_filters_and_routes_tools() {
        let url = serve().await;
        let (s, r) = rpc(&url, "sk-a", json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}}))
        .await;
        assert_eq!(s, 200);
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");

        let (_, r) = rpc(
            &url,
            "sk-a",
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .await;
        let names: Vec<&str> = r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["fake-echo"], "only the granted tool, prefixed");

        let call = |id: u64, name: &str| {
            json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": {"name": name, "arguments": {"text": "hi"}}})
        };
        let (_, r) = rpc(&url, "sk-a", call(3, "fake-echo")).await;
        assert_eq!(r["result"]["content"][0]["text"], "called");
        assert_eq!(r["id"], 3);
        let (_, r) = rpc(&url, "sk-a", call(4, "fake-secret")).await;
        assert_eq!(
            r["error"]["code"], -32602,
            "a tool outside the grant is unknown"
        );

        let (s, _) = rpc(
            &url,
            "sk-a",
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await;
        assert_eq!(s, 202);
        let (s, _) = rpc(
            &url,
            "sk-b",
            json!({"jsonrpc": "2.0", "id": 5, "method": "tools/list"}),
        )
        .await;
        assert_eq!(s, 403, "a key without grants gets no /mcp");
        let (s, _) = rpc(
            &url,
            "sk-x",
            json!({"jsonrpc": "2.0", "id": 6, "method": "tools/list"}),
        )
        .await;
        assert_eq!(s, 401);
    }
}
