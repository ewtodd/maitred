//! The request log behind `GET /status`: a ring of the most recent requests, updated as each one streams, and
//! per-key counters. Recording is a side effect of serving a request and never changes what is served.

use bytes::Bytes;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// How many requests the ring keeps.
pub const RING: usize = 512;
/// How much of the end of a relayed body is kept to look for `usage`.
const TAIL: usize = 16 * 1024;
/// The error of a request whose client went away before the body ended.
pub const CLIENT_CLOSED: &str = "client closed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
}

#[derive(Deserialize)]
struct RawUsage {
    #[serde(alias = "input_tokens")]
    prompt_tokens: Option<u64>,
    #[serde(alias = "output_tokens")]
    completion_tokens: Option<u64>,
    #[serde(alias = "input_tokens_details")]
    prompt_tokens_details: Option<RawDetails>,
}

#[derive(Deserialize)]
struct RawDetails {
    cached_tokens: Option<u64>,
}

/// The last `usage` object in `tail` (the end of a response body, JSON or SSE), if there is one.
pub fn parse_usage(tail: &[u8]) -> Option<Usage> {
    const KEY: &[u8] = b"\"usage\"";
    let mut end = tail.len();
    while let Some(at) = tail[..end].windows(KEY.len()).rposition(|w| w == KEY) {
        end = at;
        // A quoted `"usage"` inside a string is escaped: skip it.
        if at > 0 && tail[at - 1] == b'\\' {
            continue;
        }
        let mut rest = &tail[at + KEY.len()..];
        rest = rest.trim_ascii_start();
        let Some(r) = rest.strip_prefix(b":") else {
            continue;
        };
        let mut values = serde_json::Deserializer::from_slice(r.trim_ascii_start())
            .into_iter::<Option<RawUsage>>();
        if let Some(Ok(Some(u))) = values.next()
            && (u.prompt_tokens.is_some() || u.completion_tokens.is_some())
        {
            return Some(Usage {
                prompt_tokens: u.prompt_tokens.unwrap_or(0),
                completion_tokens: u.completion_tokens.unwrap_or(0),
                cached_tokens: u
                    .prompt_tokens_details
                    .and_then(|d| d.cached_tokens)
                    .unwrap_or(0),
            });
        }
    }
    None
}

struct Entry {
    id: u64,
    start: f64,
    key: String,
    requested: String,
    model: String,
    upstream: String,
    route: String,
    path: String,
    status: u16,
    headers_ms: Option<u64>,
    first_chunk_ms: Option<u64>,
    first_token_ms: Option<u64>,
    duration_ms: Option<u64>,
    done: bool,
    error: String,
    usage: Option<Usage>,
}

impl Entry {
    fn json(&self) -> Value {
        json!({
            "id": self.id,
            "start": self.start,
            "key": self.key,
            "requested": self.requested,
            "model": self.model,
            "upstream": self.upstream,
            "route": self.route,
            "path": self.path,
            "status": self.status,
            "headers_ms": self.headers_ms,
            "first_chunk_ms": self.first_chunk_ms,
            "first_token_ms": self.first_token_ms,
            "duration_ms": self.duration_ms,
            "done": self.done,
            "error": self.error,
            "usage": self.usage.map(|u| json!({
                "prompt_tokens": u.prompt_tokens,
                "completion_tokens": u.completion_tokens,
                "cached_tokens": u.cached_tokens,
            })),
        })
    }
}

#[derive(Default)]
struct KeyStats {
    in_flight: u64,
    requests: u64,
    failures: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_tokens: u64,
    last: Option<f64>,
}

#[derive(Default)]
struct Inner {
    ring: VecDeque<Entry>,
    next_id: u64,
    keys: HashMap<String, KeyStats>,
}

impl Inner {
    fn entry(&mut self, id: u64) -> Option<&mut Entry> {
        let first = self.ring.front()?.id;
        let i = usize::try_from(id.checked_sub(first)?).ok()?;
        self.ring.get_mut(i)
    }
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn millis(t: Instant) -> u64 {
    t.elapsed().as_millis() as u64
}

pub struct RequestLog {
    started: f64,
    inner: Mutex<Inner>,
}

impl RequestLog {
    pub fn new() -> Arc<RequestLog> {
        Arc::new(RequestLog {
            started: unix_now(),
            inner: Mutex::new(Inner::default()),
        })
    }

    /// Record a request from `key` that arrived at `arrived`. The returned guard follows it to its end; dropping
    /// it unfinished records a client that went away.
    pub fn begin(self: &Arc<Self>, key: &str, path: &str, arrived: Instant) -> Rec {
        let start = unix_now();
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_id;
        inner.next_id += 1;
        if inner.ring.len() >= RING {
            inner.ring.pop_front();
        }
        inner.ring.push_back(Entry {
            id,
            start,
            key: key.to_string(),
            requested: String::new(),
            model: String::new(),
            upstream: String::new(),
            route: String::new(),
            path: path.to_string(),
            status: 0,
            headers_ms: None,
            first_chunk_ms: None,
            first_token_ms: None,
            duration_ms: None,
            done: false,
            error: String::new(),
            usage: None,
        });
        let stats = inner.keys.entry(key.to_string()).or_default();
        stats.in_flight += 1;
        stats.requests += 1;
        stats.last = Some(start);
        drop(inner);
        Rec {
            log: self.clone(),
            id,
            key: key.to_string(),
            arrived,
            status: AtomicU16::new(0),
            finished: false,
            first_chunk: false,
            first_token: false,
            tail: Vec::new(),
        }
    }

    pub fn started(&self) -> f64 {
        self.started
    }

    /// The ring newest first, and the counters of `keys` in that order.
    pub fn snapshot(&self, keys: &[&str]) -> (Vec<Value>, Vec<Value>) {
        let inner = self.inner.lock().unwrap();
        let recent = inner.ring.iter().rev().map(Entry::json).collect();
        let none = KeyStats::default();
        let keys = keys
            .iter()
            .map(|name| {
                let s = inner.keys.get(*name).unwrap_or(&none);
                json!({
                    "name": name,
                    "in_flight": s.in_flight,
                    "requests": s.requests,
                    "failures": s.failures,
                    "prompt_tokens": s.prompt_tokens,
                    "completion_tokens": s.completion_tokens,
                    "cached_tokens": s.cached_tokens,
                    "last": s.last,
                })
            })
            .collect();
        (recent, keys)
    }
}

/// One recorded request, followed by whoever is serving it.
pub struct Rec {
    log: Arc<RequestLog>,
    id: u64,
    key: String,
    arrived: Instant,
    status: AtomicU16,
    finished: bool,
    first_chunk: bool,
    first_token: bool,
    tail: Vec<u8>,
}

impl Rec {
    fn update(&self, f: impl FnOnce(&mut Entry)) {
        if let Some(e) = self.log.inner.lock().unwrap().entry(self.id) {
            f(e);
        }
    }

    /// The model the client asked for; until one is picked it is also the model of the entry.
    pub fn requested(&self, name: &str) {
        self.update(|e| {
            e.requested = name.to_string();
            e.model = name.to_string();
        });
    }

    /// The model and upstream the request goes to (the last pick wins), and the router's reason.
    pub fn picked(&self, model: &str, upstream: &str, route: &str) {
        self.update(|e| {
            e.model = model.to_string();
            e.upstream = upstream.to_string();
            e.route = route.to_string();
        });
    }

    /// The upstream answered with `status`.
    pub fn headers(&self, status: u16) {
        self.status.store(status, Ordering::Relaxed);
        let ms = millis(self.arrived);
        self.update(|e| {
            e.status = status;
            e.headers_ms = Some(ms);
        });
    }

    /// A chunk of the body is being relayed to the client.
    pub fn chunk(&mut self, chunk: &Bytes) {
        if !self.first_chunk {
            self.first_chunk = true;
            let ms = millis(self.arrived);
            self.update(|e| e.first_chunk_ms = Some(ms));
        }
        if !self.first_token && carries_token(chunk) {
            self.first_token = true;
            let ms = millis(self.arrived);
            self.update(|e| e.first_token_ms = Some(ms));
        }
        // Keep the end of the body: trim in batches, not on every chunk.
        if chunk.len() >= TAIL {
            self.tail.clear();
            self.tail.extend_from_slice(&chunk[chunk.len() - TAIL..]);
        } else {
            self.tail.extend_from_slice(chunk);
            if self.tail.len() > 2 * TAIL {
                self.tail.drain(..self.tail.len() - TAIL);
            }
        }
    }

    /// maitred's own answer: `status` with the error `code`.
    pub fn rejected(&mut self, status: u16, code: &str) {
        self.status.store(status, Ordering::Relaxed);
        self.finish_with(Some(status), code);
    }

    /// The response is over; `error` is empty on success.
    pub fn finish(&mut self, error: &str) {
        self.finish_with(None, error);
    }

    fn finish_with(&mut self, status: Option<u16>, error: &str) {
        if std::mem::replace(&mut self.finished, true) {
            return;
        }
        let start = self.tail.len().saturating_sub(TAIL);
        let usage = parse_usage(&self.tail[start..]);
        let ms = millis(self.arrived);
        let failed = self.status.load(Ordering::Relaxed) >= 400 || !error.is_empty();
        let mut inner = self.log.inner.lock().unwrap();
        if let Some(e) = inner.entry(self.id) {
            if let Some(s) = status {
                e.status = s;
            }
            e.duration_ms = Some(ms);
            e.done = true;
            e.error = error.to_string();
            e.usage = usage;
        }
        let stats = inner.keys.entry(self.key.clone()).or_default();
        stats.in_flight = stats.in_flight.saturating_sub(1);
        if failed {
            stats.failures += 1;
        }
        if let Some(u) = usage {
            stats.prompt_tokens += u.prompt_tokens;
            stats.completion_tokens += u.completion_tokens;
            stats.cached_tokens += u.cached_tokens;
        }
    }
}

impl Drop for Rec {
    fn drop(&mut self) {
        self.finish(CLIENT_CLOSED);
    }
}

/// Whether a relayed chunk carries generated output: a non-empty `content`, `reasoning_content`, `reasoning`,
/// `text` or Responses `delta` string, or a non-empty `tool_calls` array. Servers send an empty role chunk as
/// soon as they accept a stream, so the first chunk says nothing about the time to the first token. A pattern
/// split across two chunks is seen one chunk late.
fn carries_token(chunk: &[u8]) -> bool {
    const STRINGS: [&[u8]; 5] = [
        b"\"content\":",
        b"\"reasoning_content\":",
        b"\"reasoning\":",
        b"\"text\":",
        b"\"delta\":",
    ];
    let after = |needle: &[u8], at: usize| -> &[u8] {
        let rest = &chunk[at + needle.len()..];
        let skip = rest.iter().take_while(|b| b.is_ascii_whitespace()).count();
        &rest[skip..]
    };
    let found = |needle: &[u8], open: u8, close: u8| {
        chunk.windows(needle.len()).enumerate().any(|(i, w)| {
            w == needle && {
                let v = after(needle, i);
                v.len() >= 2 && v[0] == open && v[1] != close
            }
        })
    };
    STRINGS.iter().any(|k| found(k, b'"', b'"')) || found(b"\"tool_calls\":", b'[', b']')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(p: u64, c: u64, cached: u64) -> Option<Usage> {
        Some(Usage {
            prompt_tokens: p,
            completion_tokens: c,
            cached_tokens: cached,
        })
    }

    #[test]
    fn usage_in_the_final_sse_chunk() {
        let body = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\"usage\":null}\n\n\
            data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":7,\"total_tokens\":107,\
            \"prompt_tokens_details\":{\"cached_tokens\":64}}}\n\ndata: [DONE]\n\n";
        assert_eq!(parse_usage(body), usage(100, 7, 64));
    }

    #[test]
    fn usage_after_choices_in_a_json_body() {
        let body = br#"{"id":"x","choices":[{"message":{"content":"a"}}],"usage": {"prompt_tokens": 5, "completion_tokens": 2}}"#;
        assert_eq!(parse_usage(body), usage(5, 2, 0));
    }

    #[test]
    fn no_usage_or_a_null_one_is_none() {
        assert_eq!(parse_usage(br#"{"choices":[]}"#), None);
        assert_eq!(parse_usage(br#"{"choices":[],"usage": null}"#), None);
        assert_eq!(parse_usage(b""), None);
    }

    #[test]
    fn usage_split_across_two_chunks() {
        let a = b"data: {\"usage\":{\"prompt_tokens\":11,\"comple";
        let b = b"tion_tokens\":3,\"prompt_tokens_details\":{\"cached_tokens\":4}}}\n\n";
        let log = RequestLog::new();
        let mut rec = log.begin("k", "chat/completions", Instant::now());
        rec.chunk(&Bytes::from_static(a));
        rec.chunk(&Bytes::from_static(b));
        rec.finish("");
        let (recent, keys) = log.snapshot(&["k"]);
        assert_eq!(recent[0]["usage"]["prompt_tokens"], 11);
        assert_eq!(recent[0]["usage"]["cached_tokens"], 4);
        assert_eq!(keys[0]["completion_tokens"], 3);
    }

    #[test]
    fn a_body_longer_than_the_tail_still_yields_its_final_usage() {
        let log = RequestLog::new();
        let mut rec = log.begin("k", "chat/completions", Instant::now());
        for _ in 0..20 {
            rec.chunk(&Bytes::from(vec![b'x'; 5000]));
        }
        rec.chunk(&Bytes::from_static(
            br#"{"usage":{"prompt_tokens":9,"completion_tokens":1}}"#,
        ));
        rec.finish("");
        assert!(rec.tail.len() <= 2 * TAIL);
        let (recent, _) = log.snapshot(&["k"]);
        assert_eq!(recent[0]["usage"]["prompt_tokens"], 9);
    }

    #[test]
    fn garbage_and_escaped_mentions_are_not_usage() {
        assert_eq!(parse_usage(b"\"usage\" \"usage\": {{{ nope"), None);
        assert_eq!(parse_usage(b"\"usage\": 12"), None);
        assert_eq!(parse_usage(&[0xff, 0xfe, b'"', b'u']), None);
        let escaped = br#"{"content":"the \"usage\": {\"prompt_tokens\": 3}"}"#;
        assert_eq!(parse_usage(escaped), None);
        // An earlier real usage survives a later malformed mention.
        let body = br#"{"usage":{"prompt_tokens":2,"completion_tokens":1}} "usage": ["#;
        assert_eq!(parse_usage(body), usage(2, 1, 0));
    }

    #[test]
    fn the_ring_keeps_the_newest_512_newest_first() {
        let log = RequestLog::new();
        for _ in 0..RING + 10 {
            let mut rec = log.begin("k", "chat/completions", Instant::now());
            rec.finish("");
        }
        let (recent, keys) = log.snapshot(&["k"]);
        assert_eq!(recent.len(), RING);
        assert_eq!(recent[0]["id"], RING as u64 + 9);
        assert_eq!(recent[RING - 1]["id"], 10);
        assert_eq!(keys[0]["requests"], RING as u64 + 10);
        assert_eq!(keys[0]["in_flight"], 0);
    }

    #[test]
    fn an_evicted_request_still_settles_its_key() {
        let log = RequestLog::new();
        let mut first = log.begin("k", "p", Instant::now());
        for _ in 0..RING {
            log.begin("k", "p", Instant::now()).finish("");
        }
        first.headers(500);
        first.finish("");
        let (_, keys) = log.snapshot(&["k"]);
        assert_eq!(keys[0]["in_flight"], 0);
        assert_eq!(keys[0]["failures"], 1);
    }

    #[test]
    fn carries_token_only_for_generated_output() {
        let yes: [&[u8]; 8] = [
            br#"data: {"choices":[{"delta":{"content":"Hi"}}]}"#,
            br#"data: {"choices":[{"delta":{"reasoning_content":"Let"}}]}"#,
            br#"data: {"choices":[{"delta":{"reasoning":"Let"}}]}"#,
            br#"data: {"choices":[{"delta": {"content": "x"}}]}"#,
            br#"data: {"choices":[{"delta":{"content":"\"q"}}]}"#,
            br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0}]}}]}"#,
            br#"data: {"choices":[{"text":"a","index":0}]}"#,
            br#"data: {"type":"response.output_text.delta","delta":"Hi"}"#,
        ];
        for c in yes {
            assert!(carries_token(c), "{}", String::from_utf8_lossy(c));
        }
        let no: [&[u8]; 6] = [
            br#"data: {"choices":[{"delta":{"role":"assistant","content":""}}]}"#,
            br#"data: {"choices":[{"delta":{"content":null}}]}"#,
            br#"data: {"choices":[{"delta":{"tool_calls":[]}}]}"#,
            br#"data: {"choices":[],"usage":{"prompt_tokens":3}}"#,
            b"data: [DONE]",
            br#"data: {"choices":[{"delta":{"content":"#,
        ];
        for c in no {
            assert!(!carries_token(c), "{}", String::from_utf8_lossy(c));
        }
    }
}
