# Rings Network Papers

This directory contains the two authoritative specifications for the Rings Network architecture: the unified **Rings 1.0 Whitepaper** ([`rings.pdf`](./rings.pdf)) and the companion **DRanking 2.0 Protocol Specification** ([`dranking.pdf`](./dranking.pdf)).

| Paper | Source | BibTeX | PDF | Scope |
| :--- | :--- | :--- | :--- | :--- |
| **Rings: An Algebraic Structured Peer-to-Peer Network** | [`rings.tex`](./rings.tex) | [`cites.bib`](./cites.bib) | [`rings.pdf`](./rings.pdf) | Unified Rings 1.0 Whitepaper: circular identifier algebra, WebRTC/Wasm runtime confinement, range-proved & non-transitive Chord overlay, CRDT removal stability & provenance, durable local stores & replicated custody, fair slot accounting & load bounds, Sphinx onion Kleisli pipelines, and Stateright formal verification. |
| **DRanking: Decentralized Trust, Ranking, and Admission for Peer-to-Peer Networks** | [`dranking.tex`](./dranking.tex) | [`dranking.bib`](./dranking.bib) | [`dranking.pdf`](./dranking.pdf) | Rings 2.0 Decentralized Trust, Sybil-Resistant Ranking, and Verifiable Admission Layer built atop the Rings 1.0 identity, delegation (`rings-delegation-v1`), and counterparty-signed service receipt (`rings-service-receipt-v1`) wire primitives. |

---

## 1. Rings 1.0 Whitepaper (`rings.pdf`)

[**`rings.pdf`**](./rings.pdf) ([`rings.tex`](./rings.tex), [`cites.bib`](./cites.bib)) specifies the complete Rings 1.0 architecture implemented in `rings-core`, `rings-transport`, `rings-rpc`, and `rings-node`:

- **Sections I–III — Algebraic Model, Multi-Source Identity & Transport Runtime**:
  - 160-bit circular identifier ring $R = \mathbb{Z}_{2^{160}}$ with clockwise distance $\delta_a(b)$, multi-source DID verification (`rings-delegation-v1` session authority), single-round-trip WebRTC SDP/ICE framing, Wasm capability confinement, slice categories $\mathbf{Set}/R \to \mathbf{Set}/N$, and free-monoid writer effect monads $\mathsf{T}^{\mathcal{E}}_A$.
- **Section IV — Chord Implementation (`PeerRing`), Range-Proved Finger Convergence & Non-Transitive WebRTC Overlay**:
  - Base Chord lookup and Zave's `CorrectChord` maintenance invariant.
  - **Range-Proved Finger Convergence** (`dht::finger::convergence`): Chord Range Lemma coverage proofs $\pi = (i, \mathrm{id}, e_\pi, s, \operatorname{end})$, zero-lookup local range confirmation over $(n, \operatorname{head}(n)]$, monotone evidence epochs $e(n,m)$, single-owner attempt machine ($\mathsf{Idle} \mid \mathsf{AwR} \mid \mathsf{AwA}$), cyclic starvation-free slot selection (`next_fix_slot`), and two-phase report claims (`NotifyPredecessor` / `ReportFloor`).
  - **Non-Transitive WebRTC Overlay**: bounded multi-predecessor sequence $\operatorname{Preds}_K(n) = \langle p_1, \dots, p_m \rangle$ ($m \le K = 4$) with `rectify_preds`, candidate dial backoff (`DialBackoff`, $\beta_0 = 5\,\text{s}$, $\beta_{\max} = 60\,\text{s}$), passive 1-hop gap-bridge hints in `TopoInfo`, and loop-free bridge-delegated delivery over $\operatorname{RouteStage} = \operatorname{aim}(\mathsf{Did}_\bot) \times \mathsf{bridged}(\mathbb{B})$ (Theorem: Bounded Single-Hop Bridge Delegation in $\le 2|V|$ hops).
- **Section V — CRDT Storage, Removal Stability & Cryptographic Data Provenance**:
  - Normalized data-topic carrier $x = (V, D, \rho, \beta)$ with Hybrid Logical Clock (HLC) timestamps $\tau_{\mathrm{HLC}} = (w, c, o)$.
  - Per-writer cryptographic provenance (`DataProvenance` over `rings-data-carrier-v1\0`) for `Append`/`Extend` (`#1179`) and `Tombstone` (`#1220`).
  - Removal stability horizon $t \ge \tau(r) + H + \sigma$ ($H = 86\,400\,\text{s}$, $\sigma = 300\,\text{s}$), factored carrier $x = \mathsf{E}(x) \sqcup \mathsf{R}(x)$, **Theorem: No Resurrection**, tombstone bound $T_{\max} = 1024$ (`Storage Tombstone Full`), pure-read fetch-cache linearizability (Law 6), and zero-tombstone signed lease heartbeats (`LeaseHeartbeat` over `rings-lease-heartbeat-v1\0`).
- **Section VI — Durable Local Stores & Replicated Custody**:
  - Crash-meaning product authority lattice $C \times M$ (`RecordAuthority`), native `.p` presence markers ($P \subseteq R$), and per-platform crash durability (`F_FULLFSYNC` on macOS/iOS, `FlushFileBuffers` + `MoveFileExW(..., MOVEFILE_WRITE_THROUGH)` on Windows, `fsync` + parent dir sync on Linux, and IndexedDB `durability: "strict"` + `navigator.storage.persist()` on browsers).
  - Speculative group commit `put*` with undo logs, link-local volatile replay windows $\mathcal{W}_{s,g}$ (`Join|Sync|Leave|Send`), atomic receiver custody sets $(\mathcal{W}_s, \mathcal{K}_s)$, and age-gated ($A_{\min} = 30\,\text{min}$) $r$-member replicated relay custody chains $\mathcal{M}_r(k)$ with digest-comparison repair.
- **Section VII — Fair Admission, Slot Accounting & Load Bounds**:
  - Cleanest-first connection slot roster (`SlotRoster`) over $\mathcal{R}(s) = 2(F + s + 1)$ slots ($F = 160, s = 4 \implies \mathcal{R} = 330$) tracking `(epoch, strikes, last_seen_ms)` to defeat Sybil identity rotation.
  - **Law: Slot Accounting** ($f_T + \lceil \mathcal{P}_T / n(\mathcal{A}(t)) \rceil$) and **Theorem: Multi-Table Slot Isolation** across the five shared node tables (Application Inbox, Relay Store, CRDT Removal Store, Transaction Replay Table, and Chunk Reassembly Buffer).
  - Replay stream spent-horizon eviction ($H_{\text{spent}} = 600\,\text{s}$), generational credit reclaim (`Credit/Shrink/Returned`) with hard frame bound $B_F(s) \le c(s) + W_{\text{shrink}}$, single-charge chunk reassembly with credit-time $\hat{\tau}$ rate floor ($r_{\min} = 1024\,\text{B/s}$, $G = 15\,\text{s}$), and per-category reassembly barriers (`Control | Interactive | Bulk`).
- **Section VIII — Privacy Layer & Sphinx Onion Kleisli Pipeline**:
  - Static operation signature $\Sigma = \{\mathsf{relay}, \mathsf{tcp}, \mathsf{https}\} \cup \Sigma_{\mathsf{invoke}}$ (`dht.find_successor`, `dht.notify`, `dht.sync`, `storage.lookup`, `storage.store`, `service.lookup`, `service.register`, `direct.message`) composed in the Kleisli category $\mathbf{Kl}(M)$.
  - Client-sealed loops $g \to m_1 \to \cdots \to g$ ($s = 2$ relays per segment, hop budget $\hat{H} = 14$), Hall-filter hop draw order, 2870-byte Sphinx header with uniform 205-byte per-hop layer (`HopRole::Transit | Terminal | GuardReturn`), AEZ v5 wide-block body carry (`OnionBodyCipher`), $k = 4$ SURB batching, constant-rate cover loops, `SlotRoster`-backed sliced-Bloom replay admission ($K = 26$ slices, FP $\le 2^{-20}$), and Security Laws L5 (Noninterference), L5$'$ (Position Hiding), L6 (Client Path Authority), and L8 (Byte Unlinkability).
- **Sections IX–X — Counterparty-Signed Service Receipts, DRanking Bridge & Formal Verification**:
  - Dual-signed `ServiceReceipt` over `rings-service-receipt-v1\0` bridging into DRanking 2.0, Stateright model-checked state spaces across all six subsystems, and closed-form analytical and operational complexity bounds.

---

## 2. DRanking 2.0 Specification (`dranking.pdf`)

[**`dranking.pdf`**](./dranking.pdf) ([`dranking.tex`](./dranking.tex), [`dranking.bib`](./dranking.bib)) specifies the Rings 2.0 decentralized trust, ranking, and admission layer built directly atop the Rings 1.0 cryptographic identity, delegation (`rings-delegation-v1`), and counterparty-signed service receipt (`rings-service-receipt-v1`) primitives:

- **Sybil Escape Bound & Receipt Flow Matrix**: proves that under zero-cost identity creation ($\mathsf{cost}(\mathsf{did}) = 0$), only counterparty-signed service receipts anchored to a non-Sybil seed distribution $s$ can bound adversarial trust share ($T_k(\mathsf{Adv}) \le \frac{\alpha g}{1 - \alpha}$).
- **Seed-Anchored Attenuated PageRank / EigenTrust**: specifies cap-budgeted receipt weights $W_k[b,a] \le w_{\max}$, normalized stochastic flow $C_k$, spectral contraction in $\ell_1$, and soulbound rank projection $\hat{t}_k(v)$.
- **Rank-Weighted VRF Committee Sortition & Nova IVC Folding**: combines verifiable random function (VRF) sortition with Nova-family Incrementally Verifiable Computation (IVC) folding so browser and lightweight nodes verify multi-epoch global admission state in $O(1)$ time.

---

## Building the Papers

Both papers compile with `latexmk` and `xelatex`:

```bash
# Build both rings.pdf and dranking.pdf
latexmk -xelatex -interaction=nonstopmode -halt-on-error rings.tex dranking.tex

# Clean intermediate build artifacts while keeping rings.pdf and dranking.pdf
latexmk -c rings.tex dranking.tex
rm -f *.bbl *.run.xml *.synctex.gz *.xdv
```
