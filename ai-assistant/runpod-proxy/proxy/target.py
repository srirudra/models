"""Mutable upstream selected at runtime."""


class UpstreamTarget:
    def __init__(self, url: str, pod_id: str = "") -> None:
        self._url = url.rstrip("/")
        self._pod_id = pod_id

    @property
    def url(self) -> str:
        return self._url

    @property
    def pod_id(self) -> str:
        return self._pod_id

    def set(self, url: str, pod_id: str = "") -> None:
        self._url = url.rstrip("/")
        self._pod_id = pod_id

    def warmup_url(self, warmup_path: str) -> str:
        path = warmup_path.lstrip("/")
        return f"{self._url}/{path}" if path else self._url
