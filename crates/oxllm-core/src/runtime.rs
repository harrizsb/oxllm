use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const RESETTING_DAY: u64 = u64::MAX;
const REQUEST_LOG_CAPACITY: usize = 3;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DailyTokenSnapshot {
    pub day: u64,
    pub cached_tokens: u64,
    pub uncached_tokens: u64,
    pub total_tokens: u64,
}

/// Atomic counters with a rollover gate. Updates stay lock-free; rollover briefly
/// blocks writers while it resets the three counters after claiming the day via CAS.
pub struct DailyTokenAccounting {
    day: AtomicU64,
    active_writers: AtomicUsize,
    cached_tokens: AtomicU64,
    uncached_tokens: AtomicU64,
    total_tokens: AtomicU64,
}

impl Default for DailyTokenAccounting {
    fn default() -> Self {
        Self::new(current_utc_day())
    }
}

impl DailyTokenAccounting {
    pub fn new(day: u64) -> Self {
        Self {
            day: AtomicU64::new(day),
            active_writers: AtomicUsize::new(0),
            cached_tokens: AtomicU64::new(0),
            uncached_tokens: AtomicU64::new(0),
            total_tokens: AtomicU64::new(0),
        }
    }

    pub fn record_usage(&self, prompt_tokens: u64, cached_tokens: u64, completion_tokens: u64) {
        self.record_usage_for_day(
            current_utc_day(),
            prompt_tokens,
            cached_tokens,
            completion_tokens,
        );
    }

    fn record_usage_for_day(
        &self,
        day: u64,
        prompt_tokens: u64,
        cached_tokens: u64,
        completion_tokens: u64,
    ) {
        let cached = cached_tokens;
        let uncached = prompt_tokens.saturating_sub(cached);
        let total = prompt_tokens.saturating_add(completion_tokens);

        let mut accounting_day = day;
        loop {
            let stored_day = self.day.load(Ordering::Acquire);
            // A request can cross midnight between sampling its day and recording usage.
            // Never roll counters backward; attribute a late update to the active day.
            accounting_day = accounting_day.max(stored_day.min(RESETTING_DAY - 1));
            self.ensure_day(accounting_day);
            self.active_writers.fetch_add(1, Ordering::Acquire);
            if self.day.load(Ordering::Acquire) != accounting_day {
                self.active_writers.fetch_sub(1, Ordering::Release);
                continue;
            }
            self.cached_tokens.fetch_add(cached, Ordering::Relaxed);
            self.uncached_tokens.fetch_add(uncached, Ordering::Relaxed);
            self.total_tokens.fetch_add(total, Ordering::Relaxed);
            self.active_writers.fetch_sub(1, Ordering::Release);
            return;
        }
    }

    fn ensure_day(&self, day: u64) {
        loop {
            let stored_day = self.day.load(Ordering::Acquire);
            if stored_day == day || (stored_day != RESETTING_DAY && stored_day > day) {
                return;
            }
            if stored_day == RESETTING_DAY {
                std::thread::yield_now();
                continue;
            }
            if self
                .day
                .compare_exchange(
                    stored_day,
                    RESETTING_DAY,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                continue;
            }

            while self.active_writers.load(Ordering::Acquire) != 0 {
                std::thread::yield_now();
            }
            self.cached_tokens.store(0, Ordering::Relaxed);
            self.uncached_tokens.store(0, Ordering::Relaxed);
            self.total_tokens.store(0, Ordering::Relaxed);
            self.day.store(day, Ordering::Release);
            return;
        }
    }

    pub fn snapshot_on_day(&self, day: u64) -> DailyTokenSnapshot {
        self.ensure_day(day);
        loop {
            let stored_day = self.day.load(Ordering::Acquire);
            if stored_day == day {
                break;
            }
            if stored_day != RESETTING_DAY {
                break; // stored day is newer; nothing to reset
            }
        }
        self.read_snapshot()
    }

    pub fn snapshot(&self) -> DailyTokenSnapshot {
        self.ensure_day(current_utc_day());
        loop {
            let day = self.day.load(Ordering::Acquire);
            if day == RESETTING_DAY {
                std::thread::yield_now();
                continue;
            }
            let snapshot = self.read_snapshot();
            if self.day.load(Ordering::Acquire) == day {
                return snapshot;
            }
        }
    }

    fn read_snapshot(&self) -> DailyTokenSnapshot {
        loop {
            let day = self.day.load(Ordering::Acquire);
            if day == RESETTING_DAY {
                std::thread::yield_now();
                continue;
            }
            let snapshot = DailyTokenSnapshot {
                day,
                cached_tokens: self.cached_tokens.load(Ordering::Relaxed),
                uncached_tokens: self.uncached_tokens.load(Ordering::Relaxed),
                total_tokens: self.total_tokens.load(Ordering::Relaxed),
            };
            if self.day.load(Ordering::Acquire) == day {
                return snapshot;
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RequestLogEntry {
    pub timestamp: u64,
    pub model_requested: String,
    pub virtual_model: Option<String>,
    pub provider: String,
    pub cached_tokens: u64,
    pub uncached_tokens: u64,
    pub status_code: u16,
}

#[derive(Default)]
pub struct RuntimeMetrics {
    pub daily_tokens: DailyTokenAccounting,
    request_log: Mutex<VecDeque<RequestLogEntry>>,
}

impl RuntimeMetrics {
    pub fn push_request(&self, entry: RequestLogEntry) {
        let mut entries = match self.request_log.lock() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        if entries.len() == REQUEST_LOG_CAPACITY {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    pub fn recent_requests(&self) -> Vec<RequestLogEntry> {
        let entries = match self.request_log.lock() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        entries.iter().cloned().collect()
    }
}

pub fn current_utc_day() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86_400
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_updates_cached_uncached_and_total_from_provider_values() {
        let accounting = DailyTokenAccounting::new(10);
        accounting.record_usage_for_day(10, 100, 25, 40);
        assert_eq!(
            accounting.snapshot_on_day(10),
            DailyTokenSnapshot {
                day: 10,
                cached_tokens: 25,
                uncached_tokens: 75,
                total_tokens: 140,
            }
        );
    }

    #[test]
    fn day_rollover_resets_all_daily_counters() {
        let accounting = DailyTokenAccounting::new(10);
        accounting.record_usage_for_day(10, 100, 25, 40);
        accounting.record_usage_for_day(11, 9, 0, 2);
        assert_eq!(
            accounting.snapshot_on_day(11),
            DailyTokenSnapshot {
                day: 11,
                cached_tokens: 0,
                uncached_tokens: 9,
                total_tokens: 11,
            }
        );
    }

    #[test]
    fn late_previous_day_update_does_not_roll_counters_backward() {
        let accounting = DailyTokenAccounting::new(11);
        accounting.record_usage_for_day(10, 5, 0, 2);
        let snapshot = accounting.snapshot_on_day(11);
        assert_eq!(snapshot.day, 11);
        assert_eq!(snapshot.total_tokens, 7);
    }

    #[test]
    fn snapshot_marks_day_rollover_without_waiting_for_a_request() {
        let accounting = DailyTokenAccounting::new(10);
        accounting.record_usage_for_day(10, 5, 0, 2);
        let snapshot = accounting.snapshot_on_day(11);
        assert_eq!(snapshot.day, 11);
        assert_eq!(snapshot.cached_tokens, 0);
        assert_eq!(snapshot.uncached_tokens, 0);
        assert_eq!(snapshot.total_tokens, 0);
    }

    #[test]
    fn cached_value_above_prompt_is_preserved_as_reported() {
        let accounting = DailyTokenAccounting::new(10);
        accounting.record_usage_for_day(10, 5, 9, 2);
        assert_eq!(accounting.snapshot_on_day(10).cached_tokens, 9);
        assert_eq!(accounting.snapshot_on_day(10).uncached_tokens, 0);
        assert_eq!(accounting.snapshot_on_day(10).total_tokens, 7);
    }

    #[test]
    fn request_log_retains_only_three_latest_entries() {
        let metrics = RuntimeMetrics::default();
        for timestamp in 1..=4 {
            metrics.push_request(RequestLogEntry {
                timestamp,
                model_requested: format!("model-{timestamp}"),
                virtual_model: None,
                provider: format!("provider-{timestamp}"),
                cached_tokens: 0,
                uncached_tokens: 0,
                status_code: 200,
            });
        }

        let entries = metrics.recent_requests();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].model_requested, "model-2");
        assert_eq!(entries[2].model_requested, "model-4");
    }
}
