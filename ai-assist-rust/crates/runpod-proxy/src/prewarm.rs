//! Scheduled prewarm with an injectable clock (spec section 6.4). WI-11.
//!
//! WI-05 lands `parse_prewarm_times` (parity with `proxy/prewarm.py`).
//! WI-11 lands the `PrewarmScheduler` (parity with `PrewarmScheduler`).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Timelike;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::state::{EndpointState, State};
use crate::warmup::WarmupManager;

/// The default scheduler tick (parity with Python `tick_s=20.0`).
const DEFAULT_TICK_S: f64 = 20.0;

/// A local-time snapshot for scheduling: calendar day (`YYYY-MM-DD`) and
/// seconds since local midnight (parity with `time.localtime()` fields).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalNow {
    /// Local calendar day formatted `YYYY-MM-DD`.
    pub day: String,
    /// Seconds since local midnight (`tm_hour*3600 + tm_min*60 + tm_sec`).
    pub sec_of_day: u32,
}

/// The production clock: local wall-clock time (parity with `time.localtime`).
pub fn local_now() -> LocalNow {
    let now = chrono::Local::now();
    LocalNow {
        day: now.format("%Y-%m-%d").to_string(),
        sec_of_day: now.hour() * 3600 + now.minute() * 60 + now.second(),
    }
}

/// An injectable clock returning the local day + seconds-of-day.
type NowFn = Arc<dyn Fn() -> LocalNow + Send + Sync>;

/// Fires one warmup per configured local time per calendar day (spec section
/// 6.4). A tick considers a time due when the current time is within
/// `max(120s, 2 * tick_s)` after it. Only the active model is warmed — the
/// scheduler never triggers a model switch. The clock is injectable for tests.
pub struct PrewarmScheduler {
    times: Vec<(u8, u8)>,
    warmup: Arc<WarmupManager>,
    state: Arc<std::sync::Mutex<EndpointState>>,
    tick_s: f64,
    now_fn: NowFn,
    /// `(day, time-index)` pairs already fired; a restart re-fires a still-in-
    /// window slot, which is harmless (warm stays warm).
    fired: Mutex<HashSet<(String, usize)>>,
    /// Stop signal: `true` asks the loop to exit.
    stop_tx: watch::Sender<bool>,
    /// The running loop task, if any.
    task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl PrewarmScheduler {
    /// Build a scheduler over the configured times and the shared warmup/state
    /// (uses the production local-time clock and the default 20s tick).
    pub fn new(
        times: Vec<(u8, u8)>,
        warmup: Arc<WarmupManager>,
        state: Arc<std::sync::Mutex<EndpointState>>,
    ) -> Arc<Self> {
        Self::with_clock(times, warmup, state, DEFAULT_TICK_S, Arc::new(local_now))
    }

    /// Build a scheduler with an explicit tick and clock (for tests).
    pub fn with_clock(
        times: Vec<(u8, u8)>,
        warmup: Arc<WarmupManager>,
        state: Arc<std::sync::Mutex<EndpointState>>,
        tick_s: f64,
        now_fn: NowFn,
    ) -> Arc<Self> {
        let (stop_tx, _) = watch::channel(false);
        Arc::new(Self {
            times,
            warmup,
            state,
            tick_s,
            now_fn,
            fired: Mutex::new(HashSet::new()),
            stop_tx,
            task: tokio::sync::Mutex::new(None),
        })
    }

    /// Start the loop (idempotent; a no-op when no times are configured).
    pub async fn start(self: &Arc<Self>) {
        if self.times.is_empty() {
            return;
        }
        let mut task = self.task.lock().await;
        if task.is_some() {
            return;
        }
        let _ = self.stop_tx.send(false);
        let stop_rx = self.stop_tx.subscribe();
        let this = Arc::clone(self);
        let handle = tokio::spawn(async move { this.run(stop_rx).await });
        *task = Some(handle);
        tracing::info!(
            times = %self
                .times
                .iter()
                .map(|(h, m)| format!("{h:02}:{m:02}"))
                .collect::<Vec<_>>()
                .join(", "),
            "prewarm: scheduled for local times"
        );
    }

    /// Stop the loop (idempotent). Signals the loop and aborts the task.
    pub async fn stop(&self) {
        let _ = self.stop_tx.send(true);
        let mut task = self.task.lock().await;
        if let Some(handle) = task.take() {
            handle.abort();
            let _ = handle.await;
        }
    }

    /// The interval loop: sleep a tick, then evaluate slots, until stopped.
    async fn run(self: Arc<Self>, mut stop_rx: watch::Receiver<bool>) {
        let tick = Duration::from_secs_f64(self.tick_s);
        loop {
            tokio::select! {
                _ = stop_rx.changed() => {
                    if *stop_rx.borrow() {
                        return;
                    }
                }
                () = tokio::time::sleep(tick) => {
                    self.tick_once((self.now_fn)()).await;
                }
            }
        }
    }

    /// Evaluate the configured slots against `now`, firing at most one warmup
    /// per due slot per day (parity with `_tick_once`).
    pub async fn tick_once(&self, now: LocalNow) {
        let window = f64::max(120.0, 2.0 * self.tick_s);
        for (index, &(hour, minute)) in self.times.iter().enumerate() {
            let target_s = i64::from(hour) * 3600 + i64::from(minute) * 60;
            let delta = (i64::from(now.sec_of_day) - target_s).rem_euclid(86_400);
            #[allow(clippy::cast_precision_loss)]
            if delta as f64 >= window {
                continue;
            }
            {
                let mut fired = self.fired.lock().unwrap();
                if fired.contains(&(now.day.clone(), index)) {
                    continue;
                }
                // Keep only today's fired slots, then record this one.
                fired.retain(|(d, _)| d == &now.day);
                fired.insert((now.day.clone(), index));
            }
            if self.state.lock().unwrap().state == State::Warm {
                tracing::info!(hour, minute, "prewarm: slot already warm; nothing to do");
                continue;
            }
            tracing::info!(hour, minute, "prewarm: slot warming the active model");
            if let Err(e) = self.warmup.ensure_warm(false).await {
                tracing::warn!(%e, "prewarm: scheduled warmup failed; dropping this slot");
            }
        }
    }
}

/// Parse `"HH:MM,HH:MM"` into `(hour, minute)` pairs; invalid parts are
/// dropped with a warning (port of `proxy/prewarm.py::parse_prewarm_times`).
///
/// Divergences from Python `int()`: underscores in digits (`"1_0"`) are
/// rejected here (Python accepts them); each half is trimmed before parsing,
/// matching Python's whitespace tolerance.
pub fn parse_prewarm_times(raw: &str) -> Vec<(u8, u8)> {
    let mut times = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let bits: Vec<&str> = part.split(':').collect();
        let (hour, minute) = if bits.len() == 2 {
            if let (Ok(h), Ok(m)) = (bits[0].trim().parse::<i32>(), bits[1].trim().parse::<i32>()) {
                (h, m)
            } else {
                tracing::warn!(time = %part, "prewarm: ignoring invalid time (expected HH:MM)");
                continue;
            }
        } else {
            tracing::warn!(time = %part, "prewarm: ignoring invalid time (expected HH:MM)");
            continue;
        };
        if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
            tracing::warn!(time = %part, "prewarm: ignoring out-of-range time");
            continue;
        }
        times.push((
            u8::try_from(hour).expect("hour range-checked 0..=23"),
            u8::try_from(minute).expect("minute range-checked 0..=59"),
        ));
    }
    times
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_times() {
        assert_eq!(parse_prewarm_times("08:30"), vec![(8, 30)]);
        assert_eq!(
            parse_prewarm_times("08:30, 13:05,23:59"),
            vec![(8, 30), (13, 5), (23, 59)]
        );
        assert_eq!(parse_prewarm_times("0:0"), vec![(0, 0)]);
    }

    #[test]
    fn skips_empty_parts() {
        assert_eq!(
            parse_prewarm_times("08:30,,  ,09:00"),
            vec![(8, 30), (9, 0)]
        );
        assert_eq!(parse_prewarm_times(""), Vec::<(u8, u8)>::new());
        assert_eq!(parse_prewarm_times(" , ,"), Vec::<(u8, u8)>::new());
    }

    #[test]
    fn drops_malformed_parts() {
        assert_eq!(
            parse_prewarm_times("8:30,banana,12,12:60:00"),
            vec![(8, 30)]
        );
        assert_eq!(parse_prewarm_times("12:xx,xx:12"), Vec::<(u8, u8)>::new());
    }

    #[test]
    fn drops_out_of_range_parts() {
        assert_eq!(parse_prewarm_times("24:00,08:60,08:30"), vec![(8, 30)]);
        assert_eq!(parse_prewarm_times("-1:00,08:-5"), Vec::<(u8, u8)>::new());
    }

    #[test]
    fn tolerates_inner_whitespace() {
        assert_eq!(parse_prewarm_times(" 08: 30 "), vec![(8, 30)]);
    }

    // ---- PrewarmScheduler ----

    use std::sync::Arc;

    use crate::config::Config;
    use crate::health::{ProbeClient, ProbeError, ProbeResponse};
    use crate::lifecycle::serverless::ServerlessLifecycle;
    use crate::target::UpstreamTarget;
    use crate::warmup::WarmupManager;

    /// A probe client that always reports ready (warmup succeeds -> WARM).
    struct ReadyProbe;

    #[async_trait::async_trait]
    impl ProbeClient for ReadyProbe {
        async fn get(
            &self,
            _url: &str,
            _headers: &[(String, String)],
        ) -> Result<ProbeResponse, ProbeError> {
            Ok(ProbeResponse::json(200, &serde_json::json!({ "data": [] })))
        }
        async fn post_json(
            &self,
            _url: &str,
            _body: &serde_json::Value,
            _headers: &[(String, String)],
            _timeout: f64,
        ) -> Result<ProbeResponse, ProbeError> {
            Ok(ProbeResponse::json(200, &serde_json::json!({ "id": "ok" })))
        }
    }

    fn scheduler_for(
        times: Vec<(u8, u8)>,
    ) -> (Arc<PrewarmScheduler>, Arc<std::sync::Mutex<EndpointState>>) {
        let mut env = std::collections::BTreeMap::new();
        env.insert("RUNPOD_MODEL_NAME".to_string(), "qwen".to_string());
        env.insert("POD_HEALTH_MODE".to_string(), "any".to_string());
        let config = Arc::new(Config::from_env_map(&env).unwrap());
        let state = Arc::new(std::sync::Mutex::new(EndpointState::new()));
        let target = Arc::new(std::sync::Mutex::new(UpstreamTarget::new(
            "http://upstream",
            "",
        )));
        let lifecycle: Arc<dyn crate::lifecycle::Lifecycle> = Arc::new(ServerlessLifecycle::new());
        let warmup = Arc::new(WarmupManager::new(
            Arc::clone(&config),
            Arc::new(ReadyProbe),
            Arc::clone(&state),
            Arc::clone(&lifecycle),
            Arc::clone(&target),
        ));
        let now = Arc::new(|| LocalNow {
            day: "2025-01-01".to_string(),
            sec_of_day: 0,
        });
        let sched = PrewarmScheduler::with_clock(times, warmup, Arc::clone(&state), 20.0, now);
        (sched, state)
    }

    fn at(day: &str, hour: u32, minute: u32, sec: u32) -> LocalNow {
        LocalNow {
            day: day.to_string(),
            sec_of_day: hour * 3600 + minute * 60 + sec,
        }
    }

    #[tokio::test]
    async fn fires_when_slot_is_in_window() {
        let (sched, state) = scheduler_for(vec![(8, 30)]);
        assert_eq!(state.lock().unwrap().state, State::Cold);
        sched.tick_once(at("2025-01-01", 8, 30, 5)).await;
        assert_eq!(state.lock().unwrap().state, State::Warm);
    }

    #[tokio::test]
    async fn does_not_fire_outside_window() {
        let (sched, state) = scheduler_for(vec![(8, 30)]);
        // 8:35 is 300s after the slot; window is max(120, 40) = 120s.
        sched.tick_once(at("2025-01-01", 8, 35, 0)).await;
        assert_eq!(state.lock().unwrap().state, State::Cold);
    }

    #[tokio::test]
    async fn fires_once_per_day_even_if_backend_goes_cold() {
        let (sched, state) = scheduler_for(vec![(8, 30)]);
        sched.tick_once(at("2025-01-01", 8, 30, 0)).await;
        assert_eq!(state.lock().unwrap().state, State::Warm);
        // Simulate the backend idling back to COLD, then a second in-window
        // tick the same day: the slot has fired, so it must not re-warm.
        state.lock().unwrap().state = State::Cold;
        sched.tick_once(at("2025-01-01", 8, 30, 30)).await;
        assert_eq!(state.lock().unwrap().state, State::Cold);
    }

    #[tokio::test]
    async fn refires_on_a_new_day() {
        let (sched, state) = scheduler_for(vec![(8, 30)]);
        sched.tick_once(at("2025-01-01", 8, 30, 0)).await;
        state.lock().unwrap().state = State::Cold;
        sched.tick_once(at("2025-01-02", 8, 30, 0)).await;
        assert_eq!(state.lock().unwrap().state, State::Warm);
    }

    #[tokio::test]
    async fn already_warm_slot_is_a_no_op() {
        let (sched, state) = scheduler_for(vec![(8, 30)]);
        state.lock().unwrap().state = State::Warm;
        sched.tick_once(at("2025-01-01", 8, 30, 0)).await;
        assert_eq!(state.lock().unwrap().state, State::Warm);
    }

    /// A probe that reports not-ready on its first call, then ready: lets a
    /// test distinguish "the slot was dropped" (no re-attempt) from "the slot
    /// was retried" (a re-attempt would now succeed and go WARM).
    struct FlakyProbe {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ProbeClient for FlakyProbe {
        async fn get(
            &self,
            _url: &str,
            _headers: &[(String, String)],
        ) -> Result<ProbeResponse, ProbeError> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                Ok(ProbeResponse::json(200, &serde_json::json!({ "data": [] })))
            } else {
                Ok(ProbeResponse::json(
                    200,
                    &serde_json::json!({ "data": [{ "id": "qwen" }] }),
                ))
            }
        }
        async fn post_json(
            &self,
            _url: &str,
            _body: &serde_json::Value,
            _headers: &[(String, String)],
            _timeout: f64,
        ) -> Result<ProbeResponse, ProbeError> {
            Ok(ProbeResponse::json(200, &serde_json::json!({ "id": "ok" })))
        }
    }

    /// A scheduler whose first warmup fails (model health mode, first probe
    /// lists no model, short timeout) but later warmups succeed.
    fn scheduler_for_flaky(
        times: Vec<(u8, u8)>,
    ) -> (Arc<PrewarmScheduler>, Arc<std::sync::Mutex<EndpointState>>) {
        let mut env = std::collections::BTreeMap::new();
        env.insert("RUNPOD_MODEL_NAME".to_string(), "qwen".to_string());
        env.insert("POD_HEALTH_MODE".to_string(), "model".to_string());
        env.insert("WARMUP_TIMEOUT_S".to_string(), "0.3".to_string());
        let config = Arc::new(Config::from_env_map(&env).unwrap());
        let state = Arc::new(std::sync::Mutex::new(EndpointState::new()));
        let target = Arc::new(std::sync::Mutex::new(UpstreamTarget::new(
            "http://upstream",
            "",
        )));
        let lifecycle: Arc<dyn crate::lifecycle::Lifecycle> = Arc::new(ServerlessLifecycle::new());
        let warmup = Arc::new(WarmupManager::new(
            Arc::clone(&config),
            Arc::new(FlakyProbe {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Arc::clone(&state),
            Arc::clone(&lifecycle),
            Arc::clone(&target),
        ));
        let now = Arc::new(|| LocalNow {
            day: "2025-01-01".to_string(),
            sec_of_day: 0,
        });
        let sched = PrewarmScheduler::with_clock(times, warmup, Arc::clone(&state), 20.0, now);
        (sched, state)
    }

    /// A due slot whose warmup fails is dropped (not retried the same day) and
    /// the scheduler survives to fire on a new day.
    #[tokio::test]
    async fn failed_slot_is_dropped_but_scheduler_survives() {
        let (sched, state) = scheduler_for_flaky(vec![(8, 30)]);
        // First due slot: the warmup fails (first probe not ready) and the slot
        // is dropped; the scheduler must not wedge.
        sched.tick_once(at("2025-01-01", 8, 30, 0)).await;
        assert_eq!(state.lock().unwrap().state, State::Cold);
        // A second in-window tick the same day must NOT re-attempt the dropped
        // slot (the flaky probe would now succeed, so a re-attempt would go WARM).
        sched.tick_once(at("2025-01-01", 8, 30, 30)).await;
        assert_eq!(state.lock().unwrap().state, State::Cold);
        // On a new day the scheduler re-evaluates the slot and (now that the
        // probe is ready) warms successfully — proving it survived the failure.
        sched.tick_once(at("2025-01-02", 8, 30, 0)).await;
        assert_eq!(state.lock().unwrap().state, State::Warm);
    }
}
