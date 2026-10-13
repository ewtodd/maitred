//! Reasoning-effort translation: a client's level onto the levels a model's server actually takes.
//!
//! Servers disagree on the vocabulary (Radiance's Qwen template takes none/minimal/low/medium/xhigh and
//! answers `high` or `max` with a 400; unholy's DeepSeek takes none/low/high/max). A request's level is kept
//! when the model takes it, rewritten by the model's explicit aliases when one applies, and otherwise clamped
//! to the nearest weaker level the model takes (the policy son-of-anton's own clamp uses), or the weakest level
//! above it when nothing is weaker. Unknown words are passed through for the server to judge.

use serde_json::Value;
use std::collections::HashMap;

/// The OpenAI-compatible ladder, weakest first.
pub const LADDER: &[&str] = &[
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

fn rank(level: &str) -> Option<usize> {
    LADDER.iter().position(|l| *l == level)
}

/// The level to send instead of `requested`, or `None` to leave the request as it is.
pub fn translate(
    requested: &str,
    supported: &[String],
    aliases: &HashMap<String, String>,
) -> Option<String> {
    if supported.is_empty() || supported.iter().any(|s| s == requested) {
        return None;
    }
    if let Some(to) = aliases.get(requested) {
        return Some(to.clone());
    }
    let want = rank(requested)?;
    let mut ranked: Vec<(usize, &String)> = supported
        .iter()
        .filter_map(|s| rank(s).map(|r| (r, s)))
        .collect();
    ranked.sort_by_key(|(r, _)| *r);
    let weaker = ranked.iter().rev().find(|(r, _)| *r <= want);
    let pick = weaker.or_else(|| ranked.iter().find(|(r, _)| *r > want))?;
    Some(pick.1.clone())
}

/// One rewrite applied to a request, for the log line.
#[derive(Debug, PartialEq)]
pub struct Rewrite {
    pub field: &'static str,
    pub from: String,
    pub to: String,
}

/// Translate every effort field a request carries: Chat's top-level `reasoning_effort`, Responses'
/// `reasoning.effort`, and the template variable `chat_template_kwargs.reasoning_effort`.
pub fn rewrite_request(
    body: &mut Value,
    supported: &[String],
    aliases: &HashMap<String, String>,
) -> Vec<Rewrite> {
    let mut done = Vec::new();
    let mut apply = |slot: Option<&mut Value>, field: &'static str| {
        if let Some(v) = slot
            && let Some(level) = v.as_str().map(str::to_ascii_lowercase)
            && let Some(to) = translate(&level, supported, aliases)
        {
            done.push(Rewrite {
                field,
                from: level,
                to: to.clone(),
            });
            *v = Value::String(to);
        }
    };
    apply(body.get_mut("reasoning_effort"), "reasoning_effort");
    apply(
        body.get_mut("reasoning").and_then(|r| r.get_mut("effort")),
        "reasoning.effort",
    );
    apply(
        body.get_mut("chat_template_kwargs")
            .and_then(|k| k.get_mut("reasoning_effort")),
        "chat_template_kwargs.reasoning_effort",
    );
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn qwen_template_vocabulary() {
        let qwen = s(&["none", "low", "medium", "xhigh"]);
        let none = HashMap::new();
        assert_eq!(translate("high", &qwen, &none), Some("medium".into()));
        assert_eq!(translate("max", &qwen, &none), Some("xhigh".into()));
        assert_eq!(translate("ultra", &qwen, &none), Some("xhigh".into()));
        assert_eq!(translate("minimal", &qwen, &none), Some("none".into()));
        assert_eq!(translate("xhigh", &qwen, &none), None);
        assert_eq!(translate("low", &qwen, &none), None);
    }

    #[test]
    fn deepseek_aliases_win_over_clamping() {
        let ds = s(&["none", "low", "high", "max"]);
        let aliases: HashMap<String, String> =
            [("medium", "high"), ("xhigh", "max"), ("minimal", "low")]
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect();
        assert_eq!(translate("medium", &ds, &aliases), Some("high".into()));
        assert_eq!(translate("xhigh", &ds, &aliases), Some("max".into()));
        assert_eq!(translate("minimal", &ds, &aliases), Some("low".into()));
        assert_eq!(translate("ultra", &ds, &aliases), Some("max".into()));
        assert_eq!(translate("max", &ds, &aliases), None);
    }

    #[test]
    fn nothing_weaker_takes_the_weakest_above() {
        let api = s(&["low", "high", "max"]);
        assert_eq!(translate("none", &api, &HashMap::new()), Some("low".into()));
        assert_eq!(
            translate("minimal", &api, &HashMap::new()),
            Some("low".into())
        );
    }

    #[test]
    fn unknown_words_and_plain_models_pass_through() {
        assert_eq!(
            translate("turbo", &s(&["low", "high"]), &HashMap::new()),
            None
        );
        assert_eq!(translate("high", &[], &HashMap::new()), None);
    }

    #[test]
    fn rewrites_every_effort_field() {
        let qwen = s(&["none", "low", "medium", "xhigh"]);
        let mut body = json!({
            "model": "x", "reasoning_effort": "max",
            "reasoning": {"effort": "HIGH"},
            "chat_template_kwargs": {"reasoning_effort": "high", "enable_thinking": true}
        });
        let done = rewrite_request(&mut body, &qwen, &HashMap::new());
        assert_eq!(done.len(), 3);
        assert_eq!(body["reasoning_effort"], "xhigh");
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(body["chat_template_kwargs"]["reasoning_effort"], "medium");
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
    }
}
