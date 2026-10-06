//! Discovery lifecycle: creation matrix, cost-safety invariants (spec
//! sections 6.8 and 11). WI-09.
//!
//! Port of `proxy/lifecycle.py::DiscoveryPodLifecycle`. Finds, resumes, or
//! creates a pod matching the configured model. Cost-safety invariants:
//! at-most-one-created-pod-per-model, reclaim-on-all-failure-paths (shielded
//! under cancellation), failed-stops-retried, found-RUNNING-never-stopped.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::{Lifecycle, LifecycleError};
use crate::catalogue::ModelSpec;
use crate::config::Config;
use crate::health::{ProbeClient, classify};
use crate::runpod_api::{
    CreatePodSpec, EXITED, Pod, RUNNING, RunpodApi, RunpodApiError, TERMINATED, Template,
    model_slug, pod_matches_model, template_matches_model,
};
use crate::state::{EndpointState, now_secs};
use crate::target::UpstreamTarget;

/// Spawn a detached, cancellation-shielded task that stops `pod_id` and records
/// the reclaim against the create circuit breaker. Detaching mirrors the Python
/// `asyncio.shield`: the stop must survive the warmup timeout cancelling its
/// caller.
fn spawn_reclaim(
    api: RunpodApi,
    pod_id: String,
    streak: Arc<Mutex<HashMap<String, i64>>>,
    circuit: Arc<Mutex<HashMap<String, f64>>>,
    threshold: i64,
    cooldown: f64,
    model: String,
) {
    tokio::spawn(async move {
        tracing::warn!(%pod_id, "reclaiming unhealthy pod");
        if let Err(e) = api.stop_pod(&pod_id).await {
            tracing::warn!(%pod_id, %e, "failed to reclaim unhealthy pod");
        }
        let mut streak = streak.lock().expect("reclaim streak lock");
        let count = streak.get(&model).copied().unwrap_or(0) + 1;
        streak.insert(model.clone(), count);
        if threshold > 0 && count >= threshold {
            circuit
                .lock()
                .expect("circuit lock")
                .insert(model.clone(), now_secs() + cooldown);
        }
    });
}

/// RAII guard that reclaims a freshly created pod on every exit from the create
/// path unless it is explicitly disarmed after the pod becomes healthy. This is
/// the Rust equivalent of the Python create-path `finally: reclaim_before_cancel`
/// (spec sections 6.8/11): the created pod must be stopped on the not-ready,
/// unhealthy, and warmup-timeout-cancellation paths alike.
struct CreatedPodReclaim {
    api: RunpodApi,
    pod_id: String,
    streak: Arc<Mutex<HashMap<String, i64>>>,
    circuit: Arc<Mutex<HashMap<String, f64>>>,
    threshold: i64,
    cooldown: f64,
    model: String,
    armed: bool,
}

impl CreatedPodReclaim {
    /// The pod became healthy: do not reclaim it.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CreatedPodReclaim {
    fn drop(&mut self) {
        if self.armed {
            spawn_reclaim(
                self.api.clone(),
                std::mem::take(&mut self.pod_id),
                Arc::clone(&self.streak),
                Arc::clone(&self.circuit),
                self.threshold,
                self.cooldown,
                std::mem::take(&mut self.model),
            );
        }
    }
}

/// Finds, resumes, or creates a pod matching the configured model.
pub struct DiscoveryPodLifecycle {
    config: Arc<Config>,
    api: RunpodApi,
    probe: Arc<dyn ProbeClient>,
    target: Arc<Mutex<UpstreamTarget>>,
    state: Arc<Mutex<EndpointState>>,
    last_error: Mutex<String>,
    /// model -> created pod id. Kept across warmup retries so a failed health
    /// probe never causes a second billable create.
    created_pod_ids: Mutex<HashMap<String, String>>,
    /// EXITED pods this proxy itself resumed (start + probe). Together with
    /// `created_pod_ids` they define ownership for `stop()`.
    started_pod_ids: Mutex<HashSet<String>>,
    /// Set when a migration-blocked pod was terminated under replace.
    migrate_replaced: Mutex<bool>,
    active_model: Mutex<String>,
    pending_stops: Mutex<HashSet<String>>,
    reclaim_streak: Arc<Mutex<HashMap<String, i64>>>,
    circuit_open_until: Arc<Mutex<HashMap<String, f64>>>,
    template_name_to_id: Mutex<Option<HashMap<String, String>>>,
}

impl DiscoveryPodLifecycle {
    /// Build a discovery lifecycle over a shared client and endpoint state.
    pub fn new(
        config: Arc<Config>,
        api: RunpodApi,
        probe: Arc<dyn ProbeClient>,
        target: Arc<Mutex<UpstreamTarget>>,
        state: Arc<Mutex<EndpointState>>,
    ) -> Self {
        let default_model = config.default_model();
        Self {
            config,
            api,
            probe,
            target,
            state,
            last_error: Mutex::new(String::new()),
            created_pod_ids: Mutex::new(HashMap::new()),
            started_pod_ids: Mutex::new(HashSet::new()),
            migrate_replaced: Mutex::new(false),
            active_model: Mutex::new(default_model),
            pending_stops: Mutex::new(HashSet::new()),
            reclaim_streak: Arc::new(Mutex::new(HashMap::new())),
            circuit_open_until: Arc::new(Mutex::new(HashMap::new())),
            template_name_to_id: Mutex::new(None),
        }
    }

    /// The model the router is currently serving (set by the router in WI-10).
    pub fn active_model(&self) -> String {
        self.active_model.lock().expect("active_model lock").clone()
    }

    /// Switch the active model (called by the router on a model switch).
    pub fn set_active_model(&self, value: &str) {
        *self.active_model.lock().expect("active_model lock") = value.to_string();
    }

    /// The last warmup error, for `/_status`.
    pub fn last_error(&self) -> String {
        self.last_error.lock().expect("last_error lock").clone()
    }

    /// Seconds remaining on the create circuit breaker for the active model.
    pub fn circuit_breaker_open_s(&self) -> f64 {
        let until = self
            .circuit_open_until
            .lock()
            .expect("circuit lock")
            .get(&self.active_model())
            .copied()
            .unwrap_or(0.0);
        (until - now_secs()).max(0.0)
    }

    /// The upstream URL for a pod at the given port.
    pub fn url(pod: &Pod, port: i64) -> String {
        format!("https://{}-{}.proxy.runpod.net", pod.id, port)
    }

    fn set_last_error(&self, msg: &str) {
        if !msg.is_empty() {
            *self.last_error.lock().expect("last_error lock") = msg.to_string();
        }
    }

    fn spec(&self) -> Option<ModelSpec> {
        self.config.catalogue().get(&self.active_model()).cloned()
    }

    fn port(&self) -> i64 {
        self.spec()
            .and_then(|s| s.port)
            .unwrap_or(self.config.pod_port)
    }

    fn container_disk_gb(&self) -> Option<i64> {
        self.spec()
            .and_then(|s| s.container_disk_gb)
            .or(self.config.container_disk_gb)
    }

    fn volume_gb(&self) -> Option<i64> {
        self.spec()
            .and_then(|s| s.volume_gb)
            .or(self.config.volume_gb)
    }

    fn cloud_type(&self) -> String {
        self.spec()
            .and_then(|s| s.cloud_type.clone())
            .unwrap_or_else(|| self.config.cloud_type.clone())
    }

    fn datacenter_ids(&self) -> Vec<String> {
        self.spec()
            .map(|s| s.datacenters.clone())
            .unwrap_or_default()
    }

    /// (`gpu_type_id`, `count`) pairs for the creation matrix.
    fn gpu_matrix(&self) -> Vec<(String, i64)> {
        if let Some(spec) = self.spec() {
            spec.gpus
                .iter()
                .flat_map(|g| g.counts().into_iter().map(move |c| (g.id.clone(), c)))
                .collect()
        } else {
            self.config
                .gpu_type_ids
                .iter()
                .map(|id| (id.clone(), 1))
                .collect()
        }
    }

    /// Load the template name -> id map (casefolded names), cached after the
    /// first call. `force` bypasses the cache to re-list (used when a wanted
    /// template name is missing from the cached map).
    async fn load_template_map(
        &self,
        force: bool,
    ) -> Result<HashMap<String, String>, RunpodApiError> {
        if !force {
            if let Some(map) = self
                .template_name_to_id
                .lock()
                .expect("template map lock")
                .as_ref()
            {
                return Ok(map.clone());
            }
        }
        let templates = self.api.list_templates(true, true).await?;
        let map: HashMap<String, String> = templates
            .iter()
            .filter(|t| !t.name.is_empty())
            .map(|t| (t.name.to_lowercase(), t.id.clone()))
            .collect();
        *self.template_name_to_id.lock().expect("template map lock") = Some(map.clone());
        Ok(map)
    }

    /// Template ids for the active model's declared templates (spec §6.8c,
    /// rule a). Resolves the catalogue spec's `templates` names against the
    /// (casefolded) template name -> id map; re-lists once if any wanted name
    /// is missing from the cached map.
    async fn template_ids_for_active_model(&self) -> HashSet<String> {
        let Some(spec) = self.spec() else {
            return HashSet::new();
        };
        let wanted: Vec<String> = spec.templates.iter().map(|t| t.to_lowercase()).collect();
        let mut map = match self.load_template_map(false).await {
            Ok(map) => map,
            Err(e) => {
                self.set_last_error(&e.to_string());
                return HashSet::new();
            }
        };
        let resolve = |map: &HashMap<String, String>| {
            wanted
                .iter()
                .filter_map(|n| map.get(n).cloned())
                .collect::<HashSet<String>>()
        };
        let mut ids = resolve(&map);
        if wanted.iter().any(|n| !map.contains_key(n)) {
            if let Ok(fresh) = self.load_template_map(true).await {
                map = fresh;
                ids = resolve(&map);
            }
        }
        ids
    }

    /// Why a pod matches the active model, or an empty string. The precise
    /// template-id match (spec §6.8c rule a) takes precedence over the legacy
    /// name/image/env slug heuristic (rule b).
    fn match_reason(&self, pod: &Pod, template_ids: &HashSet<String>) -> String {
        if let Some(tid) = &pod.template_id {
            if template_ids.contains(tid) {
                return "template id".into();
            }
        }
        if pod_matches_model(pod, &self.active_model()) {
            return "name/image/env".into();
        }
        String::new()
    }

    /// Templates eligible for creation (spec §6.8d), non-serverless only:
    /// 1. `RUNPOD_TEMPLATE_NAME` set -> that exact template (casefolded), or
    ///    none (no fallthrough).
    /// 2. Otherwise the active model's catalogue spec `templates`, in declared
    ///    order (unknown names logged and skipped; no fallthrough).
    /// 3. Otherwise the legacy slug heuristic: the first template whose
    ///    name/image/env matches the active model, or none.
    fn select_create_templates(&self, templates: &[Template]) -> Vec<Template> {
        let non_serverless: Vec<&Template> =
            templates.iter().filter(|t| !t.is_serverless).collect();

        // 1. Explicit template override.
        let override_name = self.config.template_name.trim();
        if !override_name.is_empty() {
            let wanted = override_name.to_lowercase();
            return non_serverless
                .into_iter()
                .find(|t| t.name.to_lowercase() == wanted)
                .cloned()
                .into_iter()
                .collect();
        }

        // 2. Catalogue spec templates, in declared order.
        if let Some(spec) = self.spec() {
            let mut resolved: Vec<Template> = Vec::new();
            for name in &spec.templates {
                let wanted = name.to_lowercase();
                if let Some(t) = non_serverless
                    .iter()
                    .copied()
                    .find(|t| t.name.to_lowercase() == wanted)
                {
                    resolved.push(t.clone());
                } else {
                    tracing::warn!(
                        template = %name,
                        "catalogue template not found in template list; skipping"
                    );
                }
            }
            return resolved;
        }

        // 3. Legacy slug heuristic.
        let model = self.active_model();
        non_serverless
            .into_iter()
            .find(|t| template_matches_model(t, &model))
            .cloned()
            .into_iter()
            .collect()
    }

    /// Try the creation matrix: for each template, for each (gpu, count), try
    /// to create a pod. Returns the first successful pod. A capacity error is
    /// surfaced (plan: surfaced as `LifecycleError`); other errors continue.
    async fn create_from_matrix(
        &self,
        templates: &[Template],
    ) -> Result<Option<Pod>, RunpodApiError> {
        let model = self.active_model();
        let name = model_slug(&model);
        let cloud_type = self.cloud_type();
        let container_disk_gb = self.container_disk_gb();
        let volume_gb = self.volume_gb();
        let datacenter_ids = self.datacenter_ids();
        let matrix = self.gpu_matrix();

        for template in templates {
            for (gpu_type, count) in &matrix {
                let spec = CreatePodSpec {
                    name: name.clone(),
                    template_id: Some(template.id.clone()),
                    image_name: None,
                    gpu_type: gpu_type.clone(),
                    gpu_count: *count,
                    cloud_type: cloud_type.clone(),
                    ports: Vec::new(),
                    env: None,
                    container_disk_gb,
                    volume_gb,
                    volume_id: None,
                    volume_mount_path: String::new(),
                    datacenter_ids: datacenter_ids.clone(),
                };
                match self.api.create_pod(&spec).await {
                    Ok(pod) => {
                        self.created_pod_ids
                            .lock()
                            .expect("created lock")
                            .insert(model.clone(), pod.id.clone());
                        return Ok(Some(pod));
                    }
                    Err(RunpodApiError::Capacity { message, body }) => {
                        return Err(RunpodApiError::Capacity { message, body });
                    }
                    Err(e) => {
                        tracing::warn!(
                            template = %template.name,
                            gpu = %gpu_type,
                            count,
                            %e,
                            "create failed; trying next combination"
                        );
                    }
                }
            }
        }
        Ok(None)
    }

    /// Log non-matching pods (observability).
    fn log_nonmatching(&self, pods: &[Pod], template_ids: &HashSet<String>) {
        for pod in pods {
            if self.match_reason(pod, template_ids).is_empty() {
                tracing::debug!(
                    pod = %pod.id,
                    status = %pod.desired_status,
                    "non-matching pod ignored"
                );
            }
        }
    }

    /// Probe a pod's health until healthy or the deadline passes.
    async fn healthy(&self, pod: &Pod, deadline: Option<Instant>) -> bool {
        let base = Self::url(pod, self.port());
        let path = self.config.warmup_path.trim_start_matches('/');
        let url = if path.is_empty() {
            base.clone()
        } else {
            format!("{base}/{path}")
        };
        let deadline = deadline.unwrap_or_else(|| {
            Instant::now() + Duration::from_secs_f64(self.config.pod_health_timeout_s)
        });
        let mut backoff: f64 = 1.0;
        loop {
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .as_secs_f64();
            let headers = self.config.auth_headers_for_pod(&pod.id);
            match self.probe.get(&url, &headers).await {
                Ok(response) => {
                    let model = self.active_model();
                    let (ok, reason) = classify(
                        self.probe.as_ref(),
                        &self.config.pod_health_mode,
                        true,
                        &response,
                        &base,
                        &self.config.warmup_path,
                        &model,
                        &headers,
                        10.0_f64.min(remaining.max(0.01_f64)),
                    )
                    .await;
                    if ok {
                        return true;
                    }
                    self.set_last_error(&reason);
                }
                Err(e) => {
                    self.set_last_error(&e.0);
                }
            }
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .as_secs_f64();
            if remaining <= 0.0 {
                return false;
            }
            tokio::time::sleep(Duration::from_secs_f64(backoff.min(remaining))).await;
            backoff = (backoff * 2.0_f64).min(self.config.warmup_backoff_max_s);
        }
    }

    /// Wait for a pod to become RUNNING and healthy, or the deadline passes.
    async fn ready(&self, pod_id: &str, deadline: Option<Instant>) -> Option<Pod> {
        let deadline = deadline.unwrap_or_else(|| {
            Instant::now() + Duration::from_secs_f64(self.config.pod_ready_timeout_s)
        });
        let mut backoff: f64 = 1.0;
        loop {
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .as_secs_f64();
            if remaining <= 0.0 {
                return None;
            }
            match self.api.get_pod(pod_id).await {
                Ok(Some(pod)) if pod.desired_status == RUNNING => {
                    if self.healthy(&pod, Some(deadline)).await {
                        return Some(pod);
                    }
                    // RUNNING but unhealthy: the caller's create-path reclaim
                    // guard stops it (single owner of created-pod reclaim).
                    self.set_last_error("created pod became unhealthy");
                    return None;
                }
                Ok(Some(pod)) if pod.desired_status == TERMINATED => {
                    self.set_last_error("pod terminated before becoming ready");
                    return None;
                }
                Ok(_) => {}
                Err(e) => {
                    self.set_last_error(&e.to_string());
                }
            }
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .as_secs_f64();
            if remaining <= 0.0 {
                return None;
            }
            tokio::time::sleep(Duration::from_secs_f64(backoff.min(remaining))).await;
            backoff = (backoff * 2.0_f64).min(self.config.warmup_backoff_max_s);
        }
    }

    /// Reset the reclaim streak (a healthy pod clears the breaker).
    fn note_healthy(&self) {
        let model = self.active_model();
        self.reclaim_streak
            .lock()
            .expect("reclaim streak lock")
            .insert(model, 0);
    }

    /// Fail fast if the create circuit breaker is open.
    fn check_circuit_breaker(&self) -> Result<(), LifecycleError> {
        let open_s = self.circuit_breaker_open_s();
        if open_s > 0.0 {
            return Err(LifecycleError::general(format!(
                "pod create circuit breaker open for {open_s:.0}s"
            )));
        }
        Ok(())
    }

    /// Reclaim a pod, shielded from cancellation of the warmup.
    fn reclaim_before_cancel(&self, pod_id: &str) {
        spawn_reclaim(
            self.api.clone(),
            pod_id.to_string(),
            Arc::clone(&self.reclaim_streak),
            Arc::clone(&self.circuit_open_until),
            self.config.pod_circuit_breaker_threshold,
            self.config.pod_circuit_breaker_cooldown_s,
            self.active_model(),
        );
    }

    /// Build the create-path reclaim guard for a freshly created pod.
    fn created_pod_reclaim(&self, pod_id: &str) -> CreatedPodReclaim {
        CreatedPodReclaim {
            api: self.api.clone(),
            pod_id: pod_id.to_string(),
            streak: Arc::clone(&self.reclaim_streak),
            circuit: Arc::clone(&self.circuit_open_until),
            threshold: self.config.pod_circuit_breaker_threshold,
            cooldown: self.config.pod_circuit_breaker_cooldown_s,
            model: self.active_model(),
            armed: true,
        }
    }

    /// Start a pod, retrying on transient errors.
    async fn start_pod_with_retry(&self, pod_id: &str) -> Result<(), RunpodApiError> {
        let mut attempts = 0;
        loop {
            match self.api.start_pod(pod_id).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    attempts += 1;
                    if attempts >= 3 {
                        return Err(e);
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    /// Resume an EXITED pod and probe its health.
    async fn resume_and_probe(&self, pod: &Pod, deadline: Option<Instant>) -> bool {
        if let Err(e) = self.start_pod_with_retry(&pod.id).await {
            tracing::warn!(pod = %pod.id, %e, "failed to resume exited pod");
            return false;
        }
        self.started_pod_ids
            .lock()
            .expect("started lock")
            .insert(pod.id.clone());
        self.state.lock().expect("state lock").pod_starts += 1;
        if self.healthy(pod, deadline).await {
            true
        } else {
            // The pod was started but is unhealthy: reclaim it (shielded).
            self.reclaim_before_cancel(&pod.id);
            false
        }
    }

    /// Terminate a pod (for the replace policy).
    async fn terminate_pod(&self, pod_id: &str) {
        if let Err(e) = self.api.delete_pod(pod_id).await {
            tracing::warn!(%pod_id, %e, "failed to terminate pod");
        }
    }

    /// Step 1: adopt a matching RUNNING pod that is healthy. Returns true on
    /// success; sets "existing pod unhealthy" if a matching pod is unhealthy.
    async fn adopt_running(
        &self,
        pods: &[Pod],
        template_ids: &HashSet<String>,
        deadline: Instant,
    ) -> bool {
        let running: Vec<&Pod> = pods
            .iter()
            .filter(|p| {
                p.desired_status == RUNNING && !self.match_reason(p, template_ids).is_empty()
            })
            .collect();
        let Some(pod) = running.first() else {
            return false;
        };
        if self.healthy(pod, Some(deadline)).await {
            self.target
                .lock()
                .expect("target lock")
                .set(&Self::url(pod, self.port()), &pod.id);
            self.note_healthy();
            return true;
        }
        if self.last_error().is_empty() {
            self.set_last_error("existing pod unhealthy");
        }
        false
    }

    /// Step 2: resume a matching EXITED pod. Returns true if one was adopted.
    async fn resume_exited(
        &self,
        pods: &[Pod],
        template_ids: &HashSet<String>,
        deadline: Instant,
    ) -> bool {
        let exited: Vec<&Pod> = pods
            .iter()
            .filter(|p| {
                p.desired_status == EXITED && !self.match_reason(p, template_ids).is_empty()
            })
            .collect();
        for pod in exited {
            if self.resume_and_probe(pod, Some(deadline)).await {
                self.target
                    .lock()
                    .expect("target lock")
                    .set(&Self::url(pod, self.port()), &pod.id);
                self.note_healthy();
                return true;
            }
        }
        false
    }

    /// Step 4: create a pod from the matrix and wait for it to become ready.
    /// The created pod is reclaimed (RAII) on every non-healthy exit.
    async fn create_and_ready(
        &self,
        templates: &[Template],
        deadline: Instant,
    ) -> Result<(), LifecycleError> {
        let selected = self.select_create_templates(templates);
        if selected.is_empty() {
            if self.last_error().is_empty() {
                self.set_last_error("no templates available for creation");
            }
            return Err(LifecycleError::general(self.last_error()));
        }
        let pod = match self.create_from_matrix(&selected).await {
            Ok(pod) => pod,
            Err(RunpodApiError::Capacity { message, .. }) => {
                self.set_last_error(&message);
                return Err(LifecycleError::general(message));
            }
            Err(e) => {
                self.set_last_error(&e.to_string());
                return Err(LifecycleError::general(format!("discovery failed: {e}")));
            }
        };
        let Some(pod) = pod else {
            if self.last_error().is_empty() {
                self.set_last_error("all creation combinations failed");
            }
            return Err(LifecycleError::general(self.last_error()));
        };
        self.state.lock().expect("state lock").pod_creates += 1;
        // Cost-safety: the created pod must be reclaimed on every non-healthy
        // exit from here, including a warmup-timeout cancellation that drops
        // this future mid-`ready()`. The RAII guard does this on drop.
        let mut reclaim = self.created_pod_reclaim(&pod.id);
        if let Some(pod) = self.ready(&pod.id, Some(deadline)).await {
            self.target
                .lock()
                .expect("target lock")
                .set(&Self::url(&pod, self.port()), &pod.id);
            self.note_healthy();
            reclaim.disarm();
            return Ok(());
        }
        let msg = self.last_error();
        let msg = if msg.is_empty() {
            "created pod did not become healthy".to_string()
        } else {
            msg
        };
        Err(LifecycleError::general(msg))
    }
}

#[async_trait]
impl Lifecycle for DiscoveryPodLifecycle {
    async fn start(&self, budget: f64) -> Result<(), LifecycleError> {
        let deadline = Instant::now() + Duration::from_secs_f64(budget.max(0.0));
        self.check_circuit_breaker()?;
        let (pods, templates) = match (
            self.api.list_pods(None).await,
            self.api.list_templates(true, true).await,
        ) {
            (Ok(pods), Ok(templates)) => (pods, templates),
            (Err(e), _) | (_, Err(e)) => {
                self.set_last_error(&e.to_string());
                return Err(LifecycleError::general(format!("discovery failed: {e}")));
            }
        };
        let template_ids = self.template_ids_for_active_model().await;
        self.log_nonmatching(&pods, &template_ids);

        // 1. A matching RUNNING pod that is healthy wins.
        if self.adopt_running(&pods, &template_ids, deadline).await {
            return Ok(());
        }

        // 2. A matching EXITED pod we can resume.
        if self.resume_exited(&pods, &template_ids, deadline).await {
            return Ok(());
        }

        // 3. Replace policy: terminate non-matching RUNNING pods.
        if self.config.on_migrate == "replace" {
            for pod in &pods {
                if pod.desired_status == RUNNING && self.match_reason(pod, &template_ids).is_empty()
                {
                    tracing::warn!(pod = %pod.id, "terminating non-matching pod for replace");
                    self.terminate_pod(&pod.id).await;
                    *self.migrate_replaced.lock().expect("migrate lock") = true;
                    self.state.lock().expect("state lock").pods_replaced += 1;
                }
            }
        }

        // 4. Create from the matrix.
        self.create_and_ready(&templates, deadline).await
    }

    async fn stop(&self) -> Result<(), LifecycleError> {
        let owned: Vec<String> = {
            let created = self
                .created_pod_ids
                .lock()
                .expect("created lock")
                .values()
                .cloned()
                .collect::<Vec<_>>();
            let started = self
                .started_pod_ids
                .lock()
                .expect("started lock")
                .iter()
                .cloned()
                .collect::<Vec<_>>();
            let mut all: HashSet<String> = created.into_iter().collect();
            all.extend(started);
            all.into_iter().collect()
        };
        for pod_id in &owned {
            if let Err(e) = self.api.stop_pod(pod_id).await {
                tracing::warn!(%pod_id, %e, "stop failed; will retry");
                self.pending_stops
                    .lock()
                    .expect("pending lock")
                    .insert(pod_id.clone());
            }
        }
        Ok(())
    }

    async fn retry_pending_stops(&self) {
        let pending: Vec<String> = self
            .pending_stops
            .lock()
            .expect("pending lock")
            .iter()
            .cloned()
            .collect();
        for pod_id in pending {
            match self.api.stop_pod(&pod_id).await {
                Ok(()) => {
                    self.pending_stops
                        .lock()
                        .expect("pending lock")
                        .remove(&pod_id);
                }
                Err(e) => {
                    tracing::warn!(%pod_id, %e, "retry stop failed");
                }
            }
        }
    }

    fn active_model(&self) -> String {
        self.active_model()
    }

    fn supports_switch(&self) -> bool {
        true
    }

    fn set_active_model(&self, model: &str) {
        self.set_active_model(model);
    }

    fn discovery_view(&self) -> Option<crate::lifecycle::DiscoveryView> {
        Some(crate::lifecycle::DiscoveryView {
            last_error: self.last_error(),
            circuit_breaker_open_s: self.circuit_breaker_open_s(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::health::{ProbeClient, ProbeError, ProbeResponse};
    use crate::runpod_api::RunpodApi;
    use crate::state::EndpointState;
    use crate::target::UpstreamTarget;
    use async_trait::async_trait;
    use std::collections::BTreeMap;

    /// A scripted probe client that returns a fixed response.
    struct ScriptedProbe {
        status: u16,
        body: String,
    }

    #[async_trait]
    impl ProbeClient for ScriptedProbe {
        async fn get(
            &self,
            _url: &str,
            _headers: &[(String, String)],
        ) -> Result<ProbeResponse, ProbeError> {
            Ok(ProbeResponse {
                status: self.status,
                body: self.body.clone(),
            })
        }

        async fn post_json(
            &self,
            _url: &str,
            _body: &serde_json::Value,
            _headers: &[(String, String)],
            _timeout: f64,
        ) -> Result<ProbeResponse, ProbeError> {
            Ok(ProbeResponse {
                status: self.status,
                body: self.body.clone(),
            })
        }
    }

    fn test_config(rest_url: &str, v2_url: &str, model: &str, allow_create: bool) -> Config {
        let mut env = BTreeMap::new();
        env.insert("RUNPOD_MODE".into(), "pod".into());
        env.insert("RUNPOD_API_KEY".into(), "test-key".into());
        env.insert("RUNPOD_MODEL_NAME".into(), model.into());
        env.insert("RUNPOD_REST_API_URL".into(), rest_url.into());
        env.insert("RUNPOD_AVAILABILITY_API_URL".into(), v2_url.into());
        env.insert("RUNPOD_ALLOW_POD_CREATE".into(), allow_create.to_string());
        env.insert("RUNPOD_POD_HEALTH_TIMEOUT_S".into(), "0.1".into());
        env.insert("RUNPOD_POD_READY_TIMEOUT_S".into(), "0.1".into());
        env.insert("WARMUP_BACKOFF_MAX_S".into(), "0.01".into());
        env.insert("WARMUP_TIMEOUT_S".into(), "0.5".into());
        env.insert("RUNPOD_GPU_TYPE_IDS".into(), "A".into());
        env.insert(
            "RUNPOD_MODELS_JSON".into(),
            format!(
                r#"{{"models":[{{"name":"{model}","templates":["{model}"],"gpus":[{{"id":"A","min":1,"max":1}}]}}]}}"#
            ),
        );
        Config::from_env_map(&env).expect("test config")
    }

    fn make_lifecycle(
        config: Config,
        api: RunpodApi,
        probe: Arc<dyn ProbeClient>,
    ) -> (
        DiscoveryPodLifecycle,
        Arc<Mutex<UpstreamTarget>>,
        Arc<Mutex<EndpointState>>,
    ) {
        let target = Arc::new(Mutex::new(UpstreamTarget::new("", "")));
        let state = Arc::new(Mutex::new(EndpointState::default()));
        let lc =
            DiscoveryPodLifecycle::new(Arc::new(config), api, probe, target.clone(), state.clone());
        (lc, target, state)
    }

    #[tokio::test]
    async fn test_failed_discovery_creates_at_most_one_pod() {
        let server = httpmock::MockServer::start();
        // No existing pods.
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/pods");
            then.status(200).json_body(serde_json::json!([]));
        });
        // Template list.
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/templates");
            then.status(200)
                .json_body(serde_json::json!([{"id":"tpl","name":"llama-3","imageName":"llama-3:latest","env":{},"ports":["8000/http"],"isServerless":false}]));
        });
        // Create pod returns EXITED.
        let create_mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/v2/pods");
            then.status(201)
                .json_body(serde_json::json!({"id":"created","name":"llama-3","desiredStatus":"EXITED","image":"llama-3:latest","templateId":"tpl","env":{},"ports":["8000/http"]}));
        });
        // Get created pod returns EXITED.
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/pods/created");
            then.status(200)
                .json_body(serde_json::json!({"id":"created","name":"llama-3","desiredStatus":"EXITED","image":"llama-3:latest","templateId":"tpl","env":{},"ports":["8000/http"]}));
        });
        // Start pod.
        server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/pods/created/start");
            then.status(200);
        });

        let config = test_config(&server.url("/v1"), &server.url("/v2"), "llama-3", true);
        let api = RunpodApi::new(
            &config.rest_api_url,
            &config.availability_api_url,
            &config.api_key,
            crate::test_client(),
        );
        // Probe always fails (unhealthy).
        let probe: Arc<dyn ProbeClient> = Arc::new(ScriptedProbe {
            status: 503,
            body: String::new(),
        });
        let (lc, _, _) = make_lifecycle(config, api, probe);

        let result = lc.start(0.5).await;
        assert!(result.is_err(), "discovery should fail for unhealthy pod");
        // At most one create call.
        assert_eq!(create_mock.hits(), 1, "exactly one pod should be created");
    }

    #[tokio::test]
    async fn test_created_pod_that_never_becomes_ready_is_reclaimed() {
        let server = httpmock::MockServer::start();
        // No existing pods.
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/pods");
            then.status(200).json_body(serde_json::json!([]));
        });
        // Template list.
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/templates");
            then.status(200)
                .json_body(serde_json::json!([{"id":"tpl","name":"llama-3","imageName":"llama-3:latest","env":{},"ports":["8000/http"],"isServerless":false}]));
        });
        // Create pod succeeds.
        let create_mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/v2/pods");
            then.status(201)
                .json_body(serde_json::json!({"id":"created","name":"llama-3","desiredStatus":"STARTING","image":"llama-3:latest","templateId":"tpl","env":{},"ports":["8000/http"]}));
        });
        // The created pod is stuck STARTING — never becomes RUNNING.
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/pods/created");
            then.status(200)
                .json_body(serde_json::json!({"id":"created","name":"llama-3","desiredStatus":"STARTING","image":"llama-3:latest","templateId":"tpl","env":{},"ports":["8000/http"]}));
        });
        // The cost-safety invariant: the created-but-never-healthy pod MUST be
        // reclaimed (stopped) on the failure path (spec section 11, item 21).
        let stop_mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/pods/created/stop");
            then.status(200);
        });

        let config = test_config(&server.url("/v1"), &server.url("/v2"), "llama-3", true);
        let api = RunpodApi::new(
            &config.rest_api_url,
            &config.availability_api_url,
            &config.api_key,
            crate::test_client(),
        );
        let probe: Arc<dyn ProbeClient> = Arc::new(ScriptedProbe {
            status: 503,
            body: String::new(),
        });
        let (lc, _, _) = make_lifecycle(config, api, probe);

        let result = lc.start(0.2).await;
        assert!(result.is_err(), "discovery should fail for a stuck pod");
        assert_eq!(create_mock.hits(), 1, "exactly one pod should be created");
        // Allow the shielded reclaim task to complete.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            stop_mock.hits(),
            1,
            "created-but-never-healthy pod must be reclaimed exactly once"
        );
    }

    #[tokio::test]
    async fn test_existing_running_unhealthy_pod_is_not_stopped() {
        let server = httpmock::MockServer::start();
        // Existing RUNNING pod.
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/pods");
            then.status(200)
                .json_body(serde_json::json!([{"id":"live","name":"llama-3","desiredStatus":"RUNNING","image":"llama-3:latest","templateId":"tpl","env":{},"ports":["8000/http"]}]))
                ;
        });
        // Get live pod.
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/pods/live");
            then.status(200)
                .json_body(serde_json::json!({"id":"live","name":"llama-3","desiredStatus":"RUNNING","image":"llama-3:latest","templateId":"tpl","env":{},"ports":["8000/http"]}));
        });
        // Stop live pod (should NOT be called).
        let stop_mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/pods/live/stop");
            then.status(200);
        });

        let config = test_config(&server.url("/v1"), &server.url("/v2"), "llama-3", false);
        let api = RunpodApi::new(
            &config.rest_api_url,
            &config.availability_api_url,
            &config.api_key,
            crate::test_client(),
        );
        // Probe always fails (unhealthy).
        let probe: Arc<dyn ProbeClient> = Arc::new(ScriptedProbe {
            status: 503,
            body: String::new(),
        });
        let (lc, _, _) = make_lifecycle(config, api, probe);

        let result = lc.start(0.12).await;
        assert!(result.is_err(), "discovery should fail for unhealthy pod");
        // The found RUNNING pod must NOT be stopped.
        assert_eq!(stop_mock.hits(), 0, "found RUNNING pod must not be stopped");
    }

    #[tokio::test]
    async fn test_circuit_breaker_opens_after_repeated_reclaims() {
        let server = httpmock::MockServer::start();
        // No RUNNING pods, one EXITED pod.
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/pods");
            then.status(200)
                .json_body(serde_json::json!([{"id":"old","name":"llama-3","desiredStatus":"EXITED","image":"llama-3:latest","templateId":"tpl","env":{},"ports":["8000/http"]}]))
                ;
        });
        // Template list.
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/templates");
            then.status(200)
                .json_body(serde_json::json!([{"id":"tpl","name":"llama-3","imageName":"llama-3:latest","env":{},"ports":["8000/http"],"isServerless":false}]));
        });
        // Get old pod.
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/v1/pods/old");
            then.status(200)
                .json_body(serde_json::json!({"id":"old","name":"llama-3","desiredStatus":"EXITED","image":"llama-3:latest","templateId":"tpl","env":{},"ports":["8000/http"]}));
        });
        // Start old pod.
        server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/pods/old/start");
            then.status(200);
        });
        // Stop old pod (reclaim).
        let stop_mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/pods/old/stop");
            then.status(200);
        });

        let mut env = BTreeMap::new();
        env.insert("RUNPOD_MODE".into(), "pod".into());
        env.insert("RUNPOD_API_KEY".into(), "test-key".into());
        env.insert("RUNPOD_MODEL_NAME".into(), "llama-3".into());
        env.insert("RUNPOD_REST_API_URL".into(), server.url("/v1"));
        env.insert("RUNPOD_AVAILABILITY_API_URL".into(), server.url("/v2"));
        env.insert("RUNPOD_ALLOW_POD_CREATE".into(), "false".into());
        env.insert("RUNPOD_POD_HEALTH_TIMEOUT_S".into(), "0.05".into());
        env.insert("RUNPOD_POD_READY_TIMEOUT_S".into(), "0.05".into());
        env.insert("WARMUP_BACKOFF_MAX_S".into(), "0.01".into());
        env.insert("WARMUP_TIMEOUT_S".into(), "0.3".into());
        env.insert("RUNPOD_GPU_TYPE_IDS".into(), "A".into());
        env.insert("POD_CIRCUIT_BREAKER_THRESHOLD".into(), "2".into());
        env.insert("POD_CIRCUIT_BREAKER_COOLDOWN_S".into(), "100.0".into());
        env.insert(
            "RUNPOD_MODELS_JSON".into(),
            r#"{"models":[{"name":"llama-3","templates":["llama-3"],"gpus":[{"id":"A","min":1,"max":1}]}]}"#.into(),
        );
        let config = Config::from_env_map(&env).expect("test config");
        let api = RunpodApi::new(
            &config.rest_api_url,
            &config.availability_api_url,
            &config.api_key,
            crate::test_client(),
        );
        // Probe always fails (unhealthy).
        let probe: Arc<dyn ProbeClient> = Arc::new(ScriptedProbe {
            status: 503,
            body: String::new(),
        });
        let (lc, _, _) = make_lifecycle(config, api, probe);

        // Two failed attempts to open the circuit breaker.
        for _ in 0..2 {
            let result = lc.start(0.3).await;
            assert!(result.is_err(), "discovery should fail for unhealthy pod");
            // Allow the spawned reclaim task to complete.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // Circuit breaker should now be open.
        assert!(
            lc.circuit_breaker_open_s() > 0.0,
            "circuit breaker should be open after repeated reclaims"
        );

        // Next start() should fail fast.
        let stops_before = stop_mock.hits();
        let result = lc.start(0.3).await;
        assert!(
            result.is_err(),
            "start should fail fast with circuit breaker open"
        );
        let stops_after = stop_mock.hits();
        assert_eq!(
            stops_before, stops_after,
            "no additional stop calls when circuit breaker is open"
        );
    }

    /// §6.8d candidate ordering: a `RUNPOD_TEMPLATE_NAME` override wins with no
    /// fallthrough; otherwise the catalogue spec's templates are used in
    /// declared order (serverless excluded, no fallthrough to the legacy slug).
    #[tokio::test]
    async fn test_select_create_templates_follows_spec_order() {
        let server = httpmock::MockServer::start();
        let probe: Arc<dyn ProbeClient> = Arc::new(ScriptedProbe {
            status: 200,
            body: String::new(),
        });

        let templates = vec![
            Template {
                id: "t-alpha".into(),
                name: "alpha".into(),
                image_name: "alpha:latest".into(),
                env: BTreeMap::new(),
                ports: vec!["8000/http".into()],
                is_serverless: false,
                args: String::new(),
            },
            Template {
                id: "t-beta".into(),
                name: "beta".into(),
                image_name: "beta:latest".into(),
                env: BTreeMap::new(),
                ports: vec!["8000/http".into()],
                is_serverless: false,
                args: String::new(),
            },
            Template {
                id: "t-gamma".into(),
                name: "gamma".into(),
                image_name: "gamma:latest".into(),
                env: BTreeMap::new(),
                ports: vec!["8000/http".into()],
                is_serverless: true,
                args: String::new(),
            },
        ];

        // The catalogue declares beta then alpha (declared order, not
        // alphabetical), so a correct implementation returns them in that order.
        let base_env = |template_name: Option<&str>| -> BTreeMap<String, String> {
            let mut env = BTreeMap::new();
            env.insert("RUNPOD_MODE".into(), "pod".into());
            env.insert("RUNPOD_API_KEY".into(), "test-key".into());
            env.insert("RUNPOD_MODEL_NAME".into(), "llama-3".into());
            env.insert("RUNPOD_REST_API_URL".into(), server.url("/v1"));
            env.insert("RUNPOD_AVAILABILITY_API_URL".into(), server.url("/v2"));
            env.insert("RUNPOD_ALLOW_POD_CREATE".into(), "true".into());
            env.insert("RUNPOD_GPU_TYPE_IDS".into(), "A".into());
            env.insert(
                "RUNPOD_MODELS_JSON".into(),
                r#"{"models":[{"name":"llama-3","templates":["beta","alpha"],"gpus":[{"id":"A","min":1,"max":1}]}]}"#.into(),
            );
            if let Some(t) = template_name {
                env.insert("RUNPOD_TEMPLATE_NAME".into(), t.into());
            }
            env
        };

        // Case 1: RUNPOD_TEMPLATE_NAME override wins; no fallthrough to the
        // catalogue spec.
        let config = Config::from_env_map(&base_env(Some("beta"))).unwrap();
        let api = RunpodApi::new(
            &config.rest_api_url,
            &config.availability_api_url,
            &config.api_key,
            crate::test_client(),
        );
        let (lc, _, _) = make_lifecycle(config, api, Arc::clone(&probe));
        let selected = lc.select_create_templates(&templates);
        assert_eq!(
            selected.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["beta"]
        );

        // Case 2: no override; the catalogue spec's templates in declared order
        // (beta, alpha), serverless gamma excluded, no fallthrough to legacy.
        let config = Config::from_env_map(&base_env(None)).unwrap();
        let api = RunpodApi::new(
            &config.rest_api_url,
            &config.availability_api_url,
            &config.api_key,
            crate::test_client(),
        );
        let (lc, _, _) = make_lifecycle(config, api, Arc::clone(&probe));
        let selected = lc.select_create_templates(&templates);
        assert_eq!(
            selected.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["beta", "alpha"]
        );
    }
}
