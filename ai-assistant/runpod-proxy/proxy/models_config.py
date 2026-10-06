"""Declarative model catalogue configuration.

Parses and validates an explicit mapping of model name -> RunPod templates and
GPU preferences. Catalogues are authored as JSON or YAML — the file extension
selects the parser, and YAML is read with ``yaml.safe_load`` (never the
unrestricted loader) so a hand-edited file can never execute code at boot.
"""
import json
import os
from dataclasses import dataclass

import yaml

from proxy.runpod_api import model_slug


class ModelConfigError(ValueError):
    """A model catalogue config is missing, unreadable, or invalid."""


_MODEL_KEYS = frozenset(
    {"name", "templates", "gpus", "port", "container_disk_gb", "volume_gb",
     "cloud_type", "datacenters", "datacenter_priority"}
)
_GPU_KEYS = frozenset({"id", "min", "max"})


@dataclass(frozen=True)
class GpuSpec:
    id: str
    min_count: int
    max_count: int

    def counts(self) -> tuple[int, ...]:
        return tuple(range(self.min_count, self.max_count + 1))


_DATACENTER_PRIORITIES = frozenset({"availability", "custom"})


@dataclass(frozen=True)
class ModelSpec:
    name: str
    templates: tuple[str, ...]
    gpus: tuple[GpuSpec, ...]
    port: int | None
    container_disk_gb: int | None
    volume_gb: int | None
    cloud_type: str | None
    # Ordered RunPod datacenter ids (e.g. "US-TX-3") the pod may be created
    # in; sent as dataCenterIds on v2 create. datacenter_priority is kept
    # for compatibility but never sent — the v2 create API has no priority
    # field, so the declared order is informational only.
    datacenters: tuple[str, ...] = ()
    datacenter_priority: str = "availability"


@dataclass(frozen=True)
class ModelCatalogue:
    models: tuple[ModelSpec, ...] = ()

    @property
    def names(self) -> tuple[str, ...]:
        return tuple(spec.name for spec in self.models)

    def get(self, name: str) -> ModelSpec | None:
        wanted = model_slug(name)
        if not wanted:
            return None
        for spec in self.models:
            if model_slug(spec.name) == wanted:
                return spec
        return None

    def __bool__(self) -> bool:
        return bool(self.models)

    def __len__(self) -> int:
        return len(self.models)


def _is_int(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def _positive_int(value: object, label: str, where: str) -> int:
    if not _is_int(value) or value < 1:
        raise ModelConfigError(f"{where}: {label} ({value!r}) must be a positive integer")
    return value


def _parse_gpu(entry: object, model_index: int, name: str, gpu_index: int) -> GpuSpec:
    where = f"models[{model_index}] ({name!r})"
    if not isinstance(entry, dict):
        raise ModelConfigError(f"{where}: 'gpus[{gpu_index}]' must be an object")
    extra = set(entry) - _GPU_KEYS
    if extra:
        raise ModelConfigError(
            f"{where}: 'gpus[{gpu_index}]' has unknown key(s) {sorted(extra)}"
        )
    gpu_id = entry.get("id")
    if not isinstance(gpu_id, str) or not gpu_id.strip():
        raise ModelConfigError(
            f"{where}: 'gpus[{gpu_index}].id' must be a non-empty string"
        )
    min_count = entry.get("min", 1)
    if not _is_int(min_count):
        raise ModelConfigError(
            f"{where}: 'gpus[{gpu_index}].min' ({min_count!r}) must be an integer"
        )
    if min_count < 1:
        raise ModelConfigError(
            f"{where}: 'gpus[{gpu_index}].min' ({min_count}) must be >= 1"
        )
    max_count = entry.get("max", min_count)
    if not _is_int(max_count):
        raise ModelConfigError(
            f"{where}: 'gpus[{gpu_index}].max' ({max_count!r}) must be an integer"
        )
    if max_count < min_count:
        raise ModelConfigError(
            f"{where}: 'gpus[{gpu_index}].max' ({max_count}) must be >= 'min' ({min_count})"
        )
    return GpuSpec(id=gpu_id, min_count=min_count, max_count=max_count)


def _parse_model(entry: object, index: int) -> ModelSpec:
    if not isinstance(entry, dict):
        raise ModelConfigError(f"models[{index}] must be an object")
    name = entry.get("name")
    if not isinstance(name, str) or not name.strip():
        raise ModelConfigError(f"models[{index}]: 'name' must be a non-empty string")
    where = f"models[{index}] ({name!r})"
    extra = set(entry) - _MODEL_KEYS
    if extra:
        raise ModelConfigError(f"{where}: unknown key(s) {sorted(extra)}")

    templates = entry.get("templates")
    if not isinstance(templates, list) or not templates:
        raise ModelConfigError(f"{where}: 'templates' must be a non-empty list")
    template_names: list[str] = []
    for t_index, template in enumerate(templates):
        if not isinstance(template, str) or not template.strip():
            raise ModelConfigError(
                f"{where}: 'templates[{t_index}]' must be a non-empty string"
            )
        template_names.append(template)

    gpus_raw = entry.get("gpus", [])
    if not isinstance(gpus_raw, list):
        raise ModelConfigError(f"{where}: 'gpus' must be a list")
    gpus: list[GpuSpec] = []
    seen_gpu_ids: set[str] = set()
    for g_index, gpu_entry in enumerate(gpus_raw):
        gpu = _parse_gpu(gpu_entry, index, name, g_index)
        slug = gpu.id.strip().casefold()
        if slug in seen_gpu_ids:
            raise ModelConfigError(
                f"{where}: duplicate gpu id {gpu.id!r} in 'gpus[{g_index}]'"
            )
        seen_gpu_ids.add(slug)
        gpus.append(gpu)

    port = None
    if "port" in entry:
        port = _positive_int(entry.get("port"), "'port'", where)
    container_disk_gb = None
    if "container_disk_gb" in entry:
        container_disk_gb = _positive_int(
            entry.get("container_disk_gb"), "'container_disk_gb'", where
        )
    volume_gb = None
    if "volume_gb" in entry:
        volume_gb = _positive_int(entry.get("volume_gb"), "'volume_gb'", where)

    cloud_type = None
    if "cloud_type" in entry:
        cloud_type = entry.get("cloud_type")
        if not isinstance(cloud_type, str) or not cloud_type.strip():
            raise ModelConfigError(
                f"{where}: 'cloud_type' must be a non-empty string"
            )

    datacenters_raw = entry.get("datacenters", [])
    if not isinstance(datacenters_raw, list):
        raise ModelConfigError(f"{where}: 'datacenters' must be a list")
    datacenters: list[str] = []
    seen_datacenters: set[str] = set()
    for d_index, datacenter in enumerate(datacenters_raw):
        if not isinstance(datacenter, str) or not datacenter.strip():
            raise ModelConfigError(
                f"{where}: 'datacenters[{d_index}]' must be a non-empty string"
            )
        if datacenter in seen_datacenters:
            raise ModelConfigError(
                f"{where}: duplicate datacenter {datacenter!r} in 'datacenters'"
            )
        seen_datacenters.add(datacenter)
        datacenters.append(datacenter)

    datacenter_priority = entry.get("datacenter_priority", "availability")
    if datacenter_priority not in _DATACENTER_PRIORITIES:
        raise ModelConfigError(
            f"{where}: 'datacenter_priority' ({datacenter_priority!r}) must be "
            f"one of {sorted(_DATACENTER_PRIORITIES)}"
        )

    return ModelSpec(
        name=name,
        templates=tuple(template_names),
        gpus=tuple(gpus),
        port=port,
        container_disk_gb=container_disk_gb,
        volume_gb=volume_gb,
        cloud_type=cloud_type,
        datacenters=tuple(datacenters),
        datacenter_priority=datacenter_priority,
    )


def _build_catalogue(data: object) -> ModelCatalogue:
    if isinstance(data, dict):
        if "models" not in data or not isinstance(data.get("models"), list):
            raise ModelConfigError(
                "top-level object must contain a 'models' list"
            )
        raw_models = data["models"]
    elif isinstance(data, list):
        raw_models = data
    else:
        raise ModelConfigError(
            "catalogue must be an object with a 'models' list or a bare array"
        )

    specs: list[ModelSpec] = []
    seen_slugs: dict[str, str] = {}
    for index, entry in enumerate(raw_models):
        spec = _parse_model(entry, index)
        slug = model_slug(spec.name)
        if slug in seen_slugs:
            raise ModelConfigError(
                f"models[{index}] ({spec.name!r}): duplicate model name; "
                f"collides with {seen_slugs[slug]!r} (compared by slug {slug!r})"
            )
        seen_slugs[slug] = spec.name
        specs.append(spec)
    return ModelCatalogue(models=tuple(specs))


def _parse_document(raw: str, source: str, ext: str) -> object:
    """Parse catalogue text in the format dictated by the file extension.

    ``.json`` files parse strictly as JSON; ``.yaml``/``.yml`` strictly as
    YAML (via safe_load, so the file can never execute code); any other
    extension tries JSON first and then YAML, since YAML is a superset of
    JSON and operators sometimes omit the extension.
    """
    ext = ext.casefold()
    try:
        if ext in {".yaml", ".yml"}:
            return yaml.safe_load(raw)
        if ext == ".json":
            return json.loads(raw)
        try:
            return json.loads(raw)
        except ValueError:
            return yaml.safe_load(raw)
    except ValueError as exc:
        raise ModelConfigError(
            f"model catalogue {source} is not valid JSON: {exc}"
        ) from exc
    except yaml.YAMLError as exc:
        what = "YAML" if ext in {".yaml", ".yml"} else "JSON or YAML"
        raise ModelConfigError(
            f"model catalogue {source} is not valid {what}: {exc}"
        ) from exc


def validate_reloaded(*, path: str, inline: str, model_name: str) -> ModelCatalogue:
    """Re-read and validate a catalogue for a hot reload.

    Unlike boot (where an empty catalogue simply means "no catalogue"), a
    reload to an empty model list is rejected: it would drop every model from
    the allowlist of a running proxy. A configured default model must survive
    the reload, mirroring the boot-time check in ``Config.__post_init__``.
    """
    catalogue = load_catalogue(path=path, inline=inline)
    if not catalogue:
        raise ModelConfigError(
            "reloaded catalogue has no models; keeping the current catalogue"
        )
    if model_name and catalogue.get(model_name) is None:
        raise ModelConfigError(
            f"RUNPOD_MODEL_NAME ({model_name!r}) is not present in the reloaded "
            f"model catalogue; known models: {list(catalogue.names)}"
        )
    return catalogue


def load_catalogue(*, path: str = "", inline: str = "") -> ModelCatalogue:
    if path:
        try:
            with open(path, "r", encoding="utf-8") as handle:
                raw = handle.read()
        except OSError as exc:
            raise ModelConfigError(
                f"could not read model catalogue file {path!r}: {exc}"
            ) from exc
        source = f"file {path!r}"
        data = _parse_document(raw, source, os.path.splitext(path)[1])
    elif inline:
        data = _parse_document(inline, "inline JSON", ".json")
    else:
        return ModelCatalogue()
    return _build_catalogue(data)
