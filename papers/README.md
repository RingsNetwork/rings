# Papers

Repository-owned papers, their sources, and compiled reading copies live here.
Start with the Rings whitepaper for the original architecture, then consult the
current [security model](../SECURITY.md) and implementation documentation for runtime
behavior. A design in a paper is not, by itself, a shipped feature.

| Paper | Reading copy | Source and bibliography | Scope |
|---|---|---|---|
| Rings | [PDF](rings.pdf) | [LaTeX](rings.tex), [BibTeX](cites.bib) | Original protocol whitepaper; image assets are under `imgs/rings/` |
| DRanking | [PDF](dranking.pdf) | [LaTeX](dranking.tex), [BibTeX](dranking.bib) | Proposed verifiable ranking and admission; supersedes the 2023 *Ranking Protocol* draft |
| Finger convergence | [PDF](finger-convergence.pdf) | [LaTeX](finger-convergence.tex), [BibTeX](finger-convergence.bib) | Range-proved finger-convergence specification, written for Rings 0.26.0 |
| Onion Kleisli pipelines | [PDF](onion-kleisli-pipeline.pdf) | [LaTeX](onion-kleisli-pipeline.tex), [BibTeX](onion-kleisli-pipeline.bib) | Proposed onion circuits as client-sealed loops of registered operation symbols ([#834](https://github.com/RingsNetwork/rings/issues/834)); specification before the Phase 2 code |
| Durable records | [PDF](durable-records.pdf) | [LaTeX](durable-records.tex), [BibTeX](durable-records.bib) | Proposed presence and survival law of a node's local stores: authority lattice, presence witness, per-platform crash obligation, relay custody ([#911](https://github.com/RingsNetwork/rings/issues/911), [#912](https://github.com/RingsNetwork/rings/issues/912), [#915](https://github.com/RingsNetwork/rings/issues/915), [#923](https://github.com/RingsNetwork/rings/issues/923), [#936](https://github.com/RingsNetwork/rings/issues/936)), claimed browser durability ([#1036](https://github.com/RingsNetwork/rings/issues/1036)), and replicated relay custody ([#777](https://github.com/RingsNetwork/rings/issues/777)); specification before the code |
| Removal stability | [PDF](removal-stability.pdf) | [LaTeX](removal-stability.tex), [BibTeX](removal-stability.bib) | Proposed survival, eviction and size law of data-topic removes, the fetch-cache linearization, and lease heartbeats ([#918](https://github.com/RingsNetwork/rings/issues/918), [#920](https://github.com/RingsNetwork/rings/issues/920), [#921](https://github.com/RingsNetwork/rings/issues/921), [#922](https://github.com/RingsNetwork/rings/issues/922)); specification before the code |
| Load bounds | [PDF](load-bounds.pdf) | [LaTeX](load-bounds.tex), [BibTeX](load-bounds.bib) | Proposed node, edge and lane accounting under free identities: edge-keyed tables with floors and a pool, replay-stream horizon, credit reclaim, one charge per reassembled message ([#907](https://github.com/RingsNetwork/rings/issues/907), [#927](https://github.com/RingsNetwork/rings/issues/927), [#932](https://github.com/RingsNetwork/rings/issues/932), [#934](https://github.com/RingsNetwork/rings/issues/934), [#938](https://github.com/RingsNetwork/rings/issues/938)); specification before the code |

DRanking's current implementation is limited to
[provisional service receipts](../docs/src/advanced-topic/dranking-service-receipts.md).
The full ledger, global trust algebra, committee protocol, and admission policy in
the paper must not be described as deployed functionality.

## Build the PDFs

Install a TeX distribution with XeLaTeX, BibTeX, and `latexmk` (for example, TeX Live
or MacTeX). The sources use packages including IEEEtran, AMS math/theorems, TikZ,
algorithm/algpseudocode, and booktabs. DRanking additionally uses geometry,
tabularx, hyperref, and bookmark; the onion Kleisli pipeline paper uses stmaryrd
and multirow.

From the repository root:

```sh
cd papers
latexmk -xelatex -interaction=nonstopmode -halt-on-error rings.tex
latexmk -xelatex -interaction=nonstopmode -halt-on-error dranking.tex
latexmk -xelatex -interaction=nonstopmode -halt-on-error finger-convergence.tex
latexmk -xelatex -interaction=nonstopmode -halt-on-error onion-kleisli-pipeline.tex
latexmk -xelatex -interaction=nonstopmode -halt-on-error durable-records.tex
latexmk -xelatex -interaction=nonstopmode -halt-on-error removal-stability.tex
latexmk -xelatex -interaction=nonstopmode -halt-on-error load-bounds.tex
```

Run only the command for the paper being edited when making a focused change.
`latexmk` runs bibliography processing and the additional passes needed to resolve
citations and cross-references. PDFs are tracked; intermediate build files are ignored.

## Editing and reviewing

Keep source and PDF changes together. DRanking uses a single-column layout for its
long formal definitions and equations, full-width tables, and linked references.
Preserve theorem labels, mathematical statements, and bibliography keys during
formatting changes; describe any substantive protocol change separately.

Before submitting a paper change:

1. Build it until citations and cross-references resolve.
2. Inspect the log for undefined references, missing glyphs, and overfull boxes.
3. Render or open the resulting PDF and inspect every page, especially equations,
   table boundaries, figure labels, and the bibliography.
4. Commit the `.tex`, any changed `.bib` or assets, and the corresponding PDF.

To remove intermediates while retaining the PDFs:

```sh
latexmk -c
```

Use `-c`, not `-C`, when keeping the compiled reading copies. For the Rings citation
entry, see the [main README](../README.md#whitepaper).
