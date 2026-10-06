"""Per-request model selection and discovery pod switching."""
import asyncio
import contextlib

from .config import Config
from .lifecycle import DiscoveryPodLifecycle
from .runpod_api import model_slug
from .state import EndpointState, State
from .target import UpstreamTarget
from .warmup import WarmupManager


class ModelRouter:
    """Serializes model changes and holds a request lease while it streams."""

    def __init__(self, config: Config, state: EndpointState, target: UpstreamTarget,
                 lifecycle, warmup: WarmupManager) -> None:
        self.config = config
        self.state = state
        self.target = target
        self.lifecycle = lifecycle
        self.warmup = warmup
        self.active_model = config.default_model
        self._lock = asyncio.Lock()
        # ``_in_flight`` is mutated only from coroutine bodies (no await between
        # read and write), so a plain int is safe without ``self._lock``.  The
        # lock guards model switches alone; the release path stays lock-free so
        # a draining switch can observe an in-flight request finishing.
        self._in_flight = 0
        # Set whenever ``_in_flight`` is 0.  Constructed lazily because
        # ModelRouter is built outside a running event loop in main.py.
        self._idle: asyncio.Event | None = None

    def _idle_event(self) -> asyncio.Event:
        if self._idle is None:
            self._idle = asyncio.Event()
            self._idle.set()
        return self._idle

    def _acquire_lease(self) -> None:
        self._in_flight += 1
        if self._in_flight == 1:
            self._idle_event().clear()

    def _release_lease(self) -> None:
        self._in_flight -= 1
        if self._in_flight == 0:
            self._idle_event().set()

    def resolve(self, requested: str | None) -> tuple[str | None, str | None]:
        model = requested or self.config.default_model
        if not self.config.allowlist_configured:
            return self.config.default_model, None
        wanted = model.casefold()
        wanted_slug = model_slug(model)
        for allowed in self.config.effective_allowed_models:
            if allowed.casefold() == wanted or model_slug(allowed) == wanted_slug:
                return allowed, None
        return None, model

    @contextlib.asynccontextmanager
    async def request(self, model: str):
        async with self._lock:
            if isinstance(self.lifecycle, DiscoveryPodLifecycle) and model != self.active_model:
                await self._switch(model)
            # Acquire the lease under the lock so a request never starts against
            # a pod that is about to be swapped.  The counter itself is mutated
            # lock-free everywhere; the lock here only orders it after a switch.
            self._acquire_lease()
        try:
            yield
        finally:
            # Lock-free release: a concurrent switch draining on ``self._idle``
            # must be able to observe this completion promptly.
            self._release_lease()

    async def _switch(self, model: str) -> None:
        # Best-effort drain: wait (lock-free) for in-flight requests to finish,
        # but never block a switch beyond the drain budget.
        if self._in_flight:
            with contextlib.suppress(asyncio.TimeoutError):
                await asyncio.wait_for(
                    self._idle_event().wait(), timeout=self.config.model_switch_drain_s
                )
        try:
            await self.lifecycle.stop()
        except Exception:
            # Discovery lifecycle records failed stops for keepalive retries.
            pass
        self.target.set(self.config.upstream_url, "")
        self.state.state = State.COLD
        self.lifecycle.active_model = model
        self.active_model = model
        self.state.model_switches += 1
        await self.warmup.ensure_warm()
