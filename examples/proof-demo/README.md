# Rings proof-demo (Yew)

A Rust/[Yew](https://yew.rs) rewrite of the (deprecated, TypeScript) `rings-proof-demo`.

## Functionality

**Distributed SNARK** over rings: this browser node is the *verifier*. It builds a
recursive proof task from a circuit, offloads the heavy proving to a *prover* peer over
the overlay, and verifies the returned proof — driving the same `SnarkProtocol` the
daemon uses, with no JS glue:

1. builds an in-browser node (IndexedDB) and registers the SNARK protocol;
2. joins the overlay via a seed node's HTTP endpoint;
3. loads a circuit (`r1cs`/`wasm` URLs), generates a recursive proof task with the sample
   input `step_in = [4, 2]` (Vesta, 5 rounds), and `gen_and_send_proof_task` to the prover
   — on the prover this runs as an `Effect::Compute`, whose result is sent back;
4. polls `get_task_result`, which returns an explicit `ProofResult`
   (`Pending | Verified | Invalid`) so a timeout (still pending) is reported distinctly
   from a proof that returned but failed verification.

The rings wiring lives in `src/lib.rs` (`build_node`, `sample_input`, `run_proof`);
`src/main.rs` mounts the Yew app.

## Run

```sh
cargo install trunk          # one-time
trunk serve                  # → http://localhost:8080
```

Needs a prover peer on the overlay (its DID), a seed node's HTTP endpoint, and the circuit
files served over HTTP (`simple_bn256.r1cs`/`.wasm` from `examples/snark/circoms`, e.g.
`python3 -m http.server 8080` there).

## Test

Runs in a real **headless browser** (the `build_node` test needs IndexedDB + the browser
WebRTC stack); `webdriver.json` supplies the Chrome launch flags:

```sh
wasm-pack test --headless --chrome   # 2 tests
```

Requires a `chromedriver` whose version matches your installed Chrome (a mismatch makes
Chrome exit on launch). Firefox works too via `--firefox` + a matching `geckodriver`.
