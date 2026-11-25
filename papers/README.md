# Rings Network Papers

This directory contains the two authoritative specifications for the Rings Network architecture: the unified **Rings 1.0 Whitepaper** ([`rings.pdf`](./rings.pdf)) and the companion **DRanking 2.0 Protocol Specification** ([`dranking.pdf`](./dranking.pdf)).

| Paper | Source | BibTeX | PDF | Scope |
| :--- | :--- | :--- | :--- | :--- |
| **Rings: An Algebraic Structured Peer-to-Peer Network** | [`rings.tex`](./rings.tex) | [`cites.bib`](./cites.bib) | [`rings.pdf`](./rings.pdf) | Unified Rings 1.0 Whitepaper: circular identifier algebra, WebRTC/Wasm runtime confinement, range-proved & non-transitive Chord overlay, CRDT removal stability & provenance, durable local stores & replicated custody, fair slot accounting & load bounds, Sphinx onion Kleisli pipelines, and Stateright formal verification. |
| **DRanking: Decentralized Trust, Ranking, and Admission for Peer-to-Peer Networks** | [`dranking.tex`](./dranking.tex) | [`dranking.bib`](./dranking.bib) | [`dranking.pdf`](./dranking.pdf) | Rings 2.0 Decentralized Trust, Sybil-Resistant Ranking, and Verifiable Admission Layer built atop the Rings 1.0 identity, delegation (`rings-delegation-v1`), and counterparty-signed service receipt (`rings-service-receipt-v1`) wire primitives. |

---

## 1. Rings 1.0 Whitepaper (`rings.pdf`)

[**`rings.pdf`**](./rings.pdf) ([`rings.tex`](./rings.tex), [`cites.bib`](./cites.bib)) specifies the complete Rings 1.0 architecture implemented across `rings-core`, `rings-transport`, `rings-runtime`, `rings-codec`, `rings-rpc`, `rings-node`, `rings-aez`, `rings-measure`, `rings-network-policy`, `rings-turn`, `rings-turn-service`, `rings-dns-relay`, `rings-gateway`, `rings-webview`, and `rings-tui`:

- **Sections I–III — Algebraic Model, Multi-Source Identity & Transport Runtime**:
  - 160-bit circular identifier ring $R = \mathbb{Z}_{2^{160}}$ with clockwise distance $\delta_a(b)$, multi-source DID verification (`rings-delegation-v1` session authority), single-round-trip WebRTC SDP/ICE framing, Wasm capability confinement, slice categories $\mathbf{Set}/R \to \mathbf{Set}/N$, and free-monoid writer effect monads $\mathsf{T}^{\mathcal{E}}_A$.
- **Section IV — Chord Implementation (`PeerRing`), Range-Proved Finger Convergence & Non-Transitive WebRTC Overlay**:
  - Base Chord lookup and Zave's `CorrectChord` maintenance invariant.
  - **Range-Proved Finger Convergence** (`dht::finger::convergence`): Chord Range Lemma coverage proofs $\pi = (i, \mathrm{id}, e_\pi, s, \operatorname{end})$, zero-lookup local range confirmation over $(n, \operatorname{head}(n)]$, monotone evidence epochs $e(n,m)$, single-owner attempt machine ($\mathsf{Idle} \mid \mathsf{AwR} \mid \mathsf{AwA}$), cyclic starvation-free slot selection (`next_fix_slot`), and two-phase report claims (`NotifyPredecessor` / `ReportFloor`).
  - **Non-Transitive WebRTC Overlay**: bounded multi-predecessor sequence $\operatorname{Preds}_K(n) = \langle p_1, \dots, p_m \rangle$ ($m \le K = 4$) with `rectify_preds`, candidate dial backoff (`DialBackoff`, $\beta_0 = 5\,\text{s}$, $\beta_{\max} = 60\,\text{s}$), passive 1-hop gap-bridge hints in `TopoInfo`, and loop-free bridge-delegated delivery over $\operatorname{RouteStage} = \operatorname{aim}(\mathsf{Did}_\bot) \times \mathsf{bridged}(\mathbb{B})$ (Theorem: Bounded Single-Hop Bridge Delegation in $\le 2|V|$ hops worst-case and $\le 3$ hops across cliques with $C_{\text{sub}} \le K$).
- **Section V — CRDT Storage, Removal Stability & Cryptographic Data Provenance**:
  - Normalized data-topic carrier $x = (V, D, \rho, \beta)$ with Hybrid Logical Clock (HLC) timestamps $\tau_{\mathrm{HLC}} = (w, c, o)$.
  - Per-writer cryptographic provenance (`DataProvenance` over `rings-core:data-entry:mutation` with batched Merkle commitment `DataMutationClaim`) for `Append`/`Extend` and `Tombstone`.
  - Removal stability horizon $t \ge \tau(r) + H + \sigma$ (`DEFAULT_TTL_MS = 600 s`, data element horizon $H = 6\,000\,\text{s}$, $\sigma = 3\,\text{s}$; relay inbox up to $7\,\text{d}$), factored carrier $x = \mathsf{E}(x) \sqcup \mathsf{R}(x)$, **Theorem: No Resurrection**, tombstone bound $T_{\max} = 1024$ (`Storage Tombstone Full`, max carrier `33,731,696 B = 33.73 MB`), pure-read fetch-cache linearizability, and zero-tombstone signed lease heartbeats (`LeaseHeartbeat` across `rings-node:{online-node,service,mail}-descriptor` and `rings-turn:service-descriptor`).
- **Section VI — Durable Local Stores & Replicated Custody**:
  - Crash-meaning product authority lattice $C \times M$ (`RecordAuthority`), native `.p` presence markers ($P \subseteq R$), and per-platform crash durability (`F_FULLFSYNC` on macOS/iOS, `FlushFileBuffers` + `MoveFileExW(..., MOVEFILE_WRITE_THROUGH)` on Windows, `fsync` + parent dir sync on Linux, and IndexedDB `durability: "strict"` + `navigator.storage.persist()` on browsers).
  - Speculative group commit `put*` with undo logs, link-local volatile replay windows $\mathcal{W}_{s,g}$ (`Join|Sync|Leave|Send`), atomic receiver custody sets $(\mathcal{W}_s, \mathcal{K}_s)$, and age-gated (`DEFAULT_RELAY_AGE_GATE_MS = 2 min`, production profiles up to $30\,\text{min}$) $r$-member replicated relay custody chains $\mathcal{M}_r(k)$ (`r = 8` default) with digest-comparison repair.
- **Section VII — Fair Admission, Slot Accounting & Load Bounds**:
  - Cleanest-first connection slot roster (`SlotRoster`) over $\mathcal{R}(s) = 2(F + s + 1)$ slots ($F = 160, s = 4 \implies \mathcal{R}(4) = 330$; $s = 3 \implies 328$) tracking `(epoch, strikes, last_seen_ms)` to defeat Sybil identity rotation.
  - **Law: Slot Accounting** ($f_T + \lceil \mathcal{P}_T / n(\mathcal{A}(t)) \rceil$) and **Theorem: Multi-Table Slot Isolation** across the five shared node tables (Application Inbox, Relay Store, CRDT Removal Store `82,176` at `s=4`, Transaction Replay Table `15,680` / Origin Quota `4,074`, and Chunk Reassembly Buffer `210.5 MiB <= 256 MiB`).
  - Replay stream spent-horizon eviction ($H_{\text{spent}} = 600\,\text{s}$), generational credit reclaim (`Credit/Shrink/Returned`) with hard frame bound $B_F(s) = \mathcal{R}(s)(W + 3) + \mathcal{P}_F$ (`6,526` at `s=4`; `6,488` at `s=3`), single-charge chunk reassembly with credit-time $\hat{\tau}$ rate floor ($r_{\min} = 1024\,\text{B/s}$, $G = 15\,\text{s}$), and per-category reassembly barriers (`Control | Interactive | Bulk`).
- **Section VIII — Privacy Layer & Sphinx Onion Kleisli Pipeline**:
  - Static operation signature $\Sigma = \{\mathsf{relay}, \mathsf{tcp}, \mathsf{https}, \mathsf{udp}\} \cup \Sigma_I$ (`invoke<In, Out, L>` shapes over `In` $\in \{0,32,64,256,1024,16384\}$, `Out` $\in \{32,64,256,1024,16384\}$, latency $L \in \{10,100,500,2000\}\,\text{ms}$, paired with open backend names $\Omega$ such as `turn.ticket`) composed in the Kleisli category $\mathbf{Kl}(M)$.
  - Client-sealed loops $g \to m_1 \to \cdots \to g$ ($s = 2$ relays per segment, hop budget $\hat{H} = 14$), Hall-filter hop draw order, 2919-byte Sphinx header ($\alpha$ 33 B, 2870-byte routing block $\beta$ of fourteen 205-byte layers `HopRole::Transit | Terminal | GuardReturn`, $\gamma$ 16 B MAC), AEZ v5 wide-block body carry (`OnionBodyCipher`), $k = 4$ SURB batching, constant-rate cover loops, `SlotRoster`-backed sliced-Bloom replay admission ($K = 26$ slices, FP $\le 2^{-20}$), and Security Laws L5 (Noninterference), L5$'$ (Position Hiding), L6 (Client Path Authority), and L8 (Byte Unlinkability).
- **Sections IX–XII — DRanking Bridge, Empirical Benchmarks, Formal Verification & Related Work**:
  - Provisional service receipts (`rings-core:service-receipt:provisional:{provider,beneficiary}`) bridging into DRanking 2.0 `rings-service-receipt-v1`, quantitative Stateright/BFS verification across 16 models ($9,496,521$ reachable finite states / $9.50\times 10^6$, 54 refuted mutants), empirical benchmarks across all Rings 1.0 subsystems, and expanded related work.

---

## 2. DRanking 2.0 Specification (`dranking.pdf`)

[**`dranking.pdf`**](./dranking.pdf) ([`dranking.tex`](./dranking.tex), [`dranking.bib`](./dranking.bib)) specifies the Rings 2.0 decentralized trust, ranking, and admission layer built directly atop the Rings 1.0 cryptographic identity, delegation (`rings-delegation-v1`), and counterparty-signed service receipt (`rings-service-receipt-v1`) primitives:

- **Sybil Escape Bound & Receipt Flow Matrix**: proves that under zero-cost identity creation ($\mathsf{cost}(\mathsf{did}) = 0$), only counterparty-signed service receipts anchored to a non-Sybil seed distribution $s$ can bound adversarial trust share ($T_k(\mathsf{Adv}) \le \frac{\alpha g}{1 - \alpha}$).
- **Seed-Anchored Attenuated PageRank / EigenTrust**: specifies cap-budgeted receipt weights $W_k[b,a] \le w_{\max}$, normalized stochastic flow $C_k$, spectral contraction in $\ell_1$, and soulbound rank projection $\hat{t}_k(v)$.
- **Rank-Weighted VRF Committee Sortition & Nova IVC Folding**: combines verifiable random function (VRF) sortition with Nova-family Incrementally Verifiable Computation (IVC) folding so browser and lightweight nodes verify multi-epoch global admission state in $O(1)$ time.

---

## Reproducing Benchmarks and Formal-Verification Metrics

All Section X quantitative measurements and Stateright/BFS metrics in `rings.pdf` can be reproduced directly from the workspace root:

```bash
# Core empirical benchmarks (finger convergence, 2-clique routing, CRDT/LeaseHeartbeat, F_FULLFSYNC group commit, SlotRoster)
cargo test --profile ci -p rings-core --lib paper_benchmarks:: -- --ignored --nocapture --test-threads=1

# Stateright / BFS formal verification model metrics across all four checked crates
cargo test --profile ci -p rings-core --lib report_ -- --ignored --nocapture --test-threads=1
cargo test --profile ci -p rings-transport --lib report_ -- --ignored --nocapture --test-threads=1
cargo test --profile ci -p rings-transport --features native-webrtc --lib report_turn_tcp_model_metrics -- --ignored --nocapture --test-threads=1
cargo test --profile ci -p rings-node --lib report_ -- --ignored --nocapture --test-threads=1
cargo test --profile ci -p rings-turn-service --lib report_ -- --ignored --nocapture --test-threads=1

# Release crypto, Sphinx/Bloom, 5-node WebRTC loopback, and multi-flow gateway benchmarks
cargo test --release -p rings-aez --lib bench_paper_aez_throughput -- --ignored --nocapture --test-threads=1
cargo test --release -p rings-node --lib sphinx::tests::test_peel_cost:: -- --ignored --nocapture --test-threads=1
cargo test --release -p rings-node --lib onion::circuit::admission::tests::bloom::bench_paper_sliced_bloom_replay -- --ignored --nocapture --test-threads=1
cargo test --release -p rings-node --lib onion::tests::test_loop_native::bench_tcp_download_rate -- --ignored --nocapture --test-threads=1
RINGS_GATEWAY_BENCH_FLOWS=1024 cargo run --release -p rings-gateway --features test-support --example gateway-bench
```

---

## Building the Papers

`rings.tex` compiles cleanly under both **standard `pdflatex + bibtex`** (the arXiv submission engine) and `xelatex`:

```bash
# arXiv-native pdflatex + bibtex build for rings.tex
pdflatex -interaction=nonstopmode -halt-on-error rings.tex
bibtex rings
pdflatex -interaction=nonstopmode -halt-on-error rings.tex
pdflatex -interaction=nonstopmode -halt-on-error rings.tex

# Alternative xelatex build for both papers
latexmk -xelatex -interaction=nonstopmode -halt-on-error rings.tex dranking.tex

# Clean intermediate build artifacts while keeping rings.pdf and dranking.pdf
rm -f *.aux *.bbl *.blg *.fls *.fdb_latexmk *.log *.out *.run.xml *.synctex.gz *.xdv
```
