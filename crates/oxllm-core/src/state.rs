use crate::config::VirtualModelTarget;
use reqwest::Url;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    Closed,
    Open { until: Instant },
    HalfOpen,
}

#[derive(Debug)]
pub struct ProviderState {
    pub name: String,
    pub base_url: Url, // Parsed reqwest::Url to handle safe path joins and trailing slashes
    pub api_key: String,
    pub models: Vec<String>,

    // Protects volatile metrics without needing a write lock on the entire pool vector
    pub circuit: Arc<RwLock<CircuitState>>,
    pub consecutive_failures: Arc<RwLock<u32>>,
    pub rate_limited_until: Arc<RwLock<Option<Instant>>>,
    pub last_attempt_time: Arc<RwLock<Option<Instant>>>,

    // Lock-free thundering-herd permit
    pub probe_in_flight: Arc<AtomicBool>,

    // Manual admin override — skips provider in routing regardless of circuit state
    pub manual_disabled: AtomicBool,

    // Local request/token counters (visible via /status without otel collector)
    pub requests: AtomicU64,
    pub successes: AtomicU64,
    pub tokens_input: AtomicU64,
    pub tokens_output: AtomicU64,
}

#[derive(Clone)]
pub struct SelectedProvider {
    pub name: String,
    pub base_url: Url,
    pub api_key: String,
    pub is_probe: bool,
}

/// Advances one smooth weighted round-robin cycle and returns its winning target index.
pub fn next_swrr_index(weights: &[u32], current: &mut Vec<i128>) -> usize {
    if current.len() != weights.len() {
        current.resize(weights.len(), 0);
    }
    let total: i128 = weights.iter().map(|weight| i128::from(*weight)).sum();
    if total == 0 || weights.is_empty() {
        return 0;
    }

    let mut selected = 0;
    for (index, weight) in weights.iter().enumerate() {
        current[index] += i128::from(*weight);
        if current[index] > current[selected] {
            selected = index;
        }
    }
    current[selected] -= total;
    selected
}

pub struct AppState {
    pub providers: Vec<ProviderState>,
    pub virtual_models: HashMap<String, Vec<VirtualModelTarget>>,
    // One small cursor per virtual model keeps SWRR independent without expanding config state.
    pub swrr_current: Mutex<HashMap<String, Vec<i128>>>,
    pub http_client: reqwest::Client,
    pub upstream_timeout_secs: u64,
}

impl AppState {
    /// Picks the SWRR target first, then keeps remaining targets as ordered failovers.
    /// Equal default weights preserve the existing target order over each complete cycle.
    pub async fn resolve_candidates(&self, virtual_model: &str) -> Vec<(&ProviderState, String)> {
        let targets = match self.virtual_models.get(virtual_model) {
            Some(targets) if !targets.is_empty() => targets,
            _ => return Vec::new(),
        };

        let weights: Vec<u32> = targets.iter().map(|target| target.weight).collect();
        let selected = {
            let mut all_current = self.swrr_current.lock().await;
            let current = all_current
                .entry(virtual_model.to_string())
                .or_insert_with(|| vec![0; targets.len()]);
            if current.len() != targets.len() {
                current.resize(targets.len(), 0);
            }
            next_swrr_index(&weights, current)
        };

        let mut candidates = Vec::new();
        for offset in 0..targets.len() {
            let target = &targets[(selected + offset) % targets.len()];
            if let Some(provider) = self.providers.iter().find(|p| p.name == target.provider) {
                candidates.push((provider, target.model.clone()));
            }
        }
        candidates
    }
}

#[cfg(test)]
mod swrr_tests {
    use super::next_swrr_index;

    #[test]
    fn smooth_weighted_round_robin_respects_weight_ratio() {
        let weights = [2, 1];
        let mut current = Vec::new();
        let mut counts = [0; 2];

        for _ in 0..300 {
            counts[next_swrr_index(&weights, &mut current)] += 1;
        }

        assert_eq!(counts, [200, 100]);
    }

    #[test]
    fn smooth_weighted_round_robin_starts_at_highest_weight() {
        let weights = [1, 3];
        let mut current = Vec::new();

        assert_eq!(next_swrr_index(&weights, &mut current), 1);
    }

    #[tokio::test]
    async fn app_state_resolution_distributes_by_target_weight() {
        use super::{AppState, CircuitState, ProviderState};
        use crate::config::VirtualModelTarget;
        use reqwest::Url;
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicBool, AtomicU64};
        use std::sync::Arc;
        use tokio::sync::{Mutex, RwLock};

        let providers = ["provider-a", "provider-b"]
            .into_iter()
            .map(|name| ProviderState {
                name: name.to_string(),
                base_url: Url::parse("https://example.com").unwrap(),
                api_key: "key".to_string(),
                models: vec!["model".to_string()],
                circuit: Arc::new(RwLock::new(CircuitState::Closed)),
                consecutive_failures: Arc::new(RwLock::new(0)),
                rate_limited_until: Arc::new(RwLock::new(None)),
                last_attempt_time: Arc::new(RwLock::new(None)),
                probe_in_flight: Arc::new(AtomicBool::new(false)),
                manual_disabled: AtomicBool::new(false),
                requests: AtomicU64::new(0),
                successes: AtomicU64::new(0),
                tokens_input: AtomicU64::new(0),
                tokens_output: AtomicU64::new(0),
            })
            .collect();
        let virtual_models = HashMap::from([(
            "balanced".to_string(),
            vec![
                VirtualModelTarget {
                    provider: "provider-a".to_string(),
                    model: "model-a".to_string(),
                    weight: 2,
                },
                VirtualModelTarget {
                    provider: "provider-b".to_string(),
                    model: "model-b".to_string(),
                    weight: 1,
                },
            ],
        )]);
        let app_state = AppState {
            providers,
            virtual_models,
            swrr_current: Mutex::new(HashMap::new()),
            http_client: reqwest::Client::new(),
            upstream_timeout_secs: 5,
        };

        let mut counts = [0; 2];
        for _ in 0..300 {
            let candidates = app_state.resolve_candidates("balanced").await;
            match candidates[0].0.name.as_str() {
                "provider-a" => counts[0] += 1,
                "provider-b" => counts[1] += 1,
                other => panic!("unexpected selected provider: {other}"),
            }
        }

        assert_eq!(counts, [200, 100]);
    }
}
