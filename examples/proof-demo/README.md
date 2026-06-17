# Rings proof-demo (Yew)

A Rust/[Yew](https://yew.rs) rewrite of the (deprecated, TypeScript) `rings-proof-demo`.
It demonstrates **distributed SNARK** over rings: this browser node is the *verifier* —
it builds a recursive proof task from a circuit, offloads the heavy proving to a *prover*
peer over the overlay, and verifies the returned proof. It drives the same
`SnarkProtocol` the daemon uses (`gen_and_send_proof_task` → `Effect::Compute` on the
prover → reply → `get_task_result`); no JS glue.

The rings wiring lives in the `rings`-prefixed helpers in `src/main.rs`; the rest is a
thin Yew UI.

## Run

```sh
cargo install trunk          # one-time
trunk serve                  # in this directory → http://localhost:8080
```

You also need:
- a **prover peer** reachable on the overlay (e.g. a `rings` daemon, or another browser
  tab) — paste its DID into the form;
- a **seed** node's HTTP endpoint to join the overlay (default `http://127.0.0.1:50000`);
- the circuit files served over HTTP (`simple_bn256.r1cs` / `.wasm` from
  `examples/snark/circoms`), e.g. `python3 -m http.server 8080` in that directory.

This crate is standalone (excluded from the cargo workspace) and only builds for wasm.
