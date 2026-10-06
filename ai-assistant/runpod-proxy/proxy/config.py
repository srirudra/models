"""Environment-based configuration."""
import os
from dataclasses import dataclass, field

from .models_config import (
    ModelCatalogue,
    ModelConfigError,
    load_catalogue,
    validate_reloaded,
)
from .prewarm import parse_prewarm_times


def _float_env(name: str, default: float) -> float:
    raw = os.environ.get(name, "").strip()
    return float(raw) if raw else default


def _bool_env(name: str, default: bool = False) -> bool:
    raw = os.environ.get(name, "").strip().casefold()
    return raw in {"1", "true", "yes", "on"} if raw else default


MODES = frozenset({"serverless", "pod"})
POD_HEALTH_MODES = frozenset({"any", "model", "completion"})
MIGRATE_POLICIES = frozenset({"fail", "replace"})


@dataclass(frozen=True)
class Config:
    serverless_url: str
    api_key: str = ""
    upstream_api_key: str = ""
    port: int = 8080
    warmup_path: str = "v1/models"
    warmup_timeout_s: float = 600.0
    warmup_backoff_max_s: float = 15.0
    keepalive_interval_s: float = 25.0
    idle_giveup_s: float = 300.0
    request_timeout_s: float = 300.0
    log_level: str = "INFO"
    # "text" (default) or "json" (one JSON object per line, for log shippers).
    log_format: str = "text"
    mode: str = "serverless"
    pod_id: str = ""
    pod_port: int = 8000
    pod_url: str = ""
    rest_api_url: str = "https://rest.runpod.io/v1"
    # RunPod v2 API base used by the GPU availability poller
    # (GET {base}/catalog/gpus?include=AVAILABILITY&product=POD).
    availability_api_url: str = "https://api.runpod.io/v2"
    # GPU availability poll interval in seconds (0 disables the poller).
    # On by default in pod mode: one cheap GET per five minutes.
    gpu_availability_interval_s: float = 300.0
    proxy_api_key: str = ""
    model_name: str = ""
    allowed_models: tuple[str, ...] = ()
    model_switch_drain_s: float = 30.0
    allow_pod_create: bool = False
    gpu_type_ids: tuple[str, ...] = ()
    gpu_type_priority: str = "availability"
    cloud_type: str = "SECURE"
    template_name: str = ""
    pod_revalidate_s: float = 300.0
    pod_health_timeout_s: float = 180.0
    pod_ready_timeout_s: float = 120.0
    # Readiness bar for pod-mode warmup (see proxy/health.py):
    #   any        - any non-synthetic HTTP response (legacy; only safe for
    #                servers whose routes genuinely refuse to answer until
    #                the model is loaded, e.g. vLLM's /v1/models)
    #   model      - the warmup route's JSON must list the target model
    #   completion - a 1-token chat completion must actually succeed
    # Default "model" so readiness does not depend on one specific server's
    # early-refusal behavior: a server that answers its HTTP routes while
    # weights are still loading would otherwise be declared WARM too soon,
    # and the first real request would stall for minutes. "completion" is
    # the strictest bar (validates the full inference path) at the cost of
    # a 1-token inference per cold start.
    pod_health_mode: str = "model"
    # What to do when a pinned pod's start is blocked because its original
    # host has no free GPU ("please migrate your pod" prompt, or REST 500
    # "not enough free GPUs on the host machine" — the host assignment is
    # sticky, so start retries can never succeed on their own):
    #   fail    - surface the error; the pod is left untouched
    #   replace - terminate the pod and create a fresh one with the same
    #             spec (same template/GPUs, network volume re-attached so
    #             on-disk data such as HF model weights is not re-downloaded)
    on_migrate: str = "fail"
    container_disk_gb: int | None = None
    volume_gb: int | None = None
    max_create_attempts: int = 12
    pod_circuit_breaker_threshold: int = 3
    pod_circuit_breaker_cooldown_s: float = 300.0
    max_body_bytes: int = 50 * 1024 * 1024
    prewarm_times: tuple[tuple[int, int], ...] = ()
    upstream_api_key_template: str = ""
    allowlist_configured: bool = False
    catalogue: ModelCatalogue = field(default_factory=ModelCatalogue)
    # Where the catalogue came from, so POST /_reload can re-read it while
    # running (file path wins over inline, same as at boot).
    catalogue_file: str = ""
    catalogue_inline: str = ""

    def __post_init__(self) -> None:
        if self.mode not in MODES:
            raise ValueError(f"RUNPOD_MODE must be one of {sorted(MODES)}, got {self.mode!r}")
        if self.pod_health_mode not in POD_HEALTH_MODES:
            raise ValueError(
                f"POD_HEALTH_MODE must be one of {sorted(POD_HEALTH_MODES)}, "
                f"got {self.pod_health_mode!r}"
            )
        if self.on_migrate not in MIGRATE_POLICIES:
            raise ValueError(
                f"RUNPOD_ON_MIGRATE must be one of {sorted(MIGRATE_POLICIES)}, "
                f"got {self.on_migrate!r}"
            )
        if self.catalogue:
            object.__setattr__(self, "allowlist_configured", True)
            if self.model_name and self.catalogue.get(self.model_name) is None:
                raise ModelConfigError(
                    f"RUNPOD_MODEL_NAME ({self.model_name!r}) is not present in the "
                    f"model catalogue; known models: {list(self.catalogue.names)}"
                )
        if self.allowed_models:
            object.__setattr__(self, "allowlist_configured", True)

    @classmethod
    def from_env(cls) -> "Config":
        return cls(
            serverless_url=os.environ.get("RUNPOD_SERVERLESS_URL", "").strip().rstrip("/"),
            api_key=os.environ.get("RUNPOD_API_KEY", "").strip(),
            upstream_api_key=os.environ.get("RUNPOD_UPSTREAM_API_KEY", "").strip(),
            mode=os.environ.get("RUNPOD_MODE", "serverless").strip() or "serverless",
            pod_id=os.environ.get("RUNPOD_POD_ID", "").strip(),
            pod_port=int(_float_env("RUNPOD_POD_PORT", 8000)),
            pod_url=os.environ.get("RUNPOD_POD_URL", "").strip().rstrip("/"),
            rest_api_url=os.environ.get("RUNPOD_REST_URL", "https://rest.runpod.io/v1").strip().rstrip("/"),
            availability_api_url=os.environ.get("RUNPOD_AVAILABILITY_URL", "https://api.runpod.io/v2").strip().rstrip("/"),
            gpu_availability_interval_s=_float_env("GPU_AVAILABILITY_INTERVAL_S", 300.0),
            port=int(_float_env("PORT", 8080)),
            warmup_path=os.environ.get("WARMUP_PATH", "v1/models").strip(),
            warmup_timeout_s=_float_env("WARMUP_TIMEOUT_S", 600.0),
            warmup_backoff_max_s=_float_env("WARMUP_BACKOFF_MAX_S", 15.0),
            keepalive_interval_s=_float_env("KEEPALIVE_INTERVAL_S", 25.0),
            idle_giveup_s=_float_env("IDLE_GIVEUP_S", 300.0),
            request_timeout_s=_float_env("REQUEST_TIMEOUT_S", 300.0),
            log_level=os.environ.get("LOG_LEVEL", "INFO").strip() or "INFO",
            log_format=os.environ.get("LOG_FORMAT", "text").strip().casefold() or "text",
            proxy_api_key=os.environ.get("PROXY_API_KEY", "").strip(),
            model_name=os.environ.get("RUNPOD_MODEL_NAME", "").strip(),
            allowed_models=tuple(x.strip() for x in os.environ.get("RUNPOD_ALLOWED_MODELS", "").split(",") if x.strip()),
            allowlist_configured=bool(os.environ.get("RUNPOD_ALLOWED_MODELS", "").strip()),
            model_switch_drain_s=_float_env("MODEL_SWITCH_DRAIN_S", 30.0),
            allow_pod_create=_bool_env("RUNPOD_ALLOW_POD_CREATE"),
            gpu_type_ids=tuple(x.strip() for x in os.environ.get("RUNPOD_GPU_TYPE_IDS", "").split(",") if x.strip()),
            gpu_type_priority=os.environ.get("RUNPOD_GPU_TYPE_PRIORITY", "availability").strip(),
            cloud_type=os.environ.get("RUNPOD_CLOUD_TYPE", "SECURE").strip(),
            template_name=os.environ.get("RUNPOD_TEMPLATE_NAME", "").strip(),
            pod_revalidate_s=_float_env("POD_REVALIDATE_S", 300.0),
            pod_health_timeout_s=_float_env("POD_HEALTH_TIMEOUT_S", 180.0),
            pod_ready_timeout_s=_float_env("POD_READY_TIMEOUT_S", 120.0),
            pod_health_mode=os.environ.get("POD_HEALTH_MODE", "model").strip().casefold() or "model",
            on_migrate=os.environ.get("RUNPOD_ON_MIGRATE", "fail").strip().casefold() or "fail",
            container_disk_gb=int(_float_env("RUNPOD_CONTAINER_DISK_GB", 0)) if os.environ.get("RUNPOD_CONTAINER_DISK_GB", "").strip() else None,
            volume_gb=int(_float_env("RUNPOD_VOLUME_GB", 0)) if os.environ.get("RUNPOD_VOLUME_GB", "").strip() else None,
            max_create_attempts=int(_float_env("RUNPOD_MAX_CREATE_ATTEMPTS", 12)),
            pod_circuit_breaker_threshold=int(_float_env("POD_CIRCUIT_BREAKER_THRESHOLD", 3)),
            pod_circuit_breaker_cooldown_s=_float_env("POD_CIRCUIT_BREAKER_COOLDOWN_S", 300.0),
            max_body_bytes=int(_float_env("MAX_BODY_BYTES", 50 * 1024 * 1024)),
            prewarm_times=parse_prewarm_times(os.environ.get("PREWARM_TIMES", "").strip()),
            upstream_api_key_template=os.environ.get("RUNPOD_UPSTREAM_API_KEY_TEMPLATE", "").strip(),
            catalogue_file=os.environ.get("RUNPOD_MODELS_FILE", "").strip(),
            catalogue_inline=os.environ.get("RUNPOD_MODELS_JSON", "").strip(),
            catalogue=load_catalogue(
                path=os.environ.get("RUNPOD_MODELS_FILE", "").strip(),
                inline=os.environ.get("RUNPOD_MODELS_JSON", "").strip(),
            ),
        )

    def reload_catalogue(self) -> ModelCatalogue:
        """Re-read the configured catalogue source and validate it.

        Returns the new catalogue; it is NOT applied — the caller swaps it in
        (``object.__setattr__`` on this shared Config instance). Raises
        ModelConfigError when there is no source, it is unreadable/invalid, it
        would drop all models, or it would drop the configured default model.
        """
        if not (self.catalogue_file or self.catalogue_inline):
            raise ModelConfigError(
                "no model catalogue source configured "
                "(set RUNPOD_MODELS_FILE or RUNPOD_MODELS_JSON)"
            )
        return validate_reloaded(
            path=self.catalogue_file,
            inline=self.catalogue_inline,
            model_name=self.model_name,
        )

    @property
    def upstream_url(self) -> str:
        if self.mode == "pod":
            if self.pod_url:
                return self.pod_url
            if not self.pod_id:
                # Discovery mode: the target only exists after the first
                # successful discovery.  Return "" rather than a garbage
                # placeholder so status views and logs stay honest.
                return ""
            return f"https://{self.pod_id}-{self.pod_port}.proxy.runpod.net"
        return self.serverless_url

    @property
    def warmup_url(self) -> str:
        path = self.warmup_path.lstrip("/")
        return f"{self.upstream_url}/{path}" if path else self.upstream_url

    @property
    def discovery_enabled(self) -> bool:
        return self.mode == "pod" and not self.pod_id and bool(self.default_model)

    @property
    def default_model(self) -> str:
        if self.catalogue:
            if self.model_name:
                spec = self.catalogue.get(self.model_name)
                if spec is not None:
                    return spec.name
            return self.catalogue.names[0]
        return self.model_name or (self.allowed_models[0] if self.allowed_models else "")

    @property
    def effective_allowed_models(self) -> tuple[str, ...]:
        if self.catalogue:
            return self.catalogue.names
        return self.allowed_models or ((self.model_name,) if self.model_name else ())

    @property
    def upstream_auth_headers(self) -> dict:
        key = self.upstream_api_key or (self.api_key if self.mode == "serverless" else "")
        return {"authorization": f"Bearer {key}"} if key else {}

    def auth_headers_for_pod(self, pod_id: str) -> dict:
        """Auth header to use for a specific pod's model server.

        Some RunPod templates (e.g. the official vLLM one) set the model
        server's own API key to a value derived from the pod's id (e.g.
        ``VLLM_API_KEY=sk-$RUNPOD_POD_ID``), which changes every time
        discovery creates or replaces a pod. RUNPOD_UPSTREAM_API_KEY_TEMPLATE
        (e.g. "sk-{pod_id}") lets the proxy derive the correct key per pod
        instead of a static RUNPOD_UPSTREAM_API_KEY going stale on every new
        pod id.
        """
        if self.upstream_api_key_template and pod_id:
            key = self.upstream_api_key_template.format(pod_id=pod_id)
            return {"authorization": f"Bearer {key}"}
        return self.upstream_auth_headers
