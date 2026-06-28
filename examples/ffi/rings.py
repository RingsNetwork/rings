from __future__ import annotations

import base64
from dataclasses import dataclass
import json
import os
import platform
from pathlib import Path
import re
import time
from typing import Any

SIGNATURE_LEN = 65
REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_HEADER_PATH = REPO_ROOT / "crates" / "node" / "include" / "rings.h"
DEFAULT_TARGET_ROOT = REPO_ROOT / "target"
DEFAULT_PROFILE = "debug"


@dataclass(frozen=True)
class FfiRuntime:
    ffi: Any
    rings: Any


@dataclass(frozen=True)
class ProviderHandle:
    provider: Any
    signer: Any


def library_extension(system: str | None = None) -> str:
    system = system or platform.system()
    if system == "Windows":
        return "dll"
    if system == "Darwin":
        return "dylib"
    return "so"


def host_target_triple(system: str | None = None, machine: str | None = None) -> str | None:
    system = system or platform.system()
    machine = (machine or platform.machine()).lower()
    if system == "Darwin":
        if machine in {"arm64", "aarch64"}:
            return "aarch64-apple-darwin"
        if machine in {"x86_64", "amd64"}:
            return "x86_64-apple-darwin"
    if system == "Linux":
        if machine in {"x86_64", "amd64"}:
            return "x86_64-unknown-linux-gnu"
        if machine in {"arm64", "aarch64"}:
            return "aarch64-unknown-linux-gnu"
    if system == "Windows":
        if machine in {"x86_64", "amd64"}:
            return "x86_64-pc-windows-msvc"
        if machine in {"arm64", "aarch64"}:
            return "aarch64-pc-windows-msvc"
    return None


def default_library_path(
    target_dir: str | Path | None = None,
    system: str | None = None,
) -> Path:
    library_name = f"librings_node.{library_extension(system)}"
    if target_dir is not None:
        return Path(target_dir) / library_name

    target_root = Path(os.environ.get("CARGO_TARGET_DIR") or DEFAULT_TARGET_ROOT)
    candidates = [target_root / DEFAULT_PROFILE]
    target_triple = os.environ.get("CARGO_BUILD_TARGET") or host_target_triple(system)
    if target_triple is not None:
        candidates.append(target_root / target_triple / DEFAULT_PROFILE)

    for candidate in candidates:
        library_path = candidate / library_name
        if library_path.exists():
            return library_path
    return candidates[0] / library_name


DEFAULT_TARGET_DIR = DEFAULT_TARGET_ROOT / DEFAULT_PROFILE


def read_header(header_path: str | Path | None = None) -> str:
    path = Path(header_path) if header_path is not None else DEFAULT_HEADER_PATH
    return path.read_text(encoding="utf-8")


def cffi_header(header: str) -> str:
    return re.sub(r"^#.*$", "", header, flags=re.MULTILINE)


def build_ffi(header_path: str | Path | None = None):
    import cffi

    ffi = cffi.FFI()
    ffi.cdef(cffi_header(read_header(header_path)))
    return ffi


def load_runtime(
    header_path: str | Path | None = None,
    library_path: str | Path | None = None,
) -> FfiRuntime:
    ffi = build_ffi(header_path)
    lib = Path(library_path) if library_path is not None else default_library_path()
    return FfiRuntime(ffi=ffi, rings=ffi.dlopen(str(lib)))


def gen_signer(ffi, acc):
    from eth_account.messages import encode_defunct

    @ffi.callback("void (*)(const char *, char *)")
    def signer(msg, output):
        c_input = ffi.string(msg)
        decoded = encode_defunct(c_input)
        sig = acc.sign_message(decoded)
        ffi.memmove(output, sig.signature, SIGNATURE_LEN)
        return

    return signer


def _provider_value(provider: ProviderHandle | Any):
    if isinstance(provider, ProviderHandle):
        return provider.provider
    return provider


def request(runtime: FfiRuntime, provider: ProviderHandle | Any, method: str, data: Any) -> bytes:
    if not isinstance(data, str):
        data = json.dumps(data)
    provider_value = _provider_value(provider)
    c_data = runtime.ffi.new("char[]", data.encode())
    c_method = runtime.ffi.new("char[]", method.encode())
    ret = runtime.rings.request(runtime.ffi.addressof(provider_value), c_method, c_data)
    return runtime.ffi.string(ret)


def request_json(runtime: FfiRuntime, provider: ProviderHandle | Any, method: str, data: Any) -> Any:
    return json.loads(request(runtime, provider, method, data).decode())


def create_provider(
    runtime: FfiRuntime,
    acc,
    network_id: int = 0,
    ice_server: str = "stun://stun.l.google.com",
    stabilize_interval: int = 10,
) -> ProviderHandle:
    # Inbound messages are routed to namespaced protocols by the extension registry;
    # the old per-variant C message callbacks have been removed.
    runtime.rings.init_logging(runtime.rings.Debug)
    signer = gen_signer(runtime.ffi, acc)
    provider = runtime.rings.new_provider_with_callback(
        network_id,
        ice_server.encode(),
        stabilize_interval,
        acc.address.encode(),
        "eip191".encode(),
        signer,
    )
    runtime.rings.listen(runtime.ffi.addressof(provider))
    return ProviderHandle(provider=provider, signer=signer)


def node_did(runtime: FfiRuntime, provider: ProviderHandle | Any) -> str:
    return request_json(runtime, provider, "nodeDid", {})["did"]


def create_offer(runtime: FfiRuntime, provider: ProviderHandle | Any, remote_did: str) -> str:
    return request_json(runtime, provider, "createOffer", {"did": remote_did})["offer"]


def answer_offer(runtime: FfiRuntime, provider: ProviderHandle | Any, offer: str) -> str:
    return request_json(runtime, provider, "answerOffer", {"offer": offer})["answer"]


def accept_answer(runtime: FfiRuntime, provider: ProviderHandle | Any, answer: str) -> Any:
    return request_json(runtime, provider, "acceptAnswer", {"answer": answer})


def list_peers(runtime: FfiRuntime, provider: ProviderHandle | Any) -> list[dict[str, Any]]:
    return request_json(runtime, provider, "listPeers", {})["peers"]


def peer_is_connected(runtime: FfiRuntime, provider: ProviderHandle | Any, remote_did: str) -> bool:
    return any(
        peer.get("did") == remote_did and peer.get("state") == "Connected"
        for peer in list_peers(runtime, provider)
    )


def wait_for_connected_peer(
    runtime: FfiRuntime,
    provider: ProviderHandle | Any,
    remote_did: str,
    timeout_seconds: float = 15.0,
    poll_seconds: float = 0.25,
) -> None:
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        if peer_is_connected(runtime, provider, remote_did):
            return
        time.sleep(poll_seconds)
    raise TimeoutError(f"peer {remote_did} did not reach Connected")


def connect_providers(
    runtime: FfiRuntime,
    initiator: ProviderHandle | Any,
    responder: ProviderHandle | Any,
    timeout_seconds: float = 15.0,
) -> None:
    initiator_did = node_did(runtime, initiator)
    responder_did = node_did(runtime, responder)
    offer = create_offer(runtime, initiator, responder_did)
    answer = answer_offer(runtime, responder, offer)
    accept_answer(runtime, initiator, answer)
    wait_for_connected_peer(runtime, initiator, responder_did, timeout_seconds)
    wait_for_connected_peer(runtime, responder, initiator_did, timeout_seconds)


def take_e2e_events(runtime: FfiRuntime, provider: ProviderHandle | Any) -> list[dict[str, Any]]:
    return request_json(runtime, provider, "takeE2eEvents", {})["events"]


def send_e2e_handshake(
    runtime: FfiRuntime,
    provider: ProviderHandle | Any,
    destination_did: str,
) -> str:
    return request_json(
        runtime,
        provider,
        "sendE2eHandshake",
        {"destination_did": destination_did},
    )["tx_id"]


def send_e2e_message(
    runtime: FfiRuntime,
    provider: ProviderHandle | Any,
    destination_did: str,
    recipient_public_key: str,
    plaintext: bytes,
    max_plaintext_frame_len: int = 0,
) -> str:
    return request_json(
        runtime,
        provider,
        "sendE2eMessage",
        {
            "destination_did": destination_did,
            "recipient_public_key": recipient_public_key,
            "data": base64.b64encode(plaintext).decode(),
            "max_plaintext_frame_len": max_plaintext_frame_len,
        },
    )["stream_id"]


def wait_for_e2e_event(
    runtime: FfiRuntime,
    provider: ProviderHandle | Any,
    predicate,
    timeout_seconds: float = 15.0,
    poll_seconds: float = 0.25,
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        for event in take_e2e_events(runtime, provider):
            if predicate(event):
                return event
        time.sleep(poll_seconds)
    raise TimeoutError("matching E2E event was not observed")


def wait_for_e2e_stream(
    runtime: FfiRuntime,
    provider: ProviderHandle | Any,
    stream_id: str,
    timeout_seconds: float = 15.0,
    poll_seconds: float = 0.25,
) -> list[dict[str, Any]]:
    frames: list[dict[str, Any]] = []
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        frames.extend(
            event
            for event in take_e2e_events(runtime, provider)
            if event.get("kind") == "streamFrame" and event.get("stream_id") == stream_id
        )
        if any(frame.get("is_final") for frame in frames):
            return frames
        time.sleep(poll_seconds)
    raise TimeoutError(f"E2E stream {stream_id} did not reach final frame")


def main():
    from web3 import Web3

    runtime = load_runtime()
    w3 = Web3()
    acc = w3.eth.account.create()
    provider = create_provider(runtime, acc)
    ret = request(runtime, provider, "nodeInfo", {})
    print("node info:", ret)
    ret = request(
        runtime,
        provider,
        "createOffer",
        {"did": "0x11E807fcc88dD319270493fB2e822e388Fe36ab0"},
    )
    print("offer", ret)


if __name__ == "__main__":
    main()
