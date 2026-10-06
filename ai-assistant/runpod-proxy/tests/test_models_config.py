"""Tests for the declarative model catalogue config layer."""
import json

import pytest

from proxy.config import Config
from proxy.models_config import (
    GpuSpec,
    ModelCatalogue,
    ModelConfigError,
    ModelSpec,
    load_catalogue,
)

CATALOGUE = {
    "models": [
        {
            "name": "Qwen/Qwen3.8-27B-FP8",
            "templates": ["qwen3-vllm-fp8", "qwen3-vllm-a100"],
            "gpus": [
                {"id": "NVIDIA H100 80GB HBM3", "min": 1, "max": 2},
                {"id": "NVIDIA A100 80GB PCIe", "min": 2, "max": 4},
            ],
            "port": 8000,
            "container_disk_gb": 60,
            "volume_gb": 100,
            "cloud_type": "SECURE",
        },
        {
            "name": "meta/Llama-3-70B",
            "templates": ["llama3-vllm"],
        },
    ]
}


def _write(tmp_path, data) -> str:
    path = tmp_path / "models.json"
    path.write_text(json.dumps(data), encoding="utf-8")
    return str(path)


# ---------------------------------------------------------------------------
# Happy paths
# ---------------------------------------------------------------------------


def test_happy_path_from_file(tmp_path):
    cat = load_catalogue(path=_write(tmp_path, CATALOGUE))
    assert cat
    assert len(cat) == 2
    assert cat.names == ("Qwen/Qwen3.8-27B-FP8", "meta/Llama-3-70B")

    qwen = cat.get("Qwen/Qwen3.8-27B-FP8")
    assert qwen.templates == ("qwen3-vllm-fp8", "qwen3-vllm-a100")
    assert tuple(g.id for g in qwen.gpus) == (
        "NVIDIA H100 80GB HBM3",
        "NVIDIA A100 80GB PCIe",
    )
    assert qwen.gpus[0].counts() == (1, 2)
    assert qwen.gpus[1].counts() == (2, 3, 4)
    assert qwen.port == 8000
    assert qwen.container_disk_gb == 60
    assert qwen.volume_gb == 100
    assert qwen.cloud_type == "SECURE"


def test_max_defaults_to_min(tmp_path):
    data = {
        "models": [
            {"name": "m", "templates": ["t"], "gpus": [{"id": "gpu", "min": 3}]}
        ]
    }
    cat = load_catalogue(path=_write(tmp_path, data))
    gpu = cat.get("m").gpus[0]
    assert gpu.min_count == 3
    assert gpu.max_count == 3
    assert gpu.counts() == (3,)


def test_min_defaults_to_one(tmp_path):
    data = {"models": [{"name": "m", "templates": ["t"], "gpus": [{"id": "gpu"}]}]}
    gpu = load_catalogue(path=_write(tmp_path, data)).get("m").gpus[0]
    assert gpu.counts() == (1,)


def test_happy_path_inline():
    cat = load_catalogue(inline=json.dumps(CATALOGUE))
    assert cat.names == ("Qwen/Qwen3.8-27B-FP8", "meta/Llama-3-70B")


def test_bare_array_top_level():
    cat = load_catalogue(inline=json.dumps(CATALOGUE["models"]))
    assert cat.names == ("Qwen/Qwen3.8-27B-FP8", "meta/Llama-3-70B")


def test_path_wins_over_inline(tmp_path):
    path = _write(tmp_path, {"models": [{"name": "fromfile", "templates": ["t"]}]})
    cat = load_catalogue(path=path, inline=json.dumps(CATALOGUE))
    assert cat.names == ("fromfile",)


def test_empty_catalogue_when_neither():
    cat = load_catalogue()
    assert not cat
    assert len(cat) == 0
    assert cat.names == ()


# ---------------------------------------------------------------------------
# YAML support
# ---------------------------------------------------------------------------

YAML_CATALOGUE = """\
models:
  - name: Qwen/Qwen3.8-27B-FP8
    templates: [qwen3-vllm-fp8, qwen3-vllm-a100]
    gpus:
      - {id: "NVIDIA H100 80GB HBM3", min: 1, max: 2}
      - {id: "NVIDIA A100 80GB PCIe", min: 2, max: 4}
    port: 8000
    container_disk_gb: 60
    volume_gb: 100
    cloud_type: SECURE
  - name: meta/Llama-3-70B
    templates: [llama3-vllm]
"""


@pytest.mark.parametrize("ext", [".yaml", ".yml"])
def test_happy_path_from_yaml_file(tmp_path, ext):
    path = tmp_path / f"models{ext}"
    path.write_text(YAML_CATALOGUE, encoding="utf-8")
    cat = load_catalogue(path=str(path))
    assert cat
    assert len(cat) == 2
    assert cat.names == ("Qwen/Qwen3.8-27B-FP8", "meta/Llama-3-70B")

    qwen = cat.get("Qwen/Qwen3.8-27B-FP8")
    assert qwen.templates == ("qwen3-vllm-fp8", "qwen3-vllm-a100")
    assert tuple(g.id for g in qwen.gpus) == (
        "NVIDIA H100 80GB HBM3",
        "NVIDIA A100 80GB PCIe",
    )
    assert qwen.gpus[0].counts() == (1, 2)
    assert qwen.gpus[1].counts() == (2, 3, 4)
    assert qwen.port == 8000
    assert qwen.container_disk_gb == 60
    assert qwen.volume_gb == 100
    assert qwen.cloud_type == "SECURE"


def test_bad_yaml(tmp_path):
    path = tmp_path / "models.yaml"
    path.write_text("models:\n  - name: [unclosed\n", encoding="utf-8")
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(path=str(path))
    assert "YAML" in str(exc.value)


def test_empty_yaml_file_rejected(tmp_path):
    path = tmp_path / "models.yaml"
    path.write_text("", encoding="utf-8")
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(path=str(path))
    assert "models" in str(exc.value)


def test_unknown_extension_parses_yaml(tmp_path):
    path = tmp_path / "models"
    path.write_text("models:\n  - name: m\n    templates: [t]\n", encoding="utf-8")
    assert load_catalogue(path=str(path)).names == ("m",)


def test_unknown_extension_prefers_json(tmp_path):
    path = tmp_path / "models.txt"
    path.write_text(
        json.dumps({"models": [{"name": "m", "templates": ["t"]}]}),
        encoding="utf-8",
    )
    assert load_catalogue(path=str(path)).names == ("m",)


def test_unknown_extension_bad_content(tmp_path):
    path = tmp_path / "models"
    path.write_text("models: [ {name: m\n", encoding="utf-8")
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(path=str(path))
    assert "JSON or YAML" in str(exc.value)


def test_yaml_bool_name_rejected(tmp_path):
    # YAML 1.1 parses unquoted "on" as a boolean; the schema validation must
    # reject it with a clear name error rather than crash downstream.
    path = tmp_path / "models.yaml"
    path.write_text("models:\n  - name: on\n    templates: [t]\n", encoding="utf-8")
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(path=str(path))
    assert "'name'" in str(exc.value)


def test_config_catalogue_from_yaml_file(monkeypatch, tmp_path):
    _clear_env(monkeypatch)
    path = tmp_path / "models.yaml"
    path.write_text(YAML_CATALOGUE, encoding="utf-8")
    monkeypatch.setenv("RUNPOD_MODELS_FILE", str(path))
    cfg = Config.from_env()
    assert cfg.default_model == "Qwen/Qwen3.8-27B-FP8"


# ---------------------------------------------------------------------------
# get() resolution
# ---------------------------------------------------------------------------


def test_get_exact_case_and_slug():
    cat = load_catalogue(inline=json.dumps(CATALOGUE))
    assert cat.get("Qwen/Qwen3.8-27B-FP8").name == "Qwen/Qwen3.8-27B-FP8"
    assert cat.get("qwen/qwen3.8-27b-fp8").name == "Qwen/Qwen3.8-27B-FP8"
    assert cat.get("qwen-qwen3-8-27b-fp8").name == "Qwen/Qwen3.8-27B-FP8"
    assert cat.get("does-not-exist") is None
    assert cat.get("") is None


# ---------------------------------------------------------------------------
# Validation failures
# ---------------------------------------------------------------------------


def test_missing_file():
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(path="/no/such/file/here.json")
    assert "read" in str(exc.value).lower()


def test_bad_json():
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline="{not valid json")
    assert "JSON" in str(exc.value)


def test_top_level_wrong_type():
    with pytest.raises(ModelConfigError):
        load_catalogue(inline="42")


def test_top_level_object_without_models_list():
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps({"model": []}))
    assert "models" in str(exc.value)


def test_model_not_object():
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps({"models": ["nope"]}))
    assert "models[0]" in str(exc.value)


def test_name_missing():
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps({"models": [{"templates": ["t"]}]}))
    assert "'name'" in str(exc.value)


def test_name_blank():
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps({"models": [{"name": "  ", "templates": ["t"]}]}))
    assert "'name'" in str(exc.value)


def test_duplicate_model_by_slug():
    data = {
        "models": [
            {"name": "Qwen/Qwen3-32B", "templates": ["t"]},
            {"name": "qwen-qwen3-32b", "templates": ["t"]},
        ]
    }
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps(data))
    assert "duplicate" in str(exc.value).lower()


def test_templates_missing():
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps({"models": [{"name": "m"}]}))
    assert "'templates'" in str(exc.value)


def test_templates_not_list():
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps({"models": [{"name": "m", "templates": "t"}]}))
    assert "'templates'" in str(exc.value)


def test_templates_empty():
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps({"models": [{"name": "m", "templates": []}]}))
    assert "'templates'" in str(exc.value)


def test_templates_blank_entry():
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps({"models": [{"name": "m", "templates": [" "]}]}))
    assert "templates[0]" in str(exc.value)


def test_unknown_key_at_model_level():
    data = {"models": [{"name": "m", "template": ["t"], "templates": ["t"]}]}
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps(data))
    assert "unknown key" in str(exc.value)
    assert "template" in str(exc.value)


def test_unknown_key_at_gpu_level():
    data = {
        "models": [
            {"name": "m", "templates": ["t"], "gpus": [{"id": "g", "mn": 1}]}
        ]
    }
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps(data))
    assert "unknown key" in str(exc.value)


def test_gpus_not_list():
    data = {"models": [{"name": "m", "templates": ["t"], "gpus": {}}]}
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps(data))
    assert "'gpus'" in str(exc.value)


def test_gpu_not_object():
    data = {"models": [{"name": "m", "templates": ["t"], "gpus": ["g"]}]}
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps(data))
    assert "gpus[0]" in str(exc.value)


def test_gpu_id_missing():
    data = {"models": [{"name": "m", "templates": ["t"], "gpus": [{"min": 1}]}]}
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps(data))
    assert "gpus[0].id" in str(exc.value)


def test_min_less_than_one():
    data = {"models": [{"name": "m", "templates": ["t"], "gpus": [{"id": "g", "min": 0}]}]}
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps(data))
    assert "gpus[0].min" in str(exc.value)


def test_max_less_than_min():
    data = {
        "models": [
            {"name": "Qwen/Qwen3.8-27B-FP8", "templates": ["t"],
             "gpus": [{"id": "g", "min": 2, "max": 1}]}
        ]
    }
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps(data))
    msg = str(exc.value)
    assert "gpus[0].max" in msg
    assert "min" in msg


def test_bool_rejected_as_int():
    # JSON true -> Python bool; must be rejected as an integer for min.
    data = {"models": [{"name": "m", "templates": ["t"], "gpus": [{"id": "g", "min": True}]}]}
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps(data))
    assert "gpus[0].min" in str(exc.value)


def test_duplicate_gpu_id():
    data = {
        "models": [
            {"name": "m", "templates": ["t"],
             "gpus": [{"id": "g"}, {"id": "g"}]}
        ]
    }
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps(data))
    assert "duplicate gpu id" in str(exc.value)


def test_port_not_positive_int():
    data = {"models": [{"name": "m", "templates": ["t"], "port": 0}]}
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps(data))
    assert "'port'" in str(exc.value)


def test_container_disk_not_positive_int():
    data = {"models": [{"name": "m", "templates": ["t"], "container_disk_gb": -5}]}
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps(data))
    assert "'container_disk_gb'" in str(exc.value)


def test_volume_not_positive_int():
    data = {"models": [{"name": "m", "templates": ["t"], "volume_gb": "big"}]}
    with pytest.raises(ModelConfigError) as exc:
        load_catalogue(inline=json.dumps(data))
    assert "'volume_gb'" in str(exc.value)


# ---------------------------------------------------------------------------
# Config.from_env integration
# ---------------------------------------------------------------------------

_ENV_KEYS = [
    "RUNPOD_MODELS_FILE",
    "RUNPOD_MODELS_JSON",
    "RUNPOD_MODEL_NAME",
    "RUNPOD_ALLOWED_MODELS",
    "RUNPOD_SERVERLESS_URL",
]


def _clear_env(monkeypatch):
    for key in _ENV_KEYS:
        monkeypatch.delenv(key, raising=False)


def test_config_catalogue_populates_fields(monkeypatch):
    _clear_env(monkeypatch)
    monkeypatch.setenv("RUNPOD_MODELS_JSON", json.dumps(CATALOGUE))
    cfg = Config.from_env()
    assert cfg.catalogue
    assert cfg.allowlist_configured is True
    assert cfg.effective_allowed_models == (
        "Qwen/Qwen3.8-27B-FP8",
        "meta/Llama-3-70B",
    )
    assert cfg.default_model == "Qwen/Qwen3.8-27B-FP8"


def test_config_model_name_not_in_catalogue_raises(monkeypatch):
    _clear_env(monkeypatch)
    monkeypatch.setenv("RUNPOD_MODELS_JSON", json.dumps(CATALOGUE))
    monkeypatch.setenv("RUNPOD_MODEL_NAME", "not/in/catalogue")
    with pytest.raises(ModelConfigError):
        Config.from_env()


def test_config_model_name_canonical_spelling(monkeypatch):
    _clear_env(monkeypatch)
    monkeypatch.setenv("RUNPOD_MODELS_JSON", json.dumps(CATALOGUE))
    monkeypatch.setenv("RUNPOD_MODEL_NAME", "qwen-qwen3-8-27b-fp8")
    cfg = Config.from_env()
    assert cfg.default_model == "Qwen/Qwen3.8-27B-FP8"


def test_config_catalogue_from_file(monkeypatch, tmp_path):
    _clear_env(monkeypatch)
    monkeypatch.setenv("RUNPOD_MODELS_FILE", _write(tmp_path, CATALOGUE))
    cfg = Config.from_env()
    assert cfg.default_model == "Qwen/Qwen3.8-27B-FP8"


# ---------------------------------------------------------------------------
# Backward compatibility (no catalogue env vars)
# ---------------------------------------------------------------------------


def test_backward_compat_allowed_models(monkeypatch):
    _clear_env(monkeypatch)
    monkeypatch.setenv("RUNPOD_ALLOWED_MODELS", "a, b, c")
    cfg = Config.from_env()
    assert not cfg.catalogue
    assert cfg.effective_allowed_models == ("a", "b", "c")
    assert cfg.default_model == "a"
    assert cfg.allowlist_configured is True


def test_backward_compat_model_name(monkeypatch):
    _clear_env(monkeypatch)
    monkeypatch.setenv("RUNPOD_MODEL_NAME", "solo-model")
    cfg = Config.from_env()
    assert not cfg.catalogue
    assert cfg.effective_allowed_models == ("solo-model",)
    assert cfg.default_model == "solo-model"


def test_backward_compat_empty(monkeypatch):
    _clear_env(monkeypatch)
    cfg = Config.from_env()
    assert cfg.effective_allowed_models == ()
    assert cfg.default_model == ""
    assert cfg.allowlist_configured is False


# ---------------------------------------------------------------------------
# Datacentre pinning
# ---------------------------------------------------------------------------


def _one_model(**extra):
    model = {"name": "m", "templates": ["t"]}
    model.update(extra)
    return json.dumps({"models": [model]})


def test_datacenters_parsed():
    cat = load_catalogue(inline=_one_model(
        datacenters=["US-TX-3", "US-KS-3"], datacenter_priority="custom"))
    spec = cat.get("m")
    assert spec.datacenters == ("US-TX-3", "US-KS-3")
    assert spec.datacenter_priority == "custom"


def test_datacenters_default_to_any():
    cat = load_catalogue(inline=_one_model())
    spec = cat.get("m")
    assert spec.datacenters == ()
    assert spec.datacenter_priority == "availability"


@pytest.mark.parametrize(
    ("bad", "match"),
    [
        ("not-a-list", "'datacenters' must be a list"),
        (["US-TX-3", ""], "non-empty string"),
        (["US-TX-3", "US-TX-3"], "duplicate datacenter"),
    ],
)
def test_datacenters_reject_invalid(bad, match):
    with pytest.raises(ModelConfigError, match=match):
        load_catalogue(inline=_one_model(datacenters=bad))


def test_datacenter_priority_rejects_unknown():
    with pytest.raises(ModelConfigError, match="datacenter_priority"):
        load_catalogue(inline=_one_model(datacenter_priority="nearest"))


# ---------------------------------------------------------------------------
# Dataclass surface
# ---------------------------------------------------------------------------


def test_dataclasses_are_frozen():
    gpu = GpuSpec(id="g", min_count=1, max_count=2)
    with pytest.raises(Exception):
        gpu.id = "x"  # type: ignore[misc]
    spec = ModelSpec(
        name="m", templates=("t",), gpus=(gpu,), port=None,
        container_disk_gb=None, volume_gb=None, cloud_type=None,
    )
    with pytest.raises(Exception):
        spec.name = "x"  # type: ignore[misc]
    assert ModelCatalogue((spec,)).get("m") is spec
