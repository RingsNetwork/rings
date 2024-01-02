
import platform
import re
import time
import cffi
from eth_account.messages import encode_defunct
from web3 import Web3
import json
import asyncio

current_os = platform.system()
if current_os == "Windows":
    extension = "dll"
elif current_os == "Darwin":
    extension = "dylib"
else:
    extension = "so"


ffi = cffi.FFI()
c_header = open("./examples/ffi/rings.h", "r").read()
c_header = re.sub(r"#define .*", "", c_header)
ffi.cdef(c_header)
rings = ffi.dlopen(f"./target/debug/librings_node.{extension}")

def gen_signer(acc):
    @ffi.callback("void (*)(const char *, char *)")
    def signer(msg, output):
        c_input = ffi.string(msg)
        decoded = encode_defunct(c_input)
        sig = acc.sign_message(decoded)
        ffi.memmove(output, sig.signature, len(sig.signature))
        return
    return signer

@ffi.callback("void(*)(FFIBackendBehaviourWithRuntime *, ProviderPtr *, char *, char *)")
def default_handler(ins, provider, relay, message):
    return


@ffi.callback("void(*)(FFIBackendBehaviourWithRuntime *, ProviderPtr *, char *, char *)")
def on_custom_message(ins, provider, relay, message):
    print(message)
    return

def request(provider, method, data):
    c_data = ffi.new("char[]", data.encode())
    c_method = ffi.new("char[]", method.encode())
    ret = rings.request(ffi.addressof(provider), c_method, c_data)
    ret = ffi.string(ret)
    return ret


def create_provider(acc,
                    on_paintext_message=default_handler,
                    on_service_message=default_handler,
                    on_extension_message=default_handler):

    rings.init_logging(rings.Debug)
    callback = rings.new_ffi_backend_behaviour(on_paintext_message, on_service_message, on_extension_message)
    provider = rings.new_provider_with_callback(
        "stun://stun.l.google.com".encode(),
        10,
        acc.address.encode(),
        "eip191".encode(),
        gen_signer(acc),
        ffi.addressof(callback),
    )
    rings.listen(ffi.addressof(provider))
    return provider


def main():
    w3 = Web3()
    acc = w3.eth.account.create()
    provider = create_provider(acc, on_custom_message)
    ret = request(provider, "nodeInfo", json.dumps([]))
    print("node info:", ret)
    ret = request(provider, "createOffer", json.dumps(["0x11E807fcc88dD319270493fB2e822e388Fe36ab0"]))
    print("offer", ret)


if __name__ == "__main__":
    main()
