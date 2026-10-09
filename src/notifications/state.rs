//! Credential-free, deterministic transition reducer. Expiry is never recovery evidence.
use crate::quota::Window;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Default, Debug)]
pub struct Evidence {
    pub sequence: u64,
    pub observations: VecDeque<Observation>,
    pub overflow: bool,
}
#[derive(Clone, Debug)]
pub struct Observation {
    pub sequence: u64,
    pub at: DateTime<Utc>,
    pub windows: Vec<Window>,
    pub authoritative: bool,
    pub unknown: Option<String>,
}
impl Evidence {
    /// Capture bounded request or provider quota evidence using the current observation time.
    pub fn observe(&mut self, windows: &[Window], authoritative: bool) {
        self.observe_at(windows, authoritative, Utc::now());
    }
    /// Capture valid quota windows with the provider poll start time for stale-response protection.
    pub fn observe_at(&mut self, windows: &[Window], authoritative: bool, at: DateTime<Utc>) {
        let windows: Vec<_> = windows.iter().filter(|w| valid_window(w)).cloned().collect();
        if windows.is_empty() {
            return;
        }
        self.push(windows, authoritative, None, at);
    }
    /// Record a model-scoped rejection without inventing a quota window or reset estimate.
    pub fn exhaust(&mut self, model: &str) {
        let scope = if model.contains("opus") {
            "opus"
        } else if model.contains("sonnet") {
            "sonnet"
        } else {
            "*"
        };
        self.push(Vec::new(), false, Some(scope.into()), Utc::now());
    }
    /// Assign an evidence sequence and flag overflow when the bounded observation queue drops data.
    fn push(&mut self, windows: Vec<Window>, authoritative: bool, unknown: Option<String>, at: DateTime<Utc>) {
        self.sequence = self.sequence.saturating_add(1);
        if self.observations.len() == 128 {
            self.observations.pop_front();
            self.overflow = true;
        }
        self.observations.push_back(Observation { sequence: self.sequence, at, windows, authoritative, unknown });
    }
}
/// Accept only finite usage percentages and the supported shared or model-scoped window names.
pub fn valid_window(w: &Window) -> bool {
    w.used.is_finite()
        && (0.0..=100.0).contains(&w.used)
        && matches!(w.name.as_str(), "5h" | "week" | "week opus" | "week sonnet" | "week overage" | "day")
        && w.model.as_deref().is_none_or(|m| matches!(m, "opus" | "sonnet"))
}
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct Subscription {
    pub provider: String,
    pub windows: BTreeMap<String, Tracked>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Tracked {
    pub name: String,
    pub model: Option<String>,
    pub exhausted: bool,
    pub observed_at: DateTime<Utc>,
    pub reset: Option<DateTime<Utc>>,
    pub used: Option<f64>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Event {
    pub version: u8,
    pub id: String,
    pub event: String,
    pub subscription: String,
    pub provider: String,
    pub window: String,
    pub model: Option<String>,
    pub observed_at: DateTime<Utc>,
    pub resets_at: Option<DateTime<Utc>>,
    pub used: Option<f64>,
    pub remaining_blockers: Vec<String>,
}
impl Subscription {
    /// Reduce fresh quota evidence into exhaustion or confirmed recovery events; elapsed timers never recover.
    pub fn apply(&mut self, id: &str, observation: &Observation) -> Vec<Event> {
        let mut transitions = Vec::new();
        let mut observations: Vec<_> = observation
            .windows
            .iter()
            .filter(|w| valid_window(w))
            .map(|w| Tracked {
                name: w.name.clone(),
                model: w.model.clone(),
                exhausted: w.used >= 100.0,
                observed_at: observation.at,
                reset: w.resets_at,
                used: Some(w.used),
            })
            .collect();
        if let Some(scope) = &observation.unknown {
            // A known blocking window already explains a quota rejection.
            if !self.windows.values().any(|w| w.exhausted && (w.model.is_none() || w.model.as_deref() == Some(scope))) {
                observations.push(Tracked {
                    name: "unknown".into(),
                    model: (scope != "*").then(|| scope.clone()),
                    exhausted: true,
                    observed_at: observation.at,
                    reset: None,
                    used: None,
                });
            }
        }
        // An unscoped quota rejection only clears after fresh authoritative observations
        // cover both shared windows AND every previously learned applicable model window.
        if observation.authoritative
            && observation.windows.iter().any(|w| w.name == "5h" && w.model.is_none())
            && observation.windows.iter().any(|w| w.name == "week" && w.model.is_none())
        {
            for unknown in self.windows.values().filter(|w| w.name == "unknown" && w.exhausted) {
                let covered = self
                    .windows
                    .values()
                    .filter(|w| w.name != "unknown" && (w.model.is_none() || w.model == unknown.model))
                    .all(|old| observation.windows.iter().any(|w| w.name == old.name && w.model == old.model));
                let available = observation
                    .windows
                    .iter()
                    .filter(|w| w.model.is_none() || w.model == unknown.model)
                    .all(|w| w.used < 100.0);
                if covered && available {
                    observations.push(Tracked { observed_at: observation.at, exhausted: false, ..unknown.clone() });
                }
            }
        }
        for next in observations {
            let key = format!("{}:{}", next.name, next.model.as_deref().unwrap_or("*"));
            if self.windows.get(&key).is_some_and(|old| old.observed_at > next.observed_at) {
                continue;
            }
            // An elapsed exhausted sample is old evidence; it cannot start a new episode.
            if next.reset.is_some_and(|r| r <= observation.at) {
                continue;
            }
            let old = self.windows.get(&key);
            // Concurrent response headers/WS have no provider sequence. A low sample
            // can request confirmation, but only a fresh usage request can clear a block.
            if !observation.authoritative && !next.exhausted && old.is_some_and(|old| old.exhausted) {
                continue;
            }
            if next.exhausted && old.is_none_or(|old| !old.exhausted) {
                transitions.push(("quota.exhausted", next.clone()));
            } else if !next.exhausted && old.is_some_and(|old| old.exhausted) {
                transitions.push(("quota.window_recovered", next.clone()));
            }
            self.windows.insert(key, next);
        }
        let blockers: Vec<_> = self
            .windows
            .values()
            .filter(|w| w.exhausted)
            .map(|w| {
                if w.name == "unknown" {
                    w.model.as_ref().map(|m| format!("unknown {m}")).unwrap_or_else(|| w.name.clone())
                } else {
                    w.name.clone()
                }
            })
            .collect();
        let recovered = transitions.iter().any(|(kind, _)| *kind == "quota.window_recovered");
        if recovered && blockers.is_empty() {
            let last = transitions.last().unwrap().1.clone();
            transitions.retain(|(kind, _)| *kind != "quota.window_recovered");
            transitions.push(("quota.available", Tracked { name: "all".into(), model: None, ..last }));
        }
        transitions
            .into_iter()
            .map(|(kind, w)| Event {
                version: 1,
                id: uuid::Uuid::new_v4().to_string(),
                event: kind.into(),
                subscription: id.into(),
                provider: self.provider.clone(),
                window: w.name,
                model: w.model,
                observed_at: observation.at,
                resets_at: w.reset,
                used: w.used,
                remaining_blockers: blockers.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Wrap deterministic window samples as authoritative provider evidence.
    fn obs(windows: Vec<Window>) -> Observation {
        Observation { sequence: 1, at: Utc::now(), windows, authoritative: true, unknown: None }
    }
    /// Construct a bounded shared-window sample with a future reset estimate.
    fn window(name: &str, used: f64) -> Window {
        Window { name: name.into(), used, resets_at: None, model: None }
    }
    #[test]
    /// Verify that exhaustion is deduplicated and partial recovery preserves week.
    fn exhaustion_is_deduplicated_and_partial_recovery_preserves_week() {
        let mut sub = Subscription::default();
        let used = obs(vec![window("5h", 100.0), window("week", 100.0)]);
        assert_eq!(sub.apply("id", &used).len(), 2);
        assert!(sub.apply("id", &used).is_empty());
        let recovered = sub.apply("id", &obs(vec![window("5h", 0.0)]));
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].remaining_blockers, vec!["week"]);
        let recovered = sub.apply("id", &obs(vec![window("week", 0.0)]));
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].event, "quota.available");
    }
    #[test]
    /// Verify that expired missing and stale samples cannot recover.
    fn expired_missing_and_stale_samples_cannot_recover() {
        let mut sub = Subscription::default();
        let used = obs(vec![window("5h", 100.0)]);
        sub.apply("id", &used);
        assert!(sub.apply("id", &obs(vec![])).is_empty());
        let mut elapsed = window("5h", 0.0);
        elapsed.resets_at = Some(Utc::now() - chrono::Duration::seconds(1));
        assert!(sub.apply("id", &obs(vec![elapsed])).is_empty());
        let mut stale = obs(vec![window("5h", 0.0)]);
        stale.at = used.at - chrono::Duration::seconds(1);
        assert!(sub.apply("id", &stale).is_empty());
        assert!(sub.windows.values().all(|w| w.exhausted));
    }
    #[test]
    /// Verify that unknown requires full authoritative evidence and model scope.
    fn unknown_requires_full_authoritative_evidence_and_model_scope() {
        let mut sub = Subscription::default();
        let mut unknown = obs(vec![]);
        unknown.unknown = Some("*".into());
        assert_eq!(sub.apply("id", &unknown).len(), 1);
        assert!(sub.apply("id", &obs(vec![window("5h", 0.0)])).is_empty());
        let recovered = sub.apply("id", &obs(vec![window("5h", 0.0), window("week", 0.0)]));
        assert_eq!(recovered.len(), 1);
    }
    #[test]
    /// Verify that headers cannot recover and provider reads started before exhaustion are stale.
    fn headers_cannot_recover_and_provider_reads_started_before_exhaustion_are_stale() {
        let mut sub = Subscription::default();
        let used = obs(vec![window("5h", 100.0)]);
        sub.apply("id", &used);
        let mut headers = obs(vec![window("5h", 1.0)]);
        headers.authoritative = false;
        assert!(sub.apply("id", &headers).is_empty());
        assert!(sub.windows.values().all(|w| w.exhausted));
        headers.authoritative = true;
        headers.at = used.at - chrono::Duration::milliseconds(1);
        assert!(sub.apply("id", &headers).is_empty());
        let recovered = sub.apply("id", &obs(vec![window("5h", 1.0)]));
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].event, "quota.available");
    }
    #[test]
    /// Verify that model scoped week stays a blocker after shared window recovers.
    fn model_scoped_week_stays_a_blocker_after_shared_window_recovers() {
        let mut sub = Subscription::default();
        let opus = Window { name: "week opus".into(), model: Some("opus".into()), ..window("week", 100.0) };
        sub.apply("id", &obs(vec![opus, window("5h", 100.0)]));
        let recovered = sub.apply("id", &obs(vec![window("5h", 0.0)]));
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].event, "quota.window_recovered");
        assert_eq!(recovered[0].remaining_blockers, vec!["week opus"]);
    }
}
