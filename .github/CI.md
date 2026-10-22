# Native CI test ownership

QACI's native critical path previously serialized debug builds, doctests, a debug
core/dummy suite, release workspace tests, and repeated package-scoped builds for
ignored gateway tests. Issue [#792](https://github.com/RingsNetwork/rings/issues/792)
records two successful baselines around 40 minutes and separates compilation from
harness execution. Those historical timings are not a benchmark of this change.

## Jobs and coverage

| Owner | Configuration | Coverage |
| --- | --- | --- |
| Build and test | Workspace defaults, `ci` profile | All workspace test targets; seven required exhaustive model searches; selected Onion/TUN/helper ignored tests; the workspace's doctests |
| Dummy transport tests | Core defaults + dummy; transport dummy without defaults, `ci` profile | Dummy integration, shell conformance, ordinary unit and inexpensive model tests; the PR decode-boundary smoke (every crate's `decode_boundary` tests, with a generated seed and 1024 cases) |
| Ring and hotspot deterministic replay | Core dummy without defaults, `ci` profile | Entire storm family, including all 30 identical replays and protection ablations, plus N=10/25/50 |
| Ring and hotspot N=10/25/50 | Same storm configuration, scheduled/manual | Existing scale and extended seed matrices |

The default native job owns the seven expensive searches listed in
`scripts/ci-native-model-tests.txt`. The dummy job excludes only those exact
names and the storm module. Both jobs validate the ownership list before running.
The rejoin model's native shell conformance tests remain in both applicable
configurations, including dummy-only send-terminal conformance. WASM model
execution, browser tests, FFI, sanitizers, Miri, and shipping release builds keep
their own jobs.

Storm retains the dedicated workflow's no-default-features configuration. Its
fixtures use deterministic secp256k1 keys, not the optional Ristretto adapter that
core defaults add. The rest of the core dummy suite still tests with defaults.
Storm now runs on every applicable PR and on every push to develop or master rather
than using path filters: changes to Cargo manifests, workspace dependencies, or
execution helpers must not bypass the sole storm owner. No repetitions, state-space bounds, seed
matrices, or non-vacuity assertions were reduced.

The `ci` profile inherits release optimization, overflow, and assertion semantics
but disables full LTO and stripping. `debug-assertions = false` is explicit because
the rejoin model's exhaustive searches are ignored when debug assertions are enabled.
The native ownership requirement fails if that test becomes ignored. The old
unoptimized core dummy execution is replaced with this optimized profile. The
separate debug workspace build is gone as well (#1002): clippy type-checks the debug
configuration, the `ci` build links every target, and the doctests run against the
`ci` libraries. This is a deliberate profile change, not a claim of preserving
every previous profile/test pairing.

## Browser jobs

The wasm32 gates are three parallel jobs, each over its own build graph:

| Job | Coverage |
| --- | --- |
| Clippy for wasm crates | Clippy for core, runtime, aez, transport, and node on wasm32; the browser graph carries no native gateway dependency |
| Build and test the frontend and dweb for wasm | Extension scripts and package, static shell, Onion WebView, frontend clippy and tests, dweb browser tests; caches the `frontend` and `examples/dweb` workspaces |
| Browser tests for wasm crates | Core (with the repair soak as its own invocation), runtime, aez, transport, and node browser tests |

## Caches

Every workflow restores Rust caches through `.github/actions/rust-cache`, which
writes a cache only on a push to develop or master. A PR restores the caches of its
base branch and the default branch and writes none, so the repository's 10 GB quota
holds those branches' generations; when every open PR also wrote its own, they
evicted one another before any PR reused one. QACI and the storm workflow run on
pushes to develop for this reason, as well as to check the merged commit. The
scheduled decode-boundary workflow builds a configuration no push builds, so it
restores no cache.

## Prebuilt executable contract

Compile with `cargo test --no-run --message-format=json --timings`, then give that
stream to `scripts/run-ci-tests.py`. The helper:

- Selects only test artifacts and deduplicates their executable paths.
- Lists ordinary and ignored tests before execution, checks required/excluded
  names, and rejects empty selections and ambiguous target names.
- Saves target, package, profile, features, executable, working directory, all
  test names, ignored names, and the selected names in an inventory artifact.
- Starts each harness from its Cargo package directory, restores native dynamic-
  library search paths (including the active toolchain's `libstd`), and propagates
  failures. Builds and execution must use the same Rust toolchain;
  `--target-libdir` can explicitly select its target library directory.
- Uses no Cargo command during execution. The native job's ignored tests reuse
  exactly the workspace build's executables instead of rebuilding smaller feature
  graphs. A privileged invocation requires one target and one exact ignored test;
  only that executable is elevated.

These are native, standard libtest harnesses. The helper does not support custom
harness protocols, benchmark-only inventories, cross-target runners, or doctests;
those require a separate explicit execution path. Native build-script library paths
inside the Cargo target directory are preserved. After `sudo`, only the explicit
loader search variable is restored with `env`; the entire caller environment is
not forwarded.

Run `python3 scripts/test-ci-tests.py` to check failure guards and exercise a real
dynamically linked Rust fixture through ordinary, ignored, and failing selections. Native and
dummy harness concurrency is capped at two tests per process to limit concurrent
large searches. This cap is a conservative scheduling choice, not a measured
memory bound for every future model.

## Measuring the result

Download `native-test-evidence`, `dummy-test-evidence`, and the storm trace artifact
from the same commit. Each contains the Cargo JSON stream, validated inventories,
and Cargo timing reports. Use job/step timestamps for wall time and libtest summaries
for execution time; do not attribute an entire build-and-test step to tests.

Compare repeated warm-cache runs and at least one cold-cache run. Record total
runner time as well as the slowest required job, and compare test inventories by
feature/profile/platform rather than comparing only aggregate counts. The below-
20-minute PR target in #792 remains unverified until those GitHub measurements are
available. Local macOS results cannot establish Linux TUN coverage or hosted-runner
latency. This implementation does not close the issue's measurement acceptance
criteria on its own.
