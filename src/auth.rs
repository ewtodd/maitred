//! Virtual keys: who is calling, which models they may use, how often, and when.

use crate::config::{ActiveHours, Key, RateLimit};
use axum::http::HeaderMap;
use chrono::{DateTime, Timelike, Utc};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct Keys {
    keys: Vec<(String, Key)>,
    windows: Mutex<HashMap<String, (Instant, u32)>>,
}

#[derive(Debug, PartialEq)]
pub enum Denied {
    Missing,
    Unknown,
    Inactive(String),
    Model { key: String, allowed: Vec<String> },
    RateLimited { retry_after: u64 },
}

impl Keys {
    /// Resolve each key's secret from the environment; a key whose variable is unset is skipped with a warning.
    pub fn from_env(keys: &[Key]) -> Keys {
        let mut resolved = Vec::new();
        for k in keys {
            match std::env::var(&k.env) {
                Ok(v) if !v.is_empty() => resolved.push((v, k.clone())),
                _ => {
                    tracing::warn!(key = %k.name, env = %k.env, "key secret not set; key disabled")
                }
            }
        }
        Keys::new(resolved)
    }

    pub fn new(keys: Vec<(String, Key)>) -> Keys {
        Keys {
            keys,
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// The caller's key, from `Authorization: Bearer`, `x-bf-vk` (Bifrost's header) or `x-api-key`.
    pub fn identify(&self, headers: &HeaderMap) -> Result<&Key, Denied> {
        let presented = presented_secret(headers).ok_or(Denied::Missing)?;
        self.keys
            .iter()
            .find(|(secret, _)| constant_time_eq(secret.as_bytes(), presented.as_bytes()))
            .map(|(_, k)| k)
            .ok_or(Denied::Unknown)
    }

    /// Everything but the model check, at `now`: the active-hours window, then the rate limit.
    pub fn admit(&self, key: &Key, now: DateTime<Utc>) -> Result<(), Denied> {
        if let Some(h) = &key.active_hours
            && !within_hours(h, now)
        {
            return Err(Denied::Inactive(h.message.clone()));
        }
        if let Some(limit) = &key.rate_limit {
            self.take(&key.name, limit, Instant::now())?;
        }
        Ok(())
    }

    fn take(&self, name: &str, limit: &RateLimit, now: Instant) -> Result<(), Denied> {
        let period = Duration::from_secs(limit.per_secs.max(1));
        let mut windows = self.windows.lock().unwrap();
        let w = windows.entry(name.to_string()).or_insert((now, 0));
        if now.duration_since(w.0) >= period {
            *w = (now, 0);
        }
        if w.1 >= limit.requests {
            let left = period.saturating_sub(now.duration_since(w.0));
            return Err(Denied::RateLimited {
                retry_after: left.as_secs().max(1),
            });
        }
        w.1 += 1;
        Ok(())
    }
}

pub fn may_use(key: &Key, model_id: &str) -> bool {
    key.models.iter().any(|m| m == "*" || m == model_id)
}

fn presented_secret(headers: &HeaderMap) -> Option<String> {
    let get = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    if let Some(auth) = get("authorization") {
        let token = auth
            .strip_prefix("Bearer ")
            .or_else(|| auth.strip_prefix("bearer "))
            .unwrap_or(auth);
        return Some(token.trim().to_string());
    }
    get("x-bf-vk")
        .or_else(|| get("x-api-key"))
        .map(str::to_string)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn within_hours(h: &ActiveHours, now: DateTime<Utc>) -> bool {
    let tz: chrono_tz::Tz = h.timezone.parse().unwrap_or(chrono_tz::UTC);
    let hour = now.with_timezone(&tz).hour();
    if h.start == h.end {
        true
    } else if h.start < h.end {
        hour >= h.start && hour < h.end
    } else {
        hour >= h.start || hour < h.end
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn key(name: &str) -> Key {
        Key {
            name: name.into(),
            env: String::new(),
            models: vec!["*".into()],
            rate_limit: None,
            active_hours: None,
            mcp: HashMap::new(),
        }
    }

    #[test]
    fn identifies_bearer_and_bifrost_headers() {
        let keys = Keys::new(vec![("sk-a".into(), key("a")), ("sk-b".into(), key("b"))]);
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer sk-b".parse().unwrap());
        assert_eq!(keys.identify(&h).unwrap().name, "b");
        let mut h = HeaderMap::new();
        h.insert("x-bf-vk", "sk-a".parse().unwrap());
        assert_eq!(keys.identify(&h).unwrap().name, "a");
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer nope".parse().unwrap());
        assert_eq!(keys.identify(&h).unwrap_err(), Denied::Unknown);
        assert_eq!(
            keys.identify(&HeaderMap::new()).unwrap_err(),
            Denied::Missing
        );
    }

    #[test]
    fn active_hours_wrap_midnight_in_the_keys_zone() {
        let h = ActiveHours {
            start: 20,
            end: 7,
            timezone: "America/Chicago".into(),
            message: "off".into(),
        };
        // 2026-10-13 is CDT (UTC-5).
        let at = |utc_h| Utc.with_ymd_and_hms(2026, 10, 13, utc_h, 0, 0).unwrap();
        assert!(within_hours(&h, at(2))); // 21:00 CDT
        assert!(within_hours(&h, at(11))); // 06:00 CDT
        assert!(!within_hours(&h, at(12))); // 07:00 CDT
        assert!(!within_hours(&h, at(20))); // 15:00 CDT
    }

    #[test]
    fn rate_limit_is_a_fixed_window() {
        let keys = Keys::new(vec![]);
        let limit = RateLimit {
            requests: 2,
            per_secs: 60,
        };
        let t0 = Instant::now();
        assert!(keys.take("f", &limit, t0).is_ok());
        assert!(keys.take("f", &limit, t0).is_ok());
        assert!(matches!(
            keys.take("f", &limit, t0 + Duration::from_secs(10)),
            Err(Denied::RateLimited { retry_after: 50 })
        ));
        assert!(keys.take("f", &limit, t0 + Duration::from_secs(61)).is_ok());
    }
}
