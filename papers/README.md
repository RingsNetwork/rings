# Papers

This directory contains the repository-owned Rings paper copies.

- `rings.pdf`: compiled Rings whitepaper.
- `rings.tex`: LaTeX source for the Rings whitepaper.
- `cites.bib`: bibliography used by `rings.tex`.
- `imgs/rings/path.png`: image asset referenced by `rings.tex`.
- `dranking.pdf`: compiled DRanking protocol paper.
- `dranking.tex`: LaTeX source for the DRanking protocol paper (supersedes the 2023 *Ranking Protocol* draft).
- `dranking.bib`: bibliography used by `dranking.tex`.

Build with:

```sh
latexmk -xelatex rings.tex
latexmk -xelatex dranking.tex
```
