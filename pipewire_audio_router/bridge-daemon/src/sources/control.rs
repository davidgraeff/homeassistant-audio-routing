//! Per-source **control endpoints** — where a source's producer accepts transport
//! commands, for a consumer that wants to drive it rather than just display it.
//!
//! Now-playing ([`super::now_playing`]) answers "what is this source playing". This
//! answers the other half: "who can I ask to pause it". A remote producer that has
//! its own control surface — the YouTube Music receiver's, for instance — announces
//! the URL of it here, keyed by the RTP port it sends to, exactly the way it reports
//! metadata. Home Assistant reads the announcement off `GET /api/sources` and talks to
//! that URL directly.
//!
//! ## Deliberately a rendezvous, not a relay
//! The daemon stores *where* the control surface is and which features it claims, and
//! nothing else: it never forwards a command and never parses one. So the router stays
//! ignorant of what the producer is (the same rule as for now-playing), and there is no
//! request path through this process that a LAN caller could aim somewhere else.
//!
//! ## Why it is not part of the now-playing entry, or of the source config
//! * A now-playing entry is *cleared* when playback stops — and "start something while
//!   nothing is playing" is precisely when a consumer needs the endpoint most.
//! * The persisted source config is rewritten whole by the web UI's edit form, which
//!   knows nothing about a field like this; and the endpoint is a property of a running
//!   producer, not of the router's configuration.
//!
//! So it is its own in-memory table with a [`TTL`], refreshed by the producer's
//! heartbeat and withdrawn explicitly when it shuts down. Lost on a daemon restart by
//! design: the next heartbeat (well inside the TTL) re-announces it.

use crate::util::locks::LockRecover;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long an announcement stays valid without a refresh. Three heartbeats of a
/// producer that announces every 60 s, so one lost request never blinks the entity.
pub const TTL: Duration = Duration::from_secs(180);
/// Bounds on what a producer may announce. The API is unauthenticated, so the table
/// must not become somewhere to park arbitrary data.
pub const MAX_URL_LEN: usize = 256;
pub const MAX_FEATURES: usize = 32;
pub const MAX_FEATURE_LEN: usize = 32;

/// One source's control surface, as published on its `SourceView`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceControl {
    /// Base URL of the producer's control API. Absolute `http(s)://`, reachable from
    /// wherever the consumer runs — the producer knows its own address, the daemon
    /// does not guess one.
    pub url: String,
    /// What the producer says it supports (`pause`, `next`, `browse_media`, …). Opaque
    /// here: the vocabulary belongs to the producer and its consumer.
    #[serde(default)]
    pub features: Vec<String>,
    /// Version of the producer's control protocol, so a consumer can refuse one it
    /// does not speak instead of misreading it.
    #[serde(default = "default_protocol")]
    pub protocol: u32,
}

fn default_protocol() -> u32 {
    1
}

impl SourceControl {
    /// Refuse anything that is not a plausible control URL plus a small, plain
    /// feature list. Returns the sentence a `400` should carry.
    pub fn validate(&self) -> Result<(), String> {
        let url = self.url.trim();
        if url.len() > MAX_URL_LEN {
            return Err(format!("control url longer than {MAX_URL_LEN} characters"));
        }
        let rest = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://"));
        match rest {
            Some(r) if !r.is_empty() && !r.starts_with('/') => {}
            _ => return Err("control url must be an absolute http:// or https:// URL".to_string()),
        }
        if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("control url must not contain whitespace".to_string());
        }
        if self.features.len() > MAX_FEATURES {
            return Err(format!("at most {MAX_FEATURES} features"));
        }
        for f in &self.features {
            if f.is_empty() || f.len() > MAX_FEATURE_LEN || !f.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                return Err(format!("feature '{f}' is not a short snake_case word"));
            }
        }
        Ok(())
    }
}

/// Every source's announced control surface, behind one mutex. Cheap to clone.
#[derive(Clone, Default)]
pub struct ControlRegistry {
    inner: Arc<Mutex<BTreeMap<String, (SourceControl, Instant)>>>,
}

impl ControlRegistry {
    /// Record (or refresh) the control surface of the source with `node_name`.
    pub fn announce(&self, node_name: &str, control: SourceControl) {
        let control = SourceControl { url: control.url.trim().trim_end_matches('/').to_string(), ..control };
        self.inner.lock_recover().insert(node_name.to_string(), (control, Instant::now()));
    }

    /// The producer went away and said so. Idempotent.
    pub fn withdraw(&self, node_name: &str) {
        self.inner.lock_recover().remove(node_name);
    }

    /// One source's control surface, or `None` when none was announced (or it expired).
    pub fn get(&self, node_name: &str) -> Option<SourceControl> {
        self.get_at(node_name, Instant::now())
    }

    fn get_at(&self, node_name: &str, now: Instant) -> Option<SourceControl> {
        let mut map = self.inner.lock_recover();
        match map.get(node_name) {
            Some((c, touched)) if now.duration_since(*touched) < TTL => Some(c.clone()),
            Some(_) => {
                map.remove(node_name);
                None
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control(url: &str) -> SourceControl {
        SourceControl { url: url.to_string(), features: vec!["pause".into(), "browse_media".into()], protocol: 1 }
    }

    #[test]
    fn an_announcement_is_readable_until_it_expires() {
        let reg = ControlRegistry::default();
        reg.announce("rtp-in-ytm", control("http://127.0.0.1:8096/"));
        let got = reg.get("rtp-in-ytm").expect("announced");
        // A trailing slash is normalised away, so a consumer can append paths.
        assert_eq!(got.url, "http://127.0.0.1:8096");
        assert!(reg.get_at("rtp-in-ytm", Instant::now() + TTL + Duration::from_secs(1)).is_none());
        // Expiry removes it, so a later read does not resurrect it either.
        assert!(reg.get("rtp-in-ytm").is_none());
    }

    #[test]
    fn a_withdrawal_removes_it_and_is_idempotent() {
        let reg = ControlRegistry::default();
        reg.announce("rtp-in-ytm", control("http://127.0.0.1:8096"));
        reg.withdraw("rtp-in-ytm");
        reg.withdraw("rtp-in-ytm");
        assert!(reg.get("rtp-in-ytm").is_none());
    }

    #[test]
    fn only_plain_http_urls_and_snake_case_features_are_accepted() {
        assert!(control("http://127.0.0.1:8096").validate().is_ok());
        assert!(control("https://receiver.local/api").validate().is_ok());
        for bad in ["", "127.0.0.1:8096", "file:///etc/passwd", "http://", "http:///x", "http://a b"] {
            assert!(control(bad).validate().is_err(), "{bad:?} must be refused");
        }
        assert!(control(&format!("http://{}", "a".repeat(MAX_URL_LEN))).validate().is_err());
        let mut c = control("http://127.0.0.1:8096");
        c.features = vec!["Pause!".into()];
        assert!(c.validate().is_err());
        c.features = (0..=MAX_FEATURES).map(|i| format!("f{i}")).collect();
        assert!(c.validate().is_err());
    }

    #[test]
    fn the_protocol_defaults_to_one() {
        let c: SourceControl = serde_json::from_str(r#"{"url":"http://127.0.0.1:8096"}"#).unwrap();
        assert_eq!(c.protocol, 1);
        assert!(c.features.is_empty());
    }
}
