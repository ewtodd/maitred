//! The routed id (`auto`): a request that leaves the model to maitred.
//!
//! Every model with a `tier` is a candidate while its server answers and the conversation fits its window (by a
//! rough estimate, with a tenth kept spare). A conversation is recognised by its opening: the key, the
//! instructions, and every message up to and including the first user message. Clients only ever append, so the
//! opening stays the same turn after turn (a client that rewrites it, by compacting, has thrown the cache away
//! anyway).
//!
//! Moving a conversation to another model makes that model re-read all of it, minutes for a long agent session,
//! so the choice is cache-aware:
//!
//! - inside a tool loop (the last message is not the user's) it stays on its model;
//! - at a new user message a short conversation takes the best model on offer (its own on a tie), while a long
//!   one stays on the model that has it cached;
//! - it moves regardless when its model stops answering or it outgrows the model's window.
//!
//! The best model is the strongest with a free slot, then the strongest busy one; ties go to fewer requests in
//! flight, then config order. There is no classifier yet to say that a weaker model will do.

use crate::config::{Config, Model};
use crate::status::{self, Unavailable};
use serde_json::{Value, json};
use std::cmp::Reverse;
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Tokens counted for an image (or other inline media), whatever its encoded size.
const MEDIA_TOKENS: u64 = 1024;

/// Whether `name` is the routed id and some model has a tier to route to.
pub fn is_router(config: &Config, name: &str) -> bool {
    name == config.router.id && config.models.iter().any(|m| m.tier.is_some())
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Turn {
    /// The last message is the user's: a new turn, where the conversation may move.
    User,
    /// A tool result or anything else: the model is mid-answer and keeps the conversation.
    Continuation,
}

impl Turn {
    fn as_str(self) -> &'static str {
        match self {
            Turn::User => "user",
            Turn::Continuation => "continuation",
        }
    }
}

/// What the router reads off a request body.
#[derive(Debug)]
pub struct Shape {
    /// The conversation, by a hash of its opening; `None` for a body without a user message (a bare prompt).
    pub conversation: Option<u64>,
    pub turn: Turn,
    /// Rough prompt size: four characters a token, a flat allowance per image.
    pub tokens: u64,
    /// The last user message's text.
    pub prompt: Option<String>,
}

/// Read a Chat (`messages`), Responses (`input`, `instructions`) or Completions (`prompt`) body sent with `key`.
pub fn shape(key: &str, body: &Value) -> Shape {
    let tokens = ["messages", "input", "instructions", "prompt", "tools"]
        .iter()
        .filter_map(|f| body.get(f))
        .map(estimate)
        .sum();
    let single: [Value; 1];
    let items: &[Value] = match body.get("messages").or_else(|| body.get("input")) {
        Some(Value::Array(a)) => a,
        Some(Value::String(s)) => {
            single = [json!({"role": "user", "content": s})];
            &single
        }
        _ => &[],
    };
    fn role(m: &Value) -> Option<&str> {
        m.get("role").and_then(Value::as_str)
    }
    let conversation = items
        .iter()
        .position(|m| role(m) == Some("user"))
        .map(|first| {
            let mut h = Fnv::new();
            h.part(key.as_bytes());
            let instructions = body.get("instructions").map(Value::to_string);
            h.part(instructions.unwrap_or_default().as_bytes());
            for m in &items[..=first] {
                h.part(role(m).unwrap_or_default().as_bytes());
                let content = m.get("content").map(Value::to_string);
                h.part(content.unwrap_or_default().as_bytes());
            }
            h.0
        });
    let turn = match items.last() {
        Some(m) if role(m) != Some("user") => Turn::Continuation,
        _ => Turn::User,
    };
    let prompt = items
        .iter()
        .rev()
        .find(|m| role(m) == Some("user"))
        .and_then(|m| text(m.get("content")?));
    Shape {
        conversation,
        turn,
        tokens,
        prompt,
    }
}

/// Rough tokens in a JSON value: its strings at four characters a token, with an image (a part typed as one, or a
/// data URL) counted as `MEDIA_TOKENS` rather than by its base64 length.
fn estimate(v: &Value) -> u64 {
    match v {
        Value::String(s) if s.starts_with("data:") => MEDIA_TOKENS,
        Value::String(s) => s.len() as u64 / 4,
        Value::Array(a) => a.iter().map(estimate).sum(),
        Value::Object(o) => match o.get("type").and_then(Value::as_str) {
            Some("image_url" | "input_image" | "image") => MEDIA_TOKENS,
            _ => o.values().map(estimate).sum(),
        },
        _ => 0,
    }
}

/// A message content's text: the string, or its text parts joined.
fn text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let texts: Vec<&str> = parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

/// FNV-1a, so a conversation's id is the same from one build to the next (the decision log records it).
struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    /// Hash `bytes`, then a byte UTF-8 never contains, so that ("ab", "c") and ("a", "bc") differ.
    fn part(&mut self, bytes: &[u8]) {
        for &b in bytes.iter().chain(&[0xff]) {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

/// Requests in flight per upstream, and the model each recent conversation was sent to.
pub struct Activity {
    in_flight: Mutex<HashMap<String, u32>>,
    conversations: Mutex<HashMap<u64, (String, Instant)>>,
    forget_after: Duration,
}

impl Activity {
    pub fn new(forget_after: Duration) -> Arc<Activity> {
        Arc::new(Activity {
            in_flight: Mutex::new(HashMap::new()),
            conversations: Mutex::new(HashMap::new()),
            forget_after,
        })
    }

    /// Count a request to `upstream` until the returned guard drops, once its response has been relayed.
    pub fn enter(self: &Arc<Self>, upstream: &str) -> InFlight {
        *self
            .in_flight
            .lock()
            .unwrap()
            .entry(upstream.to_string())
            .or_insert(0) += 1;
        InFlight {
            activity: self.clone(),
            upstream: upstream.to_string(),
        }
    }

    pub fn in_flight(&self, upstream: &str) -> u32 {
        self.in_flight
            .lock()
            .unwrap()
            .get(upstream)
            .copied()
            .unwrap_or(0)
    }

    /// The model `conversation` was last sent to, unless it has been quiet for longer than `forget_after`.
    fn last_model(&self, conversation: u64) -> Option<String> {
        let conversations = self.conversations.lock().unwrap();
        let (model, at) = conversations.get(&conversation)?;
        (at.elapsed() < self.forget_after).then(|| model.clone())
    }

    fn record(&self, conversation: u64, model: &str) {
        let mut conversations = self.conversations.lock().unwrap();
        conversations.retain(|_, (_, at)| at.elapsed() < self.forget_after);
        conversations.insert(conversation, (model.to_string(), Instant::now()));
    }
}

/// One request in flight; dropping it ends the count.
pub struct InFlight {
    activity: Arc<Activity>,
    upstream: String,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Some(n) = self
            .activity
            .in_flight
            .lock()
            .unwrap()
            .get_mut(&self.upstream)
        {
            *n = n.saturating_sub(1);
        }
    }
}

/// A model the router may pick for a request: its server answers and the conversation fits it.
#[derive(Debug)]
pub struct Candidate<'a> {
    pub model: &'a Model,
    pub in_flight: u32,
    /// Every slot of its server is taken.
    pub full: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reason {
    /// A conversation the router has not seen, or has forgotten.
    New,
    /// A new user message on a conversation short enough to move, whose model is still the best on offer.
    Stay,
    /// A new user message on a conversation short enough to move, and another model is better now.
    Moved,
    /// Mid tool loop: the model that began the answer finishes it.
    ToolLoop,
    /// A new user message on a conversation too long to move cheaply: it stays where it is cached.
    Cached,
    /// Its model stopped answering, or the conversation outgrew it.
    PreviousGone,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::New => "new",
            Reason::Stay => "stay",
            Reason::Moved => "moved",
            Reason::ToolLoop => "tool-loop",
            Reason::Cached => "cached",
            Reason::PreviousGone => "previous-gone",
        }
    }
}

/// Pick among `candidates` for a request at `turn`, of about `tokens`, in a conversation last sent to `previous`.
pub fn choose<'a>(
    candidates: &[Candidate<'a>],
    previous: Option<&str>,
    turn: Turn,
    tokens: u64,
    move_below: u64,
) -> Option<(&'a Model, Reason)> {
    // The strongest with a free slot, then the strongest busy one; on a tie `prefer`, then fewer in flight, then
    // config order.
    let best = |prefer: Option<&str>| {
        candidates
            .iter()
            .enumerate()
            .min_by_key(|(i, c)| {
                (
                    c.full,
                    Reverse(c.model.tier),
                    Some(c.model.id.as_str()) != prefer,
                    c.in_flight,
                    *i,
                )
            })
            .map(|(_, c)| c.model)
    };
    let Some(previous) = previous else {
        return best(None).map(|m| (m, Reason::New));
    };
    let Some(current) = candidates.iter().find(|c| c.model.id == previous) else {
        return best(None).map(|m| (m, Reason::PreviousGone));
    };
    if turn == Turn::Continuation {
        return Some((current.model, Reason::ToolLoop));
    }
    if tokens < move_below {
        let m = best(Some(previous))?;
        let reason = if m.id == previous {
            Reason::Stay
        } else {
            Reason::Moved
        };
        return Some((m, reason));
    }
    Some((current.model, Reason::Cached))
}

/// Whether a conversation of about `tokens` fits `m`'s window, a tenth kept spare for the estimate's error and the
/// answer.
fn fits(m: &Model, tokens: u64) -> bool {
    m.context_length.is_none_or(|c| tokens <= c - c / 10)
}

/// A routed request's model, and why.
pub struct Decision<'a> {
    pub model: &'a Model,
    pub reason: Reason,
    pub previous: Option<String>,
    /// What it would have picked with every routed model serving and free.
    pub ideal: Option<&'a Model>,
    /// The models it picked among.
    pub candidates: Vec<&'a str>,
}

/// Why no model could be picked.
pub enum NoRoute {
    /// Nothing that fits the conversation is serving (503).
    Unavailable(Unavailable),
    /// The conversation is longer than every routed model's window (400).
    TooLong(String),
}

/// Pick the model for a request to the routed id, given which upstreams answer (`up`), and remember it for the
/// conversation.
pub fn route<'a>(
    config: &'a Config,
    up: &HashMap<String, bool>,
    activity: &Activity,
    shape: &Shape,
) -> Result<Decision<'a>, NoRoute> {
    let is_up = |u: &str| up.get(u).copied().unwrap_or(false);
    let fitting: Vec<&Model> = config
        .models
        .iter()
        .filter(|m| m.tier.is_some() && fits(m, shape.tokens))
        .collect();
    let candidates: Vec<Candidate> = fitting
        .iter()
        .filter(|m| is_up(&m.upstream))
        .map(|m| {
            let in_flight = activity.in_flight(&m.upstream);
            let slots = config.upstreams[&m.upstream].slots;
            Candidate {
                model: m,
                in_flight,
                full: slots.is_some_and(|s| in_flight >= s),
            }
        })
        .collect();
    let ideal = fitting.iter().copied().min_by_key(|m| Reverse(m.tier));
    let previous = shape.conversation.and_then(|c| activity.last_model(c));
    let Some((model, reason)) = choose(
        &candidates,
        previous.as_deref(),
        shape.turn,
        shape.tokens,
        config.router.move_below_tokens,
    ) else {
        return Err(no_route(config, up, shape.tokens, ideal));
    };
    if let Some(c) = shape.conversation {
        activity.record(c, &model.id);
    }
    Ok(Decision {
        model,
        reason,
        previous,
        ideal,
        candidates: candidates.iter().map(|c| c.model.id.as_str()).collect(),
    })
}

fn no_route(
    config: &Config,
    up: &HashMap<String, bool>,
    tokens: u64,
    ideal: Option<&Model>,
) -> NoRoute {
    let id = &config.router.id;
    let Some(ideal) = ideal else {
        let largest = config
            .models
            .iter()
            .filter(|m| m.tier.is_some())
            .filter_map(|m| m.context_length)
            .max()
            .unwrap_or(0);
        return NoRoute::TooLong(format!(
            "This conversation (about {tokens} tokens) is too long for every model {id} routes to: the largest \
             window is {largest} tokens, a tenth of it kept for the answer."
        ));
    };
    let serving = config
        .models
        .iter()
        .any(|m| m.tier.is_some() && up.get(&m.upstream).copied().unwrap_or(false));
    if !serving {
        return NoRoute::Unavailable(Unavailable {
            message: format!(
                "{id} has nothing to serve: none of its models is answering right now (they may be switching \
                 or restarting). Try again in a few minutes."
            ),
            retry_after: Some(120),
        });
    }
    let then = match config
        .modes
        .iter()
        .find(|m| m.upstreams.contains(&ideal.upstream))
    {
        Some(mode) => status::hint(config, &mode.label),
        None => "try again later".to_string(),
    };
    NoRoute::Unavailable(Unavailable {
        message: format!(
            "This conversation (about {tokens} tokens) is too long for the models {id} has serving right now. \
             {} ({}) would take it but is not serving; {then}.",
            ideal.display(),
            ideal.id
        ),
        retry_after: None,
    })
}

/// The decision log: one JSON line per routed request.
pub struct DecisionLog {
    file: Mutex<std::fs::File>,
    prompts: bool,
}

impl DecisionLog {
    pub fn open(path: &str, prompts: bool) -> Result<DecisionLog, String> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("decision log {path}: {e}"))?;
        Ok(DecisionLog {
            file: Mutex::new(file),
            prompts,
        })
    }

    pub fn write(&self, key: &str, shape: &Shape, d: &Decision) {
        let mut line = json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "key": key,
            "conversation": shape.conversation.map(|c| format!("{c:016x}")),
            "turn": shape.turn.as_str(),
            "tokens": shape.tokens,
            "previous": d.previous,
            "model": d.model.id,
            "reason": d.reason.as_str(),
            "ideal": d.ideal.map(|m| &m.id),
            "candidates": d.candidates,
        });
        if self.prompts
            && let Some(p) = &shape.prompt
        {
            line["prompt"] = json!(p);
        }
        let mut text = line.to_string();
        text.push('\n');
        if let Err(e) = self.file.lock().unwrap().write_all(text.as_bytes()) {
            tracing::warn!("decision log: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, tier: u32) -> Model {
        serde_json::from_value(
            json!({"id": id, "upstream": id, "upstream_model": id, "tier": tier}),
        )
        .unwrap()
    }

    fn cand(m: &Model, in_flight: u32, full: bool) -> Candidate<'_> {
        Candidate {
            model: m,
            in_flight,
            full,
        }
    }

    #[test]
    fn a_conversation_keeps_its_id_as_it_grows() {
        let first = json!({"messages": [
            {"role": "system", "content": "be brief"},
            {"role": "user", "content": "hi"}
        ]});
        let later = json!({"messages": [
            {"role": "system", "content": "be brief"},
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": "and again"}
        ]});
        let a = shape("k", &first);
        let b = shape("k", &later);
        assert!(a.conversation.is_some());
        assert_eq!(a.conversation, b.conversation);
        assert_eq!(b.prompt.as_deref(), Some("and again"));
        assert_ne!(
            a.conversation,
            shape("other key", &first).conversation,
            "keys never share a conversation"
        );
        let elsewhere = json!({"messages": [
            {"role": "system", "content": "be verbose"},
            {"role": "user", "content": "hi"}
        ]});
        assert_ne!(a.conversation, shape("k", &elsewhere).conversation);
    }

    #[test]
    fn a_tool_result_continues_and_a_user_message_is_a_new_turn() {
        let tool = json!({"messages": [
            {"role": "user", "content": "list files"},
            {"role": "assistant", "tool_calls": [{"id": "1", "type": "function", "function": {"name": "ls", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "1", "content": "a b c"}
        ]});
        assert_eq!(shape("k", &tool).turn, Turn::Continuation);
        let responses = json!({"input": [
            {"role": "user", "content": "list files"},
            {"type": "function_call_output", "call_id": "1", "output": "a b c"}
        ]});
        assert_eq!(shape("k", &responses).turn, Turn::Continuation);
        let plain = shape(
            "k",
            &json!({"input": "hello there", "instructions": "be brief"}),
        );
        assert_eq!(plain.turn, Turn::User);
        assert!(plain.conversation.is_some());
        assert_eq!(plain.prompt.as_deref(), Some("hello there"));
        let bare = shape("k", &json!({"prompt": "once upon a time"}));
        assert_eq!(bare.conversation, None);
    }

    #[test]
    fn images_count_as_a_flat_allowance_not_their_base64() {
        let photo = format!("data:image/png;base64,{}", "A".repeat(4_000_000));
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "what is this?"},
            {"type": "image_url", "image_url": {"url": photo}}
        ]}]});
        let s = shape("k", &body);
        // The image, plus a few tokens of text, role and part types.
        assert!(
            (MEDIA_TOKENS..MEDIA_TOKENS + 10).contains(&s.tokens),
            "{}",
            s.tokens
        );
        assert_eq!(s.prompt.as_deref(), Some("what is this?"));
    }

    #[test]
    fn a_new_conversation_takes_the_strongest_free_model() {
        let (big, mid_a, mid_b) = (model("big", 3), model("mid-a", 2), model("mid-b", 2));
        fn pick<'a>(c: &[Candidate<'a>]) -> &'a Model {
            choose(c, None, Turn::User, 10, 100).unwrap().0
        }
        assert_eq!(
            pick(&[cand(&mid_a, 0, false), cand(&big, 5, false)]).id,
            "big"
        );
        // A full model is passed over for a free one, however weaker.
        assert_eq!(
            pick(&[cand(&big, 1, true), cand(&mid_a, 3, false)]).id,
            "mid-a"
        );
        // Within a tier, fewer in flight wins; then config order.
        assert_eq!(
            pick(&[cand(&mid_a, 2, false), cand(&mid_b, 1, false)]).id,
            "mid-b"
        );
        assert_eq!(
            pick(&[cand(&mid_a, 1, false), cand(&mid_b, 1, false)]).id,
            "mid-a"
        );
        // Everything full: the strongest, to queue there.
        assert_eq!(
            pick(&[cand(&mid_a, 1, true), cand(&big, 1, true)]).id,
            "big"
        );
        assert!(choose(&[], None, Turn::User, 10, 100).is_none());
    }

    #[test]
    fn a_conversation_moves_only_when_it_is_cheap_or_it_has_to() {
        let (big, mid_a, mid_b) = (model("big", 3), model("mid-a", 2), model("mid-b", 2));
        let all = [
            cand(&big, 0, false),
            cand(&mid_a, 3, false),
            cand(&mid_b, 0, false),
        ];
        let go = |prev, turn, tokens| {
            let (m, r) = choose(&all, Some(prev), turn, tokens, 100).unwrap();
            (m.id.as_str(), r)
        };
        // Short: it takes the best on offer.
        assert_eq!(go("mid-a", Turn::User, 10), ("big", Reason::Moved));
        // Long: it stays where it is cached, though a stronger model is free.
        assert_eq!(go("mid-a", Turn::User, 1000), ("mid-a", Reason::Cached));
        // Mid tool loop: it stays, however short.
        assert_eq!(
            go("mid-a", Turn::Continuation, 10),
            ("mid-a", Reason::ToolLoop)
        );
        // Its model is gone (not serving, or outgrown): the best on offer.
        assert_eq!(
            go("elsewhere", Turn::User, 1000),
            ("big", Reason::PreviousGone)
        );

        // Short, on a tie it keeps its own model even with more in flight than the other.
        let tie = [cand(&mid_a, 3, false), cand(&mid_b, 0, false)];
        let (m, r) = choose(&tie, Some("mid-a"), Turn::User, 10, 100).unwrap();
        assert_eq!((m.id.as_str(), r), ("mid-a", Reason::Stay));
        // ...unless its model is full.
        let tie = [cand(&mid_a, 1, true), cand(&mid_b, 0, false)];
        let (m, r) = choose(&tie, Some("mid-a"), Turn::User, 10, 100).unwrap();
        assert_eq!((m.id.as_str(), r), ("mid-b", Reason::Moved));
    }

    #[test]
    fn in_flight_counts_until_the_guard_drops_and_quiet_conversations_are_forgotten() {
        let a = Activity::new(Duration::from_millis(50));
        let g1 = a.enter("up");
        let g2 = a.enter("up");
        assert_eq!(a.in_flight("up"), 2);
        drop(g1);
        assert_eq!(a.in_flight("up"), 1);
        drop(g2);
        assert_eq!(a.in_flight("up"), 0);
        a.record(7, "m");
        assert_eq!(a.last_model(7).as_deref(), Some("m"));
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(a.last_model(7), None);
    }
}
