import importlib.util
import os
from pathlib import Path
import sys

import pytest
from web3 import Web3

REPO_ROOT = Path(__file__).resolve().parents[3]
RINGS_PY = REPO_ROOT / "examples" / "ffi" / "rings.py"


def load_example_module():
    spec = importlib.util.spec_from_file_location("rings_ffi_example", RINGS_PY)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def require_built_library(module):
    library_path = module.default_library_path()
    if library_path.exists():
        return library_path
    if os.environ.get("RINGS_FFI_REQUIRE_LIBRARY") == "1":
        pytest.fail(f"built FFI library not found: {library_path}")
    pytest.skip(f"built FFI library not found: {library_path}")


def test_import_has_no_header_or_dlopen_side_effects():
    module = load_example_module()

    assert module.DEFAULT_HEADER_PATH == REPO_ROOT / "crates" / "node" / "include" / "rings.h"
    assert module.default_library_path("target/debug", "Linux").as_posix().endswith(
        "target/debug/librings_node.so"
    )


def test_default_library_path_honors_cargo_target_dir(monkeypatch):
    module = load_example_module()

    monkeypatch.setenv("CARGO_TARGET_DIR", "/tmp/rings-target")

    assert module.default_library_path(system="Linux") == Path(
        "/tmp/rings-target/debug/librings_node.so"
    )


def test_default_library_path_prefers_existing_target_triple(monkeypatch, tmp_path):
    module = load_example_module()
    library_path = tmp_path / "x86_64-unknown-linux-gnu" / "debug" / "librings_node.so"
    library_path.parent.mkdir(parents=True)
    library_path.write_bytes(b"")

    monkeypatch.setenv("CARGO_TARGET_DIR", str(tmp_path))
    monkeypatch.setenv("CARGO_BUILD_TARGET", "x86_64-unknown-linux-gnu")

    assert module.default_library_path(system="Linux") == library_path


@pytest.mark.parametrize(
    ("system", "extension"),
    [("Linux", "so"), ("Darwin", "dylib"), ("Windows", "dll")],
)
def test_library_extension_matches_platform(system, extension):
    module = load_example_module()

    assert module.library_extension(system) == extension


def test_header_loads_from_crate_owned_path():
    module = load_example_module()

    header = module.read_header()
    assert "new_provider_with_callback" in header
    assert "const char *request" in header

    ffi = module.build_ffi()
    assert ffi.typeof("struct ProviderPtr *")


def test_signer_writes_a_65_byte_signature():
    module = load_example_module()
    ffi = module.build_ffi()
    account = Web3().eth.account.create()
    signer = module.gen_signer(ffi, account)
    output = ffi.new("char[]", module.SIGNATURE_LEN)

    signer(ffi.new("char[]", b"rings ffi test"), output)

    signature = bytes(ffi.buffer(output, module.SIGNATURE_LEN))
    assert len(signature) == module.SIGNATURE_LEN
    assert any(signature)


@pytest.fixture(scope="module")
def ffi_runtime():
    module = load_example_module()
    library_path = require_built_library(module)
    return module, module.load_runtime(library_path=library_path)


def test_provider_node_info_round_trip(ffi_runtime):
    module, runtime = ffi_runtime
    account = Web3().eth.account.create()
    provider = module.create_provider(runtime, account)

    result = module.request_json(runtime, provider, "nodeInfo", {})

    assert isinstance(result, dict)
    assert result


def test_create_offer_exercises_ffi_request_path(ffi_runtime):
    module, runtime = ffi_runtime
    account = Web3().eth.account.create()
    provider = module.create_provider(runtime, account)

    result = module.request_json(
        runtime,
        provider,
        "createOffer",
        {"did": "0x11E807fcc88dD319270493fB2e822e388Fe36ab0"},
    )

    assert isinstance(result, dict)
    assert isinstance(result.get("offer"), str)
    assert result["offer"]


def test_two_ffi_providers_connect_with_offer_answer(ffi_runtime):
    module, runtime = ffi_runtime
    provider_a = module.create_provider(runtime, Web3().eth.account.create())
    provider_b = module.create_provider(runtime, Web3().eth.account.create())
    did_a = module.node_did(runtime, provider_a)
    did_b = module.node_did(runtime, provider_b)

    module.connect_providers(runtime, provider_a, provider_b)

    assert module.peer_is_connected(runtime, provider_a, did_b)
    assert module.peer_is_connected(runtime, provider_b, did_a)


def test_two_ffi_providers_exchange_e2e_handshake_and_stream_frames(ffi_runtime):
    module, runtime = ffi_runtime
    provider_a = module.create_provider(runtime, Web3().eth.account.create())
    provider_b = module.create_provider(runtime, Web3().eth.account.create())
    did_a = module.node_did(runtime, provider_a)
    did_b = module.node_did(runtime, provider_b)

    module.connect_providers(runtime, provider_a, provider_b)
    module.send_e2e_handshake(runtime, provider_a, did_b)

    request = module.wait_for_e2e_event(
        runtime,
        provider_b,
        lambda event: event.get("kind") == "handshakeRequest"
        and event.get("from") == did_a,
    )
    response = module.wait_for_e2e_event(
        runtime,
        provider_a,
        lambda event: event.get("kind") == "handshakeResponse"
        and event.get("from") == did_b,
    )

    assert request["public_key"]
    assert response["public_key"]

    stream_id = module.send_e2e_message(
        runtime,
        provider_a,
        did_b,
        response["public_key"],
        b"ffi e2e encrypted stream body",
        max_plaintext_frame_len=8,
    )
    frames = module.wait_for_e2e_stream(runtime, provider_b, stream_id)
    sequences = sorted(frame["sequence"] for frame in frames)

    assert len(frames) > 1
    assert sequences == list(range(len(frames)))
    assert sum(1 for frame in frames if frame["is_final"]) == 1
    assert all(frame["from"] == did_a for frame in frames)
    assert all(frame["ciphertext_blocks"] > 0 for frame in frames)
