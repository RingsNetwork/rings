#!/usr/bin/env python3
"""Compile the Rings Network papers (`papers/*.tex`) into native Web + PDF reader pages under `<site>/papers/`.

Invoked by `finish_site.py` during both `qaci.yml` and `deploy-pages.yml` using only the Python
standard library so CI needs no extra packages or TeX installation. Emits:
- `<site>/papers/index.html` and `<site>/papers/index.md`
- `<site>/papers/papers-telemetry.js` (first-party cookie-free read/scroll/preview/download beacon)
- `<site>/papers/rings/index.html`, `<site>/papers/rings.md`, `<site>/papers/rings.pdf`, `<site>/papers/cites.bib`
- `<site>/papers/dranking/index.html`, `<site>/papers/dranking.md`, `<site>/papers/dranking.pdf`, `<site>/papers/dranking.bib`
"""

from __future__ import annotations

import html
import json
import math
import re
import shutil
from dataclasses import dataclass, field
from pathlib import Path

SITE_URL = "https://rings.rs/"
PAPERS_URL = f"{SITE_URL}papers/"
REPO_ROOT = Path(__file__).resolve().parents[2]
PAPERS_SOURCE_DIR = REPO_ROOT / "papers"
PAPERS_CSS = (PAPERS_SOURCE_DIR / "papers.css").read_text(encoding="utf-8").strip()

THEOREM_ENVS: dict[str, tuple[str, str]] = {
    "definition": ("Definition", "definition"),
    "constraint": ("Constraint", "constraint"),
    "assumption": ("Assumption", "assumption"),
    "proposition": ("Proposition", "proposition"),
    "theorem": ("Theorem", "theorem"),
    "lemma": ("Lemma", "lemma"),
    "corollary": ("Corollary", "corollary"),
    "law": ("Law", "law"),
    "invariant": ("Invariant", "invariant"),
    "obligation": ("Obligation", "obligation"),
    "remark": ("Remark", "remark"),
}

ROMAN_NUMERALS = [
    "",
    "I",
    "II",
    "III",
    "IV",
    "V",
    "VI",
    "VII",
    "VIII",
    "IX",
    "X",
    "XI",
    "XII",
    "XIII",
    "XIV",
    "XV",
]


@dataclass(frozen=True)
class PaperSpec:
    """Configuration and metadata for one paper in `papers/`."""

    slug: str
    series_badge: str
    short_title: str
    tex_file: str
    bib_file: str
    pdf_file: str
    download_name: str
    published: str
    updated: str
    pages: int
    roman_sections: bool
    tags: tuple[str, ...]
    bibtex_key: str


PAPER_SPECS: tuple[PaperSpec, ...] = (
    PaperSpec(
        slug="rings",
        series_badge="Rings 1.0 Unified Whitepaper",
        short_title="Rings 1.0 Whitepaper",
        tex_file="rings.tex",
        bib_file="cites.bib",
        pdf_file="rings.pdf",
        download_name="rings-whitepaper-1.0.pdf",
        published="2023-02-08",
        updated="2026-11-28",
        pages=23,
        roman_sections=True,
        tags=(
            "Chord DHT",
            "Non-Transitive WebRTC",
            "Actor-Scoped CRDT",
            "Crash Consistency",
            "SlotRoster Admission",
            "Sphinx & AEZ v5",
            "TURN Onion Relay",
            "Stateright Verification",
        ),
        bibtex_key="rings-whitepaper-2026",
    ),
    PaperSpec(
        slug="dranking",
        series_badge="Rings 2.0 Protocol Specification",
        short_title="DRanking 2.0 Specification",
        tex_file="dranking.tex",
        bib_file="dranking.bib",
        pdf_file="dranking.pdf",
        download_name="rings-dranking-2.0.pdf",
        published="2023-02-11",
        updated="2026-09-01",
        pages=8,
        roman_sections=False,
        tags=(
            "Sybil Resistance",
            "Service Receipts",
            "Attenuated EigenTrust",
            "VRF Sortition",
            "Nova IVC Folding",
            "Soulbound Rank",
        ),
        bibtex_key="rings-dranking-2026",
    ),
)


@dataclass
class BibEntry:
    """Parsed BibTeX record."""

    key: str
    entry_type: str
    fields: dict[str, str]


@dataclass
class TocItem:
    """One section or subsection entry in the sticky Table of Contents."""

    level: int
    number: str
    title_html: str
    anchor: str


@dataclass
class ParsedPaper:
    """Compiled paper ready for HTML and Markdown rendering."""

    spec: PaperSpec
    title_plain: str
    title_html: str
    author_plain: str
    thanks_html: str
    abstract_html: str
    abstract_plain: str
    keywords_html: str
    scope_note_html: str
    body_html: str
    toc: list[TocItem]
    bib_html: str
    markdown: str
    pdf_size_kb: int
    canonical_bibtex: str


def strip_latex_comments(source: str) -> str:
    """Remove unescaped `%` comments while preserving line structure."""

    out_lines: list[str] = []
    for line in source.splitlines():
        idx = 0
        cut = len(line)
        while idx < len(line):
            if line[idx] == "%":
                backslashes = 0
                j = idx - 1
                while j >= 0 and line[j] == "\\":
                    backslashes += 1
                    j -= 1
                if backslashes % 2 == 0:
                    cut = idx
                    break
            idx += 1
        trimmed = line[:cut]
        if cut < len(line) and not trimmed.strip():
            continue
        out_lines.append(trimmed)
    return "\n".join(out_lines)


def find_brace_group(text: str, start: int) -> tuple[str, int] | None:
    """Given `start` pointing at `{`, return `(inner_content, end_index_after_close_brace)`."""

    if start >= len(text) or text[start] != "{":
        return None
    depth = 0
    i = start
    while i < len(text):
        ch = text[i]
        if ch == "\\":
            i += 2
            continue
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                return text[start + 1 : i], i + 1
        i += 1
    return None


def find_bracket_group(text: str, start: int) -> tuple[str, int] | None:
    """Given `start` pointing at `[`, return `(inner_content, end_index_after_close_bracket)`."""

    if start >= len(text) or text[start] != "[":
        return None
    depth = 0
    brace_depth = 0
    i = start
    while i < len(text):
        ch = text[i]
        if ch == "\\":
            i += 2
            continue
        if ch == "{":
            brace_depth += 1
        elif ch == "}":
            brace_depth = max(0, brace_depth - 1)
        elif brace_depth == 0 and ch == "[":
            depth += 1
        elif brace_depth == 0 and ch == "]":
            depth -= 1
            if depth == 0:
                return text[start + 1 : i], i + 1
        i += 1
    return None


def parse_bib_file(path: Path) -> dict[str, BibEntry]:
    """Parse a `.bib` file into a dictionary keyed by citation key."""

    if not path.is_file():
        return {}
    raw = strip_latex_comments(path.read_text(encoding="utf-8"))
    entries: dict[str, BibEntry] = {}
    pos = 0
    header_re = re.compile(r"@([A-Za-z]+)\s*\{\s*([^,\s]+)\s*,")
    field_re = re.compile(r"\s*([A-Za-z0-9_-]+)\s*=\s*")
    while True:
        match = header_re.search(raw, pos)
        if not match:
            break
        entry_type = match.group(1).lower()
        key = match.group(2).strip()
        brace_open = raw.find("{", match.start())
        grouped = find_brace_group(raw, brace_open)
        if not grouped:
            break
        body, end_pos = grouped
        pos = end_pos
        comma_idx = body.find(",")
        if comma_idx < 0:
            continue
        fields_str = body[comma_idx + 1 :]
        fields: dict[str, str] = {}
        fpos = 0
        while fpos < len(fields_str):
            fm = field_re.match(fields_str, fpos)
            if not fm:
                next_comma = fields_str.find(",", fpos)
                if next_comma < 0:
                    break
                fpos = next_comma + 1
                continue
            fname = fm.group(1).lower()
            fpos = fm.end()
            while fpos < len(fields_str) and fields_str[fpos].isspace():
                fpos += 1
            if fpos >= len(fields_str):
                break
            if fields_str[fpos] == "{":
                val_group = find_brace_group(fields_str, fpos)
                if not val_group:
                    break
                fval, fpos = val_group
            elif fields_str[fpos] == '"':
                end_q = fields_str.find('"', fpos + 1)
                if end_q < 0:
                    break
                fval = fields_str[fpos + 1 : end_q]
                fpos = end_q + 1
            else:
                end_c = fields_str.find(",", fpos)
                if end_c < 0:
                    end_c = len(fields_str)
                fval = fields_str[fpos:end_c].strip()
                fpos = end_c
            fields[fname] = " ".join(fval.split())
            while fpos < len(fields_str) and fields_str[fpos] in " \t\r\n,":
                fpos += 1
        entries[key] = BibEntry(key=key, entry_type=entry_type, fields=fields)
    return entries


def normalize_bib_accents(text: str) -> str:
    """Convert common BibTeX accents and monospace wrappers into plain Unicode text."""

    text = re.sub(r"\{\\c\{c\}\}|\\c\{c\}", "ç", text)
    text = re.sub(r"\{\\'e\}|\\'e", "é", text)
    text = re.sub(r"\{\\`e\}|\\`e", "è", text)
    text = re.sub(r"\{\\\"o\}|\\\"o", "ö", text)
    text = re.sub(r"\{\\\"u\}|\\\"u", "ü", text)
    return text


def clean_bib_plain(text: str) -> str:
    """Convert BibTeX field formatting to clean plain text (e.g. for HTML `title` attributes)."""

    text = normalize_bib_accents(text)
    text = re.sub(r"\\(?:texttt|code|emph|textit|textbf|url)\{([^}]*)\}", r"\1", text)
    text = text.replace("{", "").replace("}", "")
    text = text.replace("---", "—").replace("--", "–").replace("~", " ")
    text = text.replace("\\&", "&").replace("\\_", "_").replace("\\%", "%")
    return " ".join(text.split())


def clean_bib_text(text: str) -> str:
    """Convert BibTeX field formatting to clean HTML."""

    stashed: list[str] = []

    def stash_fragment(frag: str) -> str:
        idx = len(stashed)
        stashed.append(frag)
        return f"\x00BIB{idx}\x00"

    text = normalize_bib_accents(text)
    text = re.sub(
        r"\\url\{([^}]+)\}",
        lambda m: stash_fragment(
            f'<a href="{html.escape(m.group(1).strip(), quote=True)}" target="_blank" rel="noreferrer">{html.escape(m.group(1).strip())}</a>'
        ),
        text,
    )
    text = re.sub(
        r"\\(?:texttt|code)\{([^}]+)\}",
        lambda m: stash_fragment(
            f'<code class="paper-code">{html.escape(m.group(1).replace("\\_", "_"))}</code>'
        ),
        text,
    )
    text = text.replace("{", "").replace("}", "")
    text = text.replace("---", "—").replace("--", "–").replace("~", " ")
    text = text.replace("\\&", "&").replace("\\_", "_").replace("\\%", "%")
    escaped = html.escape(text)
    for idx, frag in enumerate(stashed):
        escaped = escaped.replace(f"\x00BIB{idx}\x00", frag)
    return escaped


def sanitize_anchor(label: str) -> str:
    """Map a LaTeX label to a safe HTML id."""

    return re.sub(r"[^A-Za-z0-9_.:-]+", "-", label.strip())


@dataclass
class CompilerContext:
    """Two-pass state for resolving cross-references and citations."""

    spec: PaperSpec
    bib_db: dict[str, BibEntry]
    labels: dict[str, tuple[str, str, str]] = field(default_factory=dict)
    cited_order: list[str] = field(default_factory=list)
    cited_num: dict[str, int] = field(default_factory=dict)
    toc: list[TocItem] = field(default_factory=list)

    def cite_number(self, key: str) -> int:
        key = key.strip()
        if key not in self.cited_num:
            self.cited_order.append(key)
            self.cited_num[key] = len(self.cited_order)
        return self.cited_num[key]


def smallcaps_math(word: str) -> str:
    """Render `\\textsc{Word}` inside KaTeX math mode as true typographic small caps."""

    w = word.strip()
    if not w:
        return ""
    if len(w) == 1:
        return f"\\mathrm{{{w.upper()}}}"
    head = w[0].upper()
    tail = w[1:].upper()
    return f"\\mathrm{{{head}{{\\scriptstyle {tail}}}}}"


def normalize_math_for_katex(math_tex: str, ctx: CompilerContext | None = None) -> str:
    """Normalize custom LaTeX macros, citations, and cross-references inside math mode for KaTeX."""

    out = math_tex
    out = re.sub(r"\\label\{[^}]*\}", "", out)
    out = re.sub(r"\\allowbreak(?:\{\}|\s*)", "", out)
    out = out.replace("\\llbracket", "[\\![")
    out = out.replace("\\rrbracket", "]\\!]")
    out = out.replace("\\mbox{-}", "\\text{-}")
    out = re.sub(r"\\mbox\{([^}]*)\}", r"\\text{\1}", out)

    if ctx is not None:
        def math_cite(inner: str) -> str:
            keys = [k.strip() for k in inner.split(",") if k.strip()]
            if not keys:
                return ""
            parts = [
                f"\\href{{#bib-{sanitize_anchor(k)}}}{{\\text{{{ctx.cite_number(k)}}}}}"
                for k in keys
            ]
            return f"\\text{{[}}{',\\,'.join(parts)}\\text{{]}}"

        def math_eqref(inner: str) -> str:
            lbl = inner.strip()
            info = ctx.labels.get(lbl)
            disp = info[1] if info else lbl
            return f"\\href{{#{sanitize_anchor(lbl)}}}{{\\text{{({disp})}}}}"

        def math_ref(inner: str) -> str:
            lbl = inner.strip()
            info = ctx.labels.get(lbl)
            disp = info[1] if info else lbl
            return f"\\href{{#{sanitize_anchor(lbl)}}}{{\\text{{{disp}}}}}"

        out = re.sub(r"~(?=\\(?:cite|eqref|ref)\{)", r"\\ ", out)
        out = replace_command_1arg(out, "cite", math_cite)
        out = replace_command_1arg(out, "eqref", math_eqref)
        out = replace_command_1arg(out, "ref", math_ref)

    out = replace_command_1arg(out, "textsc", smallcaps_math)
    out = replace_command_1arg(out, "emph", lambda s: f"\\textit{{{s}}}")
    out = replace_command_1arg(out, "code", lambda s: f"\\texttt{{{s}}}")

    return out.strip()


def extract_label(text: str) -> tuple[str | None, str]:
    """Extract the first `\\label{...}` from `text` and return `(label, stripped_text)`."""

    m = re.search(r"\\label\{([^}]+)\}", text)
    if not m:
        return None, text
    lbl = m.group(1).strip()
    cleaned = text[: m.start()] + text[m.end() :]
    return lbl, cleaned


def replace_command_1arg(text: str, cmd: str, transform) -> str:
    """Replace all occurrences of `\\cmd{arg}` using balanced brace matching."""

    needle = f"\\{cmd}{{"
    pos = 0
    out: list[str] = []
    while True:
        idx = text.find(needle, pos)
        if idx < 0:
            out.append(text[pos:])
            break
        if idx > 0 and text[idx - 1] == "\\":
            out.append(text[pos : idx + len(needle)])
            pos = idx + len(needle)
            continue
        out.append(text[pos:idx])
        grp = find_brace_group(text, idx + len(cmd) + 1)
        if not grp:
            out.append(text[idx:])
            break
        inner, end_idx = grp
        out.append(transform(inner))
        pos = end_idx
    return "".join(out)


def replace_href(text: str, render_inner, stash_fn) -> str:
    """Replace `\\href{url}{label}` using balanced brace matching and stash the resulting anchor."""

    needle = "\\href{"
    pos = 0
    out: list[str] = []
    while True:
        idx = text.find(needle, pos)
        if idx < 0:
            out.append(text[pos:])
            break
        out.append(text[pos:idx])
        g1 = find_brace_group(text, idx + 5)
        if not g1:
            out.append(text[idx:])
            break
        url, next_idx = g1
        while next_idx < len(text) and text[next_idx].isspace():
            next_idx += 1
        g2 = find_brace_group(text, next_idx) if next_idx < len(text) and text[next_idx] == "{" else None
        if not g2:
            out.append(text[idx:next_idx])
            pos = next_idx
            continue
        label, end_idx = g2
        url_clean = url.replace("\\_", "_").replace("\\%", "%").strip()
        out.append(
            stash_fn(
                f'<a href="{html.escape(url_clean, quote=True)}" target="_blank" rel="noreferrer">{render_inner(label)}</a>'
            )
        )
        pos = end_idx
    return "".join(out)


def stash_inline_math(text: str, stash_fn, ctx: CompilerContext) -> str:
    """Extract `$...$` (respecting nested `{...}` braces) and `\\(...\\)` spans into the unified HTML stash."""

    out: list[str] = []
    i = 0
    n = len(text)
    while i < n:
        if text[i] == "\\" and i + 1 < n and text[i + 1] == "(":
            end = text.find("\\)", i + 2)
            if end >= 0:
                norm = normalize_math_for_katex(text[i + 2 : end], ctx)
                out.append(stash_fn(f'<span class="paper-math-inline">\\({html.escape(norm)}\\)</span>'))
                i = end + 2
                continue
        if text[i] == "\\" and i + 1 < n and text[i + 1] == "$":
            out.append("$")
            i += 2
            continue
        if text[i] == "$" and (i + 1 >= n or text[i + 1] != "$"):
            j = i + 1
            brace_depth = 0
            while j < n:
                if text[j] == "\\":
                    j += 2
                    continue
                if text[j] == "{":
                    brace_depth += 1
                elif text[j] == "}":
                    brace_depth = max(0, brace_depth - 1)
                elif text[j] == "$" and brace_depth == 0:
                    break
                j += 1
            if j < n and text[j] == "$":
                norm = normalize_math_for_katex(text[i + 1 : j], ctx)
                out.append(stash_fn(f'<span class="paper-math-inline">\\({html.escape(norm)}\\)</span>'))
                i = j + 1
                continue
        out.append(text[i])
        i += 1
    return "".join(out)


def render_inline_latex(text: str, ctx: CompilerContext) -> str:
    """Convert inline LaTeX markup, math, citations, and references into semantic HTML."""

    if not text:
        return ""

    html_stash: list[str] = []

    def put_stash(fragment: str) -> str:
        idx = len(html_stash)
        html_stash.append(fragment)
        return f"\x00HTML{idx}\x00"

    # Unwrap `\texorpdfstring{tex}{pdf}` -> keep `tex`
    while "\\texorpdfstring{" in text:
        idx = text.find("\\texorpdfstring{")
        g1 = find_brace_group(text, idx + 15)
        if not g1:
            break
        tex_part, nxt = g1
        g2 = find_brace_group(text, nxt) if nxt < len(text) and text[nxt] == "{" else None
        if not g2:
            break
        _, end_idx = g2
        text = text[:idx] + tex_part + text[end_idx:]

    # Citations `\cite{k1,k2}`
    def render_cite(inner: str) -> str:
        keys = [k.strip() for k in inner.split(",") if k.strip()]
        links: list[str] = []
        for k in keys:
            num = ctx.cite_number(k)
            bib = ctx.bib_db.get(k)
            tip = ""
            if bib:
                t = clean_bib_plain(bib.fields.get("title", ""))
                yr = clean_bib_plain(bib.fields.get("year", ""))
                tip = f' title="{html.escape(f"{t} ({yr})", quote=True)}"'
            links.append(f'<a href="#bib-{sanitize_anchor(k)}"{tip}>{num}</a>')
        return put_stash(f'<span class="paper-cite">[{", ".join(links)}]</span>')

    text = replace_command_1arg(text, "cite", render_cite)

    # Equation references `\eqref{...}` and general references `\ref{...}`
    def render_eqref(inner: str) -> str:
        lbl = inner.strip()
        info = ctx.labels.get(lbl)
        anchor_id = sanitize_anchor(lbl)
        disp = info[1] if info else lbl
        return put_stash(f'<a href="#{anchor_id}">({html.escape(disp)})</a>')

    def render_ref(inner: str) -> str:
        lbl = inner.strip()
        info = ctx.labels.get(lbl)
        anchor_id = sanitize_anchor(lbl)
        disp = info[1] if info else lbl
        return put_stash(f'<a href="#{anchor_id}">{html.escape(disp)}</a>')

    text = replace_command_1arg(text, "eqref", render_eqref)
    text = replace_command_1arg(text, "ref", render_ref)

    # URLs and hrefs
    def render_url(inner: str) -> str:
        u = inner.replace("\\_", "_").replace("\\%", "%").strip()
        return put_stash(
            f'<a href="{html.escape(u, quote=True)}" target="_blank" rel="noreferrer">{html.escape(u)}</a>'
        )

    text = replace_command_1arg(text, "url", render_url)
    text = replace_href(text, lambda s: render_inline_latex(s, ctx), put_stash)

    # Stash inline math into the SAME `html_stash` before `\code`/`\textbf`/`\emph` wrappers
    text = stash_inline_math(text, put_stash, ctx)

    def render_code(inner: str) -> str:
        clean = (
            re.sub(r"\\allowbreak(?:\{\}|\s*)", "", inner)
            .replace("\\_", "_")
            .replace("\\{", "{")
            .replace("\\}", "}")
            .replace("\\backslash", "\\")
            .replace("~", " ")
        )
        return put_stash(f'<code class="paper-code">{html.escape(clean)}</code>')

    text = replace_command_1arg(text, "code", render_code)
    text = replace_command_1arg(text, "texttt", render_code)
    text = replace_command_1arg(
        text, "textsc", lambda s: put_stash(f'<span style="font-variant:small-caps">{render_inline_latex(s, ctx)}</span>')
    )
    text = replace_command_1arg(text, "textbf", lambda s: put_stash(f"<strong>{render_inline_latex(s, ctx)}</strong>"))
    text = replace_command_1arg(text, "emph", lambda s: put_stash(f"<em>{render_inline_latex(s, ctx)}</em>"))
    text = replace_command_1arg(text, "textit", lambda s: put_stash(f"<em>{render_inline_latex(s, ctx)}</em>"))

    # Strip presentation switches
    text = re.sub(
        r"\\(?:footnotesize|scriptsize|normalsize|small|large|Large|bfseries|itshape|raggedright|arraybackslash|noindent|leavevmode|qed)\b\s*",
        "",
        text,
    )
    text = re.sub(r"\\vspace\*?\{[^}]*\}", "", text)
    text = re.sub(r"\\hspace\*?\{[^}]*\}", " ", text)
    text = text.replace("\\qquad", "\u2003\u2003").replace("\\quad", "\u2003")
    text = text.replace("\\,", "\u202f").replace("\\;", " ").replace("\\:", " ").replace("\\!", "").replace("\\/", "")

    # Typographic ligatures and escapes
    text = text.replace("---", "—").replace("--", "–")
    text = text.replace("\\ ", " ").replace("~", "\u00a0")
    text = text.replace("\\&", "&").replace("\\%", "%").replace("\\_", "_")
    text = text.replace("\\#", "#").replace("\\{", "{").replace("\\}", "}")
    text = text.replace("``", "“").replace("''", "”")
    # Unwrap bare `{word}` grouping braces left in prose
    text = text.replace("{", "").replace("}", "")

    escaped = html.escape(text, quote=False)
    # Restore unified stash in reverse topological order
    for idx in range(len(html_stash) - 1, -1, -1):
        escaped = escaped.replace(f"\x00HTML{idx}\x00", html_stash[idx])
    return escaped.strip()


def split_top_level(text: str, sep: str) -> list[str]:
    """Split `text` by `sep` ignoring occurrences inside `{...}` or nested `\\begin...\\end`."""

    parts: list[str] = []
    cur: list[str] = []
    brace_depth = 0
    env_depth = 0
    math_inline = False
    i = 0
    n = len(text)
    sep_len = len(sep)
    while i < n:
        if text.startswith("\\begin{", i):
            env_depth += 1
            cur.append("\\begin{")
            i += 7
            continue
        if text.startswith("\\end{", i):
            env_depth = max(0, env_depth - 1)
            cur.append("\\end{")
            i += 5
            continue
        ch = text[i]
        if ch == "\\" and i + 1 < n and not text.startswith(sep, i):
            cur.append(text[i : i + 2])
            i += 2
            continue
        if ch == "$" and env_depth == 0:
            math_inline = not math_inline
            cur.append(ch)
            i += 1
            continue
        if not math_inline:
            if ch == "{":
                brace_depth += 1
            elif ch == "}":
                brace_depth = max(0, brace_depth - 1)
        if brace_depth == 0 and env_depth == 0 and not math_inline and text.startswith(sep, i):
            parts.append("".join(cur))
            cur = []
            i += sep_len
            if sep == "\\\\" and i < n and text[i] == "[":
                brk = find_bracket_group(text, i)
                if brk:
                    i = brk[1]
            continue
        cur.append(ch)
        i += 1
    parts.append("".join(cur))
    return parts


def parse_col_alignments(col_spec: str) -> list[str]:
    """Parse LaTeX tabular column alignment specification into `left` / `right` / `center`."""

    cleaned = re.sub(r"@\{[^}]*\}", "", col_spec)
    cleaned = re.sub(r">\{[^}]*\}", "", cleaned)
    aligns: list[str] = []
    i = 0
    while i < len(cleaned):
        c = cleaned[i]
        if c in ("l", "p", "X", "Y"):
            aligns.append("left")
            if c == "p" and i + 1 < len(cleaned) and cleaned[i + 1] == "{":
                grp = find_brace_group(cleaned, i + 1)
                if grp:
                    i = grp[1] - 1
        elif c == "r":
            aligns.append("num")
        elif c == "c":
            aligns.append("center")
        i += 1
    return aligns


def render_table_block(body: str, num_str: str, anchor_override: str | None, ctx: CompilerContext) -> str:
    """Render `\\begin{table}` / `\\begin{table*}` into a responsive HTML `<figure>` table."""

    lbl, body = extract_label(body)
    anchor = anchor_override or (sanitize_anchor(lbl) if lbl else f"table-{num_str}")

    caption_html = ""
    cap_idx = body.find("\\caption{")
    if cap_idx >= 0:
        cap_grp = find_brace_group(body, cap_idx + 8)
        if cap_grp:
            cap_raw, cap_end = cap_grp
            _, cap_raw = extract_label(cap_raw)
            caption_html = render_inline_latex(cap_raw, ctx)
            body = body[:cap_idx] + body[cap_end:]

    note_html = ""
    mp_match = re.search(r"\\begin\{minipage\}(?:\[[^\]]*\])?\{[^}]*\}(.*?)\\end\{minipage\}", body, re.S)
    if mp_match:
        note_html = render_inline_latex(mp_match.group(1), ctx)
        body = body[: mp_match.start()] + body[mp_match.end() :]

    tab_match = re.search(r"\\begin\{(tabularx?)\}", body)
    if not tab_match:
        return ""
    env_name = tab_match.group(1)
    cursor = tab_match.end()
    while cursor < len(body) and body[cursor].isspace():
        cursor += 1
    if env_name == "tabularx" and cursor < len(body) and body[cursor] == "{":
        wgrp = find_brace_group(body, cursor)
        if wgrp:
            cursor = wgrp[1]
    while cursor < len(body) and body[cursor].isspace():
        cursor += 1
    col_spec = ""
    if cursor < len(body) and body[cursor] == "{":
        cgrp = find_brace_group(body, cursor)
        if cgrp:
            col_spec, cursor = cgrp
    end_tag = f"\\end{{{env_name}}}"
    end_idx = body.rfind(end_tag)
    tab_inner = body[cursor:end_idx] if end_idx >= 0 else body[cursor:]
    aligns = parse_col_alignments(col_spec)

    raw_rows = split_top_level(tab_inner, "\\\\")
    header_rows: list[str] = []
    body_rows: list[str] = []
    seen_First_midrule = False

    for raw_row in raw_rows:
        row_str = raw_row.strip()
        has_midrule = "\\midrule" in row_str
        row_str = re.sub(r"\\(?:toprule|midrule|bottomrule|hline)\b", "", row_str)
        row_str = re.sub(r"\\cmidrule(?:\([^)]*\))?\{[^}]*\}", "", row_str).strip()
        if not row_str:
            if has_midrule and header_rows:
                seen_First_midrule = True
            continue
        if has_midrule and header_rows:
            seen_First_midrule = True

        cells = split_top_level(row_str, "&")
        tag = "th" if not seen_First_midrule else "td"
        rendered_cells: list[str] = []
        col_cursor = 0
        for cell in cells:
            c_trim = cell.strip()
            if c_trim.startswith("\\multicolumn{"):
                g1 = find_brace_group(c_trim, 12)
                g2 = find_brace_group(c_trim, g1[1]) if g1 and g1[1] < len(c_trim) else None
                g3 = find_brace_group(c_trim, g2[1]) if g2 and g2[1] < len(c_trim) else None
                if g1 and g2 and g3:
                    span = int(g1[0].strip() or "1")
                    mc_aligns = parse_col_alignments(g2[0])
                    cls = f' class="{mc_aligns[0]}"' if mc_aligns and mc_aligns[0] != "left" else ""
                    content = render_inline_latex(g3[0], ctx)
                    rendered_cells.append(f'<{tag} colspan="{span}"{cls}>{content}</{tag}>')
                    col_cursor += span
                    continue
            cls = ""
            if col_cursor < len(aligns) and aligns[col_cursor] != "left":
                cls = f' class="{aligns[col_cursor]}"'
            rendered_cells.append(f"<{tag}{cls}>{render_inline_latex(c_trim, ctx)}</{tag}>")
            col_cursor += 1

        tr_html = f"<tr>{''.join(rendered_cells)}</tr>"
        if not seen_First_midrule:
            header_rows.append(tr_html)
        else:
            body_rows.append(tr_html)

    if not body_rows and len(header_rows) > 1:
        body_rows = header_rows[1:]
        header_rows = header_rows[:1]

    thead = f"<thead>{''.join(header_rows)}</thead>" if header_rows else ""
    tbody = f"<tbody>{''.join(body_rows)}</tbody>"
    cap_block = (
        f'<div class="paper-table-caption">Table {html.escape(num_str)}: {caption_html} '
        f'<a class="sec-anchor" href="#{anchor}" aria-label="Permalink">#</a></div>'
        if caption_html
        else ""
    )
    note_block = f'<div class="paper-table-note">{note_html}</div>' if note_html else ""
    return (
        f'<div class="paper-table-figure" id="{anchor}">'
        f"{cap_block}"
        f'<div class="paper-table-wrap"><table class="paper-table">{thead}{tbody}</table></div>'
        f"{note_block}"
        f"</div>"
    )


def render_alg_statement(stmt: str, ctx: CompilerContext) -> str:
    """Render an algorithmic statement or trailing inline clause (`\\Return ...`)."""

    s = stmt.lstrip("\\ ").strip()
    if s.startswith("\\Return"):
        return f'<span class="paper-alg-kw">return</span> {render_inline_latex(s[7:].strip(), ctx)}'
    return render_inline_latex(s, ctx)


def render_algorithmic(alg_inner: str, ctx: CompilerContext) -> str:
    """Render `\\begin{algorithmic} ... \\end{algorithmic}` into structured pseudocode."""

    directive_re = re.compile(
        r"^\\(?:Function|EndFunction|Procedure|EndProcedure|If|ElsIf|Else|EndIf|For|EndFor|While|EndWhile|State|Require|Ensure)\b"
    )
    logical_lines: list[str] = []
    for raw_line in alg_inner.splitlines():
        stripped = raw_line.strip()
        if not stripped:
            continue
        if directive_re.match(stripped) or not logical_lines:
            logical_lines.append(stripped)
        else:
            logical_lines[-1] = f"{logical_lines[-1]} {stripped}"

    lines_html: list[str] = []
    indent = 0
    lineno = 0
    for line in logical_lines:
        comment_html = ""
        c_idx = line.find("\\Comment{")
        if c_idx >= 0:
            cgrp = find_brace_group(line, c_idx + 8)
            if cgrp:
                comment_html = f'<span class="paper-alg-comment">// {render_inline_latex(cgrp[0], ctx)}</span>'
                line = (line[:c_idx] + line[cgrp[1] :]).strip()

        if line.startswith(("\\EndFunction", "\\EndProcedure", "\\EndIf", "\\EndFor", "\\EndWhile")):
            indent = max(0, indent - 1)
            continue

        lineno += 1
        pad = indent * 18
        body_out = ""

        if line.startswith(("\\Function{", "\\Procedure{")):
            kw = "function" if line.startswith("\\Function{") else "procedure"
            b_start = line.find("{")
            g1 = find_brace_group(line, b_start)
            g2 = find_brace_group(line, g1[1]) if g1 and g1[1] < len(line) else None
            if g1 and g2:
                body_out = (
                    f'<span class="paper-alg-kw">{kw}</span> '
                    f"<strong>{render_inline_latex(g1[0], ctx)}</strong>"
                    f"({render_inline_latex(g2[0], ctx)})"
                )
            indent += 1
        elif re.match(r"^\\If\s*\{", line):
            b_start = line.find("{")
            grp = find_brace_group(line, b_start)
            if grp:
                tail = line[grp[1] :].strip()
                tail_html = f" {render_alg_statement(tail, ctx)}" if tail else ""
                body_out = f'<span class="paper-alg-kw">if</span> {render_inline_latex(grp[0], ctx)} <span class="paper-alg-kw">then</span>{tail_html}'
            indent += 1
        elif re.match(r"^\\ElsIf\s*\{", line):
            indent = max(0, indent - 1)
            pad = indent * 18
            b_start = line.find("{")
            grp = find_brace_group(line, b_start)
            if grp:
                tail = line[grp[1] :].strip()
                tail_html = f" {render_alg_statement(tail, ctx)}" if tail else ""
                body_out = f'<span class="paper-alg-kw">else if</span> {render_inline_latex(grp[0], ctx)} <span class="paper-alg-kw">then</span>{tail_html}'
            indent += 1
        elif line.startswith("\\Else"):
            indent = max(0, indent - 1)
            pad = indent * 18
            rest = line[5:].strip()
            tail_html = f" {render_alg_statement(rest, ctx)}" if rest else ""
            body_out = f'<span class="paper-alg-kw">else</span>{tail_html}'
            indent += 1
        elif line.startswith("\\State"):
            body_out = render_alg_statement(line[6:], ctx)
        else:
            body_out = render_alg_statement(line, ctx)

        lines_html.append(
            f'<li class="paper-alg-line">'
            f'<span class="paper-alg-code"><span class="paper-alg-lineno">{lineno}</span>'
            f'<span style="padding-left:{pad}px">{body_out}</span></span>'
            f"{comment_html}</li>"
        )
    return f'<ol class="paper-alg-lines">{"".join(lines_html)}</ol>'


def render_algorithm_block(body: str, num_str: str, anchor_override: str | None, ctx: CompilerContext) -> str:
    """Render `\\begin{algorithm}` block."""

    lbl, body = extract_label(body)
    anchor = anchor_override or (sanitize_anchor(lbl) if lbl else f"alg-{num_str}")
    caption_html = ""
    cap_idx = body.find("\\caption{")
    if cap_idx >= 0:
        cap_grp = find_brace_group(body, cap_idx + 8)
        if cap_grp:
            cap_raw, cap_end = cap_grp
            _, cap_raw = extract_label(cap_raw)
            caption_html = render_inline_latex(cap_raw, ctx)
            body = body[:cap_idx] + body[cap_end:]

    alg_m = re.search(r"\\begin\{algorithmic\}(?:\[[^\]]*\])?(.*?)\\end\{algorithmic\}", body, re.S)
    code_html = render_algorithmic(alg_m.group(1), ctx) if alg_m else ""
    head = (
        f'<div class="paper-algorithm-head">Algorithm {html.escape(num_str)}: {caption_html} '
        f'<a class="sec-anchor" href="#{anchor}" aria-label="Permalink">#</a></div>'
    )
    return f'<div class="paper-algorithm" id="{anchor}">{head}{code_html}</div>'


def render_pgfplots_axis_svg(axis_tex: str) -> str:
    """Compile a `\\begin{axis}[...] ... \\end{axis}` block directly into a crisp vector `<svg>`."""

    opt_end = axis_tex.find("]")
    opts_str = axis_tex[axis_tex.find("[") + 1 : opt_end] if "[" in axis_tex and opt_end > 0 else ""
    content = axis_tex[opt_end + 1 :] if opt_end > 0 else axis_tex

    x_log = "xmode=log" in opts_str
    y_log = "ymode=log" in opts_str

    def find_opt_brace(key: str) -> str:
        m = re.search(rf"{key}\s*=\s*\{{", opts_str)
        if not m:
            return ""
        grp = find_brace_group(opts_str, m.end() - 1)
        return grp[0] if grp else ""

    def clean_svg_label(s: str) -> str:
        s = re.sub(r"\\(?:code|texttt|text|mathcal|mathrm|mathsf)\{([^}]*)\}", r"\1", s)
        s = re.sub(r"\\tfrac\{1\}\{2\}", "0.5 ", s)
        s = s.replace("\\log_2", "log₂").replace("\\,", " ").replace("\\%", "%").replace("\\_", "_").replace("$", "")
        s = s.replace("{", "").replace("}", "")
        return " ".join(s.split())

    xlabel = clean_svg_label(find_opt_brace("xlabel"))
    ylabel = clean_svg_label(find_opt_brace("ylabel"))
    xtick_raw = find_opt_brace("xtick")
    xticks = [float(v.strip()) for v in xtick_raw.split(",") if v.strip()] if xtick_raw else []

    ymin_m = re.search(r"ymin\s*=\s*([-0-9.]+)", opts_str)
    ymax_m = re.search(r"ymax\s*=\s*([-0-9.]+)", opts_str)

    series_list: list[tuple[str, list[tuple[float, float]], str]] = []
    plot_re = re.compile(r"\\addplot\s*\[([^\]]*)\]\s*coordinates\s*\{([^}]*)\}\s*;")
    legends = [
        clean_svg_label(find_brace_group(content, m.end() - 1)[0])
        for m in re.finditer(r"\\addlegendentry\s*\{", content)
        if find_brace_group(content, m.end() - 1)
    ]

    for idx, pm in enumerate(plot_re.finditer(content)):
        style_str = pm.group(1)
        coords_str = pm.group(2)
        pts = [
            (float(xm), float(ym))
            for xm, ym in re.findall(r"\(\s*([-0-9.]+)\s*,\s*([-0-9.]+)\s*\)", coords_str)
        ]
        label = legends[idx] if idx < len(legends) else f"Series {idx + 1}"
        series_list.append((style_str, pts, label))

    if not series_list:
        return ""

    all_x = [p[0] for _, pts, _ in series_list for p in pts]
    all_y = [p[1] for _, pts, _ in series_list for p in pts]
    xmin = min(all_x)
    xmax = max(all_x)
    ymin = float(ymin_m.group(1)) if ymin_m else min(all_y)
    ymax = float(ymax_m.group(1)) if ymax_m else max(all_y) * 1.08
    if y_log and ymin <= 0:
        ymin = max(0.1, min(all_y) * 0.8)

    def tx_val(x: float) -> float:
        return math.log2(max(x, 1e-6)) if x_log else x

    def ty_val(y: float) -> float:
        return math.log10(max(y, 1e-6)) if y_log else y

    w, h = 430, 255
    pad_l, pad_r, pad_t, pad_b = 54, 18, 18, 44
    pw = w - pad_l - pad_r
    ph = h - pad_t - pad_b

    tx0, tx1 = tx_val(xmin), tx_val(xmax)
    ty0, ty1 = ty_val(ymin), ty_val(ymax)

    def map_x(x: float) -> float:
        return pad_l + ((tx_val(x) - tx0) / max(tx1 - tx0, 1e-9)) * pw

    def map_y(y: float) -> float:
        return pad_t + ph - ((ty_val(y) - ty0) / max(ty1 - ty0, 1e-9)) * ph

    svg: list[str] = [
        f'<svg viewBox="0 0 {w} {h}" width="{w}" height="{h}" role="img" aria-label="{html.escape(xlabel)} vs {html.escape(ylabel)}">',
        '<rect x="0" y="0" width="100%" height="100%" fill="#fffdf9" rx="6"/>',
        f'<rect x="{pad_l}" y="{pad_t}" width="{pw}" height="{ph}" fill="#ffffff" stroke="#d7c6aa" stroke-width="1"/>',
    ]

    # Grid & X ticks
    tick_xs = xticks if xticks else sorted(set(all_x))[:8]
    for xv in tick_xs:
        if xmin <= xv <= xmax:
            px = map_x(xv)
            svg.append(f'<line x1="{px:.1f}" y1="{pad_t}" x2="{px:.1f}" y2="{pad_t + ph}" stroke="#f0e6d5" stroke-width="1"/>')
            lbl = str(int(xv)) if xv == int(xv) else f"{xv:g}"
            svg.append(
                f'<text x="{px:.1f}" y="{pad_t + ph + 15}" text-anchor="middle" font-size="10" fill="#736453" font-family="ui-monospace, monospace">{html.escape(lbl)}</text>'
            )

    # Y ticks
    if y_log:
        y_ticks = [v for v in (2, 5, 10, 20, 50, 100, 200) if ymin <= v <= ymax]
    else:
        step = (ymax - ymin) / 4.0
        y_ticks = [ymin + i * step for i in range(5)]
    for yv in y_ticks:
        py = map_y(yv)
        if pad_t <= py <= pad_t + ph:
            svg.append(f'<line x1="{pad_l}" y1="{py:.1f}" x2="{pad_l + pw}" y2="{py:.1f}" stroke="#f0e6d5" stroke-width="1"/>')
            lbl = f"{yv:.0f}" if abs(yv) >= 10 or yv == int(yv) else f"{yv:.1f}"
            svg.append(
                f'<text x="{pad_l - 6}" y="{py + 3.5:.1f}" text-anchor="end" font-size="10" fill="#736453" font-family="ui-monospace, monospace">{html.escape(lbl)}</text>'
            )

    palette = ["#1d4ed8", "#b42318", "#344054", "#6b7280"]
    for idx, (style_str, pts, _) in enumerate(series_list):
        color = "#101828"
        if "blue" in style_str:
            color = "#1d4ed8"
        elif "red" in style_str:
            color = "#b42318"
        elif "gray" in style_str:
            color = "#6b7280"
        elif idx < len(palette):
            color = palette[idx]

        dash = ""
        if "dashdotted" in style_str:
            dash = ' stroke-dasharray="6,3,2,3"'
        elif "dashed" in style_str:
            dash = ' stroke-dasharray="5,4"'
        elif "dotted" in style_str:
            dash = ' stroke-dasharray="2,3"'

        poly_pts = " ".join(f"{map_x(x):.1f},{map_y(y):.1f}" for x, y in pts)
        svg.append(f'<polyline fill="none" stroke="{color}" stroke-width="2"{dash} points="{poly_pts}"/>')
        if "mark=*" in style_str or "mark=square*" in style_str or "mark=triangle*" in style_str:
            for x, y in pts:
                svg.append(f'<circle cx="{map_x(x):.1f}" cy="{map_y(y):.1f}" r="2.6" fill="{color}"/>')

    # Axis titles
    svg.append(
        f'<text x="{pad_l + pw / 2:.1f}" y="{h - 8}" text-anchor="middle" font-size="11" font-weight="600" fill="#101828">{html.escape(xlabel)}</text>'
    )
    svg.append(
        f'<text x="14" y="{pad_t + ph / 2:.1f}" text-anchor="middle" font-size="11" font-weight="600" fill="#101828" transform="rotate(-90 14 {pad_t + ph / 2:.1f})">{html.escape(ylabel)}</text>'
    )

    # Compact legend box
    leg_x = pad_l + 10
    leg_y = pad_t + 10
    leg_h = len(series_list) * 15 + 8
    svg.append(
        f'<rect x="{leg_x}" y="{leg_y}" width="195" height="{leg_h}" rx="4" fill="#fffaf0" fill-opacity="0.92" stroke="#dfd0b7" stroke-width="0.8"/>'
    )
    for idx, (style_str, _, label) in enumerate(series_list):
        color = "#101828"
        if "blue" in style_str:
            color = "#1d4ed8"
        elif "red" in style_str:
            color = "#b42318"
        elif "gray" in style_str:
            color = "#6b7280"
        dash = ""
        if "dashdotted" in style_str:
            dash = ' stroke-dasharray="5,2,1.5,2"'
        elif "dashed" in style_str:
            dash = ' stroke-dasharray="4,3"'
        elif "dotted" in style_str:
            dash = ' stroke-dasharray="2,2"'
        ly = leg_y + 13 + idx * 15
        svg.append(f'<line x1="{leg_x + 7}" y1="{ly - 3}" x2="{leg_x + 25}" y2="{ly - 3}" stroke="{color}" stroke-width="2"{dash}/>')
        svg.append(f'<text x="{leg_x + 31}" y="{ly}" font-size="9.5" fill="#253041">{html.escape(label)}</text>')

    svg.append("</svg>")
    return "".join(svg)


def render_diagram_svg(label: str) -> str:
    """Return crisp vector `<svg>` diagrams for the 7 structural TikZ figures across both papers."""

    if label == "layers":
        names = [
            "6. Application Layer",
            "5. Protocol Layer",
            "4. Extension Runtime Layer",
            "3. Overlay Layer (PeerRing)",
            "2. Transport Layer (WebRTC / TURN)",
            "1. Identity Layer (DID / Delegation)",
        ]
        boxes = "".join(
            f'<rect x="70" y="{14 + i * 36}" width="320" height="28" rx="5" fill="#fffaf0" stroke="#101828" stroke-width="1.3"/>'
            f'<text x="230" y="{32 + i * 36}" text-anchor="middle" font-size="12" font-weight="700" fill="#101828">{n}</text>'
            for i, n in enumerate(names)
        )
        return f'<svg viewBox="0 0 460 236" width="460" height="236" role="img" aria-label="Six architectural layers of Rings Network">{boxes}</svg>'

    if label in ("webrtc-standard-sdp", "webrtc-rings-sdp"):
        is_rings = label == "webrtc-rings-sdp"
        steps = (
            [
                (True, "Offer + Gathered ICE"),
                (False, "Answer + Gathered ICE"),
                ("bi", "Overlay 1-RTT, then Direct DTLS / SCTP"),
            ]
            if is_rings
            else [
                (True, "SDP Offer"),
                (False, "SDP Answer"),
                (True, "Trickle ICE Candidates"),
                (False, "Trickle ICE Candidates"),
                ("bi", "DTLS / SCTP Open"),
            ]
        )
        h = 74 + len(steps) * 38
        lines = [
            f'<svg viewBox="0 0 440 {h}" width="440" height="{h}" role="img" aria-label="SDP signaling sequence diagram">',
            '<defs><marker id="arr" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse"><path d="M 0 1 L 10 5 L 0 9 z" fill="#101828"/></marker></defs>',
            '<rect x="50" y="12" width="90" height="28" rx="5" fill="#fffaf0" stroke="#101828" stroke-width="1.3"/>',
            '<text x="95" y="30" text-anchor="middle" font-size="12" font-weight="700" fill="#101828">Node 1</text>',
            '<rect x="300" y="12" width="90" height="28" rx="5" fill="#fffaf0" stroke="#101828" stroke-width="1.3"/>',
            '<text x="345" y="30" text-anchor="middle" font-size="12" font-weight="700" fill="#101828">Node 2</text>',
            f'<line x1="95" y1="40" x2="95" y2="{h - 14}" stroke="#344054" stroke-width="1.4"/>',
            f'<line x1="345" y1="40" x2="345" y2="{h - 14}" stroke="#344054" stroke-width="1.4"/>',
        ]
        for i, (direction, text_lbl) in enumerate(steps):
            y = 70 + i * 36
            if direction == "bi":
                lines.append(
                    f'<line x1="102" y1="{y}" x2="338" y2="{y}" stroke="#b42318" stroke-width="1.6" stroke-dasharray="5,3" marker-start="url(#arr)" marker-end="url(#arr)"/>'
                )
            elif direction:
                lines.append(f'<line x1="95" y1="{y}" x2="338" y2="{y}" stroke="#101828" stroke-width="1.4" marker-end="url(#arr)"/>')
            else:
                lines.append(f'<line x1="345" y1="{y}" x2="102" y2="{y}" stroke="#101828" stroke-width="1.4" marker-end="url(#arr)"/>')
            lines.append(f'<text x="220" y="{y - 6}" text-anchor="middle" font-size="11" fill="#253041">{html.escape(text_lbl)}</text>')
        lines.append("</svg>")
        return "".join(lines)

    if label == "wasm-boundary":
        cols = [(75, "Browser Host", "(WebRTC / IDB)"), (250, "JS FFI Bridge", "(cap-checked Iₓ)"), (425, "Wasm Core", "(rings-core step)")]
        msgs = [
            (75, 250, "Load & init"),
            (250, 425, "Instantiate"),
            (75, 250, "Net / timer event"),
            (250, 425, "step(S, i)"),
            (425, 250, "Effects Aₓ*"),
            (250, 75, "Permitted IO"),
        ]
        out = [
            '<svg viewBox="0 0 500 265" width="500" height="265" role="img" aria-label="Wasm runtime confinement boundary">',
            '<defs><marker id="warr" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto"><path d="M 0 1 L 10 5 L 0 9 z" fill="#101828"/></marker></defs>',
        ]
        for cx, t1, t2 in cols:
            out.append(f'<rect x="{cx - 64}" y="10" width="128" height="38" rx="5" fill="#fffaf0" stroke="#101828" stroke-width="1.3"/>')
            out.append(f'<text x="{cx}" y="26" text-anchor="middle" font-size="11" font-weight="700" fill="#101828">{t1}</text>')
            out.append(f'<text x="{cx}" y="40" text-anchor="middle" font-size="9.5" fill="#736453">{t2}</text>')
            out.append(f'<line x1="{cx}" y1="48" x2="{cx}" y2="250" stroke="#736453" stroke-width="1.2"/>')
        for idx, (x1, x2, lbl) in enumerate(msgs):
            y = 74 + idx * 30
            dx = -6 if x2 > x1 else 6
            out.append(f'<line x1="{x1}" y1="{y}" x2="{x2 + dx}" y2="{y}" stroke="#101828" stroke-width="1.3" marker-end="url(#warr)"/>')
            out.append(f'<text x="{(x1 + x2) / 2}" y="{y - 5}" text-anchor="middle" font-size="10.5" fill="#253041">{html.escape(lbl)}</text>')
        out.append("</svg>")
        return "".join(out)

    if label in ("dht", "lookup-figure"):
        cx, cy, r = 150, 150, 112
        pts = [
            (
                cx + r * math.cos(math.radians(i * 22.5 - 90)),
                cy + r * math.sin(math.radians(i * 22.5 - 90)),
            )
            for i in range(16)
        ]
        out = [
            f'<svg viewBox="0 0 300 300" width="290" height="290" role="img" aria-label="16-node Chord ring ({label})">',
            f'<circle cx="{cx}" cy="{cy}" r="{r}" fill="none" stroke="#101828" stroke-width="1.5"/>',
        ]
        if label == "dht":
            for i in range(16):
                for j in (1, 2, 3, 4):
                    dst = (i + (1 << j)) % 16
                    out.append(
                        f'<line x1="{pts[i][0]:.1f}" y1="{pts[i][1]:.1f}" x2="{pts[dst][0]:.1f}" y2="{pts[dst][1]:.1f}" stroke="#736453" stroke-opacity="0.45" stroke-width="0.9"/>'
                    )
        else:
            for dst in (2, 3, 5, 9):
                x1, y1 = pts[1]
                x2, y2 = pts[dst]
                mx, my = (x1 + x2) / 2 * 0.85 + cx * 0.15, (y1 + y2) / 2 * 0.85 + cy * 0.15
                out.append(
                    f'<path d="M {x1:.1f} {y1:.1f} Q {mx:.1f} {my:.1f} {x2:.1f} {y2:.1f}" fill="none" stroke="#b42318" stroke-width="1.8"/>'
                )
        for i, (px, py) in enumerate(pts):
            fill = "#b42318" if (label == "lookup-figure" and i == 1) else "#fffaf0"
            tcol = "#ffffff" if fill == "#b42318" else "#101828"
            out.append(f'<circle cx="{px:.1f}" cy="{py:.1f}" r="11" fill="{fill}" stroke="#101828" stroke-width="1.2"/>')
            out.append(f'<text x="{px:.1f}" y="{py + 3.5:.1f}" text-anchor="middle" font-size="9.5" font-weight="700" fill="{tcol}">{i}</text>')
        out.append("</svg>")
        return "".join(out)

    if label == "fig:interleaved-ring":
        cx, cy, r = 160, 145, 94
        nodes = [
            ("A₁₀", -144, "#f3ead8", False),
            ("B₃₀", -54, "#f3ead8", False),
            ("E₄₀", -6, "#ffffff", True),
            ("C₅₀", 42, "#dfd0b7", False),
            ("D₈₀", 142, "#ffffff", True),
        ]
        coords = {
            name: (cx + r * math.cos(math.radians(ang)), cy + r * math.sin(math.radians(ang)))
            for name, ang, _, _ in nodes
        }
        out = [
            '<svg viewBox="0 0 320 290" width="320" height="290" role="img" aria-label="Interleaved non-transitive WebRTC cliques sharing bridge C">',
            f'<circle cx="{cx}" cy="{cy}" r="{r + 22}" fill="none" stroke="#98a2b3" stroke-dasharray="3,3" stroke-width="1.1"/>',
        ]
        for u, v in (("A₁₀", "B₃₀"), ("B₃₀", "C₅₀"), ("A₁₀", "C₅₀")):
            out.append(
                f'<line x1="{coords[u][0]:.1f}" y1="{coords[u][1]:.1f}" x2="{coords[v][0]:.1f}" y2="{coords[v][1]:.1f}" stroke="#101828" stroke-width="1.6"/>'
            )
        for u, v in (("C₅₀", "D₈₀"), ("D₈₀", "E₄₀"), ("E₄₀", "C₅₀")):
            out.append(
                f'<line x1="{coords[u][0]:.1f}" y1="{coords[u][1]:.1f}" x2="{coords[v][0]:.1f}" y2="{coords[v][1]:.1f}" stroke="#b42318" stroke-dasharray="5,3" stroke-width="1.6"/>'
            )
        for name, _, fill, dashed in nodes:
            px, py = coords[name]
            d_attr = ' stroke-dasharray="3,2"' if dashed else ""
            sw = "2.2" if name == "C₅₀" else "1.4"
            out.append(f'<circle cx="{px:.1f}" cy="{py:.1f}" r="18" fill="{fill}" stroke="#101828" stroke-width="{sw}"{d_attr}/>')
            out.append(f'<text x="{px:.1f}" y="{py + 4:.1f}" text-anchor="middle" font-size="11" font-weight="700" fill="#101828">{name}</text>')
        out.append("</svg>")
        return "".join(out)

    if label == "fig:cycle":
        return (
            '<svg viewBox="0 0 460 150" width="460" height="150" role="img" aria-label="DRanking epoch cycle">'
            '<defs><marker id="carr" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto"><path d="M 0 1 L 10 5 L 0 9 z" fill="#101828"/></marker></defs>'
            '<rect x="20" y="20" width="76" height="30" rx="5" fill="#fffaf0" stroke="#101828"/><text x="58" y="39" text-anchor="middle" font-size="12" font-weight="700" fill="#101828">W<tspan dy="3" font-size="8.5">k</tspan></text>'
            '<rect x="135" y="20" width="76" height="30" rx="5" fill="#fffaf0" stroke="#101828"/><text x="173" y="39" text-anchor="middle" font-size="12" font-weight="700" fill="#101828">F</text>'
            '<rect x="250" y="20" width="76" height="30" rx="5" fill="#fffaf0" stroke="#101828"/><text x="288" y="39" text-anchor="middle" font-size="12" font-weight="700" fill="#101828">t<tspan dy="3" font-size="8.5">k+1</tspan></text>'
            '<rect x="365" y="20" width="76" height="30" rx="5" fill="#fffaf0" stroke="#101828"/><text x="403" y="39" text-anchor="middle" font-size="12" font-weight="700" fill="#101828">Σ<tspan dy="3" font-size="8.5">k+1</tspan></text>'
            '<rect x="20" y="92" width="76" height="30" rx="5" fill="#fff8ee" stroke="#b42318"/><text x="58" y="111" text-anchor="middle" font-size="12" font-weight="700" fill="#101828">B<tspan dy="3" font-size="8.5">k+1</tspan></text>'
            '<rect x="135" y="92" width="76" height="30" rx="5" fill="#fff8ee" stroke="#b42318"/><text x="173" y="111" text-anchor="middle" font-size="12" font-weight="700" fill="#101828">sortition</text>'
            '<rect x="250" y="92" width="76" height="30" rx="5" fill="#fff8ee" stroke="#b42318"/><text x="288" y="111" text-anchor="middle" font-size="12" font-weight="700" fill="#101828">committee</text>'
            '<rect x="365" y="92" width="76" height="30" rx="5" fill="#fff8ee" stroke="#b42318"/><text x="403" y="111" text-anchor="middle" font-size="12" font-weight="700" fill="#101828">QC<tspan dy="3" font-size="8.5">2/3</tspan></text>'
            '<line x1="96" y1="35" x2="133" y2="35" stroke="#101828" stroke-width="1.3" marker-end="url(#carr)"/>'
            '<line x1="211" y1="35" x2="248" y2="35" stroke="#101828" stroke-width="1.3" marker-end="url(#carr)"/>'
            '<line x1="326" y1="35" x2="363" y2="35" stroke="#101828" stroke-width="1.3" marker-end="url(#carr)"/>'
            '<line x1="403" y1="50" x2="403" y2="90" stroke="#101828" stroke-width="1.3" marker-end="url(#carr)"/>'
            '<line x1="96" y1="107" x2="133" y2="107" stroke="#b42318" stroke-width="1.3" marker-end="url(#carr)"/>'
            '<line x1="211" y1="107" x2="248" y2="107" stroke="#b42318" stroke-width="1.3" marker-end="url(#carr)"/>'
            '<line x1="326" y1="107" x2="363" y2="107" stroke="#b42318" stroke-width="1.3" marker-end="url(#carr)"/>'
            '<line x1="268" y1="50" x2="195" y2="90" stroke="#736453" stroke-width="1.2" marker-end="url(#carr)"/>'
            '<path d="M 403 122 L 403 138 L 58 138 L 58 124" fill="none" stroke="#b42318" stroke-width="1.2" marker-end="url(#carr)"/>'
            "</svg>"
        )

    return ""


def render_figure_block(body: str, num_str: str, anchor_override: str | None, ctx: CompilerContext) -> str:
    """Render `\\begin{figure}` / `\\begin{figure*}` with inline vector `<svg>` plots or diagrams."""

    lbl, body = extract_label(body)
    anchor = anchor_override or (sanitize_anchor(lbl) if lbl else f"fig-{num_str}")

    caption_html = ""
    cap_idx = body.find("\\caption{")
    if cap_idx >= 0:
        cap_grp = find_brace_group(body, cap_idx + 8)
        if cap_grp:
            cap_raw, cap_end = cap_grp
            lbl2, cap_raw = extract_label(cap_raw)
            if lbl2 and not lbl:
                lbl = lbl2
                anchor = sanitize_anchor(lbl2)
            caption_html = render_inline_latex(cap_raw, ctx)
            body = body[:cap_idx] + body[cap_end:]

    axes = re.findall(r"\\begin\{axis\}(.*?)\\end\{axis\}", body, re.S)
    visual_html = ""
    if axes:
        plots = [render_pgfplots_axis_svg(ax) for ax in axes]
        visual_html = (
            f'<div class="paper-figure-grid">{"".join(plots)}</div>'
            if len(plots) > 1
            else plots[0]
        )
    elif lbl:
        visual_html = render_diagram_svg(lbl)

    cap_el = (
        f"<figcaption><strong>Figure {html.escape(num_str)}:</strong> {caption_html} "
        f'<a class="sec-anchor" href="#{anchor}" aria-label="Permalink">#</a></figcaption>'
        if caption_html
        else ""
    )
    return f'<figure class="paper-figure" id="{anchor}">{visual_html}{cap_el}</figure>'


def render_list_env(body: str, ordered: bool, ctx: CompilerContext) -> str:
    """Render `itemize` or `enumerate` environments (supporting nested content and display math)."""

    if body.lstrip().startswith("["):
        brk = find_bracket_group(body.lstrip(), 0)
        if brk:
            body = body.lstrip()[brk[1] :]
    parts = split_top_level(body, "\\item")
    items: list[str] = []
    for part in parts[1:]:
        p = part.strip()
        if not p:
            continue
        custom_lbl = ""
        if p.startswith("["):
            brk = find_bracket_group(p, 0)
            if brk:
                custom_lbl = f"<strong>{render_inline_latex(brk[0], ctx)}</strong> "
                p = p[brk[1] :].strip()
        items.append(f"<li>{custom_lbl}{render_blocks(p, ctx, inside_block=True)}</li>")
    tag = "ol" if ordered else "ul"
    return f"<{tag}>{''.join(items)}</{tag}>"


def find_matching_end(text: str, env_name: str, start_after_begin: int) -> int:
    """Find the matching `\\end{env_name}` handling nested environments of the same name."""

    begin_tok = f"\\begin{{{env_name}}}"
    end_tok = f"\\end{{{env_name}}}"
    depth = 1
    pos = start_after_begin
    while depth > 0:
        b_idx = text.find(begin_tok, pos)
        e_idx = text.find(end_tok, pos)
        if e_idx < 0:
            return len(text)
        if 0 <= b_idx < e_idx:
            depth += 1
            pos = b_idx + len(begin_tok)
        else:
            depth -= 1
            if depth == 0:
                return e_idx
            pos = e_idx + len(end_tok)
    return len(text)


def collect_labels_pass1(doc_body: str, ctx: CompilerContext) -> None:
    """First pass over document body to assign exact numbers to sections, theorems, tables, figures, algorithms, and equations."""

    sec_num = 0
    subsec_num = 0
    theorem_counters: dict[str, int] = {}
    tab_num = 0
    fig_num = 0
    alg_num = 0
    eq_num = 0

    token_re = re.compile(
        r"\\(section|subsection)\*?\{|\\begin\{(definition|constraint|assumption|proposition|theorem|lemma|corollary|law|invariant|obligation|remark|table\*?|figure\*?|algorithm|equation)\}"
    )
    pos = 0
    while True:
        m = token_re.search(doc_body, pos)
        if not m:
            break
        if m.group(1):
            kind = m.group(1)
            starred = doc_body[m.end() - 2] == "*"
            grp = find_brace_group(doc_body, m.end() - 1)
            if not grp:
                pos = m.end()
                continue
            pos = grp[1]
            tail = doc_body[pos : pos + 140]
            lm = re.match(r"\s*\\label\{([^}]+)\}", tail)
            if not starred:
                if kind == "section":
                    sec_num += 1
                    subsec_num = 0
                    disp = ROMAN_NUMERALS[sec_num] if ctx.spec.roman_sections and sec_num < len(ROMAN_NUMERALS) else str(sec_num)
                else:
                    subsec_num += 1
                    sec_disp = ROMAN_NUMERALS[sec_num] if ctx.spec.roman_sections and sec_num < len(ROMAN_NUMERALS) else str(sec_num)
                    disp = f"{sec_disp}.{subsec_num}" if not ctx.spec.roman_sections else f"{sec_disp}-{chr(64 + subsec_num)}"
                if lm:
                    lbl = lm.group(1).strip()
                    ctx.labels[lbl] = ("Section", disp, sanitize_anchor(lbl))
        else:
            env = m.group(2)
            end_idx = find_matching_end(doc_body, env, m.end())
            inner = doc_body[m.end() : end_idx]
            pos = end_idx + len(f"\\end{{{env}}}")
            base_env = env.rstrip("*")
            if base_env in THEOREM_ENVS:
                theorem_counters[base_env] = theorem_counters.get(base_env, 0) + 1
                disp = str(theorem_counters[base_env])
                # Look only at top of environment for its own label
                lm = re.search(r"\\label\{([^}]+)\}", inner[:220])
                if lm:
                    lbl = lm.group(1).strip()
                    ctx.labels[lbl] = (THEOREM_ENVS[base_env][0], disp, sanitize_anchor(lbl))
            elif base_env == "table":
                tab_num += 1
                for lm in re.finditer(r"\\label\{([^}]+)\}", inner):
                    lbl = lm.group(1).strip()
                    ctx.labels[lbl] = ("Table", str(tab_num), sanitize_anchor(lbl))
            elif base_env == "figure":
                fig_num += 1
                for lm in re.finditer(r"\\label\{([^}]+)\}", inner):
                    lbl = lm.group(1).strip()
                    ctx.labels[lbl] = ("Figure", str(fig_num), sanitize_anchor(lbl))
            elif base_env == "algorithm":
                alg_num += 1
                for lm in re.finditer(r"\\label\{([^}]+)\}", inner):
                    lbl = lm.group(1).strip()
                    ctx.labels[lbl] = ("Algorithm", str(alg_num), sanitize_anchor(lbl))
            elif base_env == "equation":
                eq_num += 1
                lm = re.search(r"\\label\{([^}]+)\}", inner)
                if lm:
                    lbl = lm.group(1).strip()
                    ctx.labels[lbl] = ("Equation", str(eq_num), sanitize_anchor(lbl))


@dataclass
class BlockCounters:
    """Mutable counters shared across top-level document rendering."""

    sec: int = 0
    subsec: int = 0
    theorems: dict[str, int] = field(default_factory=dict)
    tab: int = 0
    fig: int = 0
    alg: int = 0
    eq: int = 0


def render_blocks(text: str, ctx: CompilerContext, counters: BlockCounters | None = None, inside_block: bool = False) -> str:
    """Render LaTeX block structure (sections, theorems, math, tables, figures, lists, paragraphs)."""

    if counters is None:
        counters = BlockCounters()

    chunks: list[str] = []
    para_buf: list[str] = []

    def flush_para() -> None:
        raw = " ".join(s.strip() for s in para_buf if s.strip()).strip()
        para_buf.clear()
        if not raw:
            return
        # Clean stray formatting switches
        raw = re.sub(r"\\(?:begingroup|endgroup|medskip|smallskip|bigskip|newpage|clearpage)\b", "", raw).strip()
        if not raw:
            return
        rendered = render_inline_latex(raw, ctx)
        if rendered:
            chunks.append(f"<p>{rendered}</p>")

    pos = 0
    n = len(text)
    block_Start_re = re.compile(
        r"(\\\[|\\(section|subsection|paragraph)\*?\{|\\begin\{(itemize|enumerate|center|definition|constraint|assumption|proposition|theorem|lemma|corollary|law|invariant|obligation|remark|proof|table\*?|figure\*?|algorithm|equation|align\*)\})"
    )

    while pos < n:
        m = block_Start_re.search(text, pos)
        if not m:
            rest = text[pos:]
            for p in re.split(r"\n\s*\n", rest):
                if p.strip():
                    para_buf.append(p)
                    flush_para()
            break

        before = text[pos : m.start()]
        if before:
            parts = re.split(r"\n\s*\n", before)
            for p in parts[:-1]:
                if p.strip():
                    para_buf.append(p)
                    flush_para()
            if parts[-1].strip():
                para_buf.append(parts[-1])

        tok = m.group(1)
        if tok == "\\[":
            flush_para()
            end_m = text.find("\\]", m.end())
            if end_m < 0:
                end_m = n
            math_raw = normalize_math_for_katex(text[m.end() : end_m], ctx)
            chunks.append(f'<div class="paper-math-display">\\[{html.escape(math_raw)}\\]</div>')
            pos = end_m + 2
            continue

        if m.group(2):
            flush_para()
            sec_kind = m.group(2)
            starred = text[m.end() - 2] == "*"
            grp = find_brace_group(text, m.end() - 1)
            if not grp:
                pos = m.end()
                continue
            sec_title_raw, pos = grp
            lbl = None
            lm = re.match(r"\s*\\label\{([^}]+)\}", text[pos:])
            if lm:
                lbl = lm.group(1).strip()
                pos += lm.end()

            title_html = render_inline_latex(sec_title_raw, ctx)
            if sec_kind == "paragraph":
                chunks.append(f"<h4>{title_html}</h4>")
                continue

            if sec_kind == "section":
                if not starred:
                    counters.sec += 1
                    counters.subsec = 0
                    num_disp = (
                        ROMAN_NUMERALS[counters.sec]
                        if ctx.spec.roman_sections and counters.sec < len(ROMAN_NUMERALS)
                        else str(counters.sec)
                    )
                else:
                    num_disp = ""
                anchor = sanitize_anchor(lbl) if lbl else re.sub(r"[^a-z0-9]+", "-", sec_title_raw.lower()).strip("-") or f"sec-{counters.sec}"
                num_span = f'<span class="sec-num">{num_disp}.</span>' if num_disp else ""
                chunks.append(
                    f'<h2 id="{anchor}">{num_span}{title_html}'
                    f'<a class="sec-anchor" href="#{anchor}" aria-label="Section link">#</a></h2>'
                )
                if not inside_block:
                    ctx.toc.append(TocItem(level=2, number=num_disp, title_html=title_html, anchor=anchor))
            else:
                if not starred:
                    counters.subsec += 1
                    sec_disp = (
                        ROMAN_NUMERALS[counters.sec]
                        if ctx.spec.roman_sections and counters.sec < len(ROMAN_NUMERALS)
                        else str(counters.sec)
                    )
                    num_disp = (
                        f"{sec_disp}-{chr(64 + counters.subsec)}"
                        if ctx.spec.roman_sections
                        else f"{sec_disp}.{counters.subsec}"
                    )
                else:
                    num_disp = ""
                anchor = sanitize_anchor(lbl) if lbl else re.sub(r"[^a-z0-9]+", "-", sec_title_raw.lower()).strip("-") or f"subsec-{counters.sec}-{counters.subsec}"
                num_span = f'<span class="sec-num">{num_disp}</span>' if num_disp else ""
                chunks.append(
                    f'<h3 id="{anchor}">{num_span}{title_html}'
                    f'<a class="sec-anchor" href="#{anchor}" aria-label="Subsection link">#</a></h3>'
                )
                if not inside_block:
                    ctx.toc.append(TocItem(level=3, number=num_disp, title_html=title_html, anchor=anchor))
            continue

        env = m.group(3)
        flush_para()
        end_idx = find_matching_end(text, env, m.end())
        inner = text[m.end() : end_idx]
        pos = end_idx + len(f"\\end{{{env}}}")
        base_env = env.rstrip("*")

        if base_env in ("itemize", "enumerate"):
            chunks.append(render_list_env(inner, ordered=(base_env == "enumerate"), ctx=ctx))
        elif base_env == "center":
            chunks.append(f'<div style="text-align:center;margin:0.85rem 0">{render_inline_latex(inner.strip(), ctx)}</div>')
        elif base_env == "equation":
            counters.eq += 1
            lbl, eq_body = extract_label(inner)
            anchor_id = sanitize_anchor(lbl) if lbl else f"eq-{counters.eq}"
            norm = normalize_math_for_katex(eq_body, ctx)
            chunks.append(
                f'<div class="paper-eq-row" id="{anchor_id}">'
                f'<div class="paper-math-display">\\[{html.escape(norm)}\\]</div>'
                f'<span class="paper-eq-num">({counters.eq})</span></div>'
            )
        elif env == "align*":
            norm = normalize_math_for_katex(f"\\begin{{aligned}}{inner}\\end{{aligned}}", ctx)
            chunks.append(f'<div class="paper-math-display">\\[{html.escape(norm)}\\]</div>')
        elif base_env == "table":
            counters.tab += 1
            chunks.append(render_table_block(inner, str(counters.tab), None, ctx))
        elif base_env == "figure":
            counters.fig += 1
            chunks.append(render_figure_block(inner, str(counters.fig), None, ctx))
        elif base_env == "algorithm":
            counters.alg += 1
            chunks.append(render_algorithm_block(inner, str(counters.alg), None, ctx))
        elif base_env == "proof":
            opt_title = "Proof"
            inner_str = inner.lstrip()
            if inner_str.startswith("["):
                brk = find_bracket_group(inner_str, 0)
                if brk:
                    opt_title = render_inline_latex(brk[0], ctx)
                    inner_str = inner_str[brk[1] :]
            inner_str = inner_str.replace("\\qed", "")
            proof_body = render_blocks(inner_str, ctx, counters=counters, inside_block=True)
            chunks.append(
                f'<div class="paper-block paper-block-proof">'
                f'<div class="paper-block-head">{opt_title}.</div>'
                f'{proof_body}<span class="paper-qed" aria-hidden="true">∎</span><div style="clear:both"></div></div>'
            )
        elif base_env in THEOREM_ENVS:
            title_word, css_slug = THEOREM_ENVS[base_env]
            counters.theorems[base_env] = counters.theorems.get(base_env, 0) + 1
            tnum = counters.theorems[base_env]
            opt_name = ""
            inner_str = inner.lstrip()
            if inner_str.startswith("["):
                brk = find_bracket_group(inner_str, 0)
                if brk:
                    opt_name = f" ({render_inline_latex(brk[0], ctx)})"
                    inner_str = inner_str[brk[1] :]
            lbl = None
            lm = re.match(r"\s*\\label\{([^}]+)\}", inner_str)
            if lm:
                lbl = lm.group(1).strip()
                inner_str = inner_str[lm.end() :]
            anchor_id = sanitize_anchor(lbl) if lbl else f"{css_slug}-{tnum}"
            body_rendered = render_blocks(inner_str, ctx, counters=counters, inside_block=True)
            chunks.append(
                f'<section class="paper-block paper-block-{css_slug}" id="{anchor_id}">'
                f'<div class="paper-block-head">{title_word} {tnum}{opt_name}.'
                f'<a class="sec-anchor" href="#{anchor_id}" aria-label="Permalink">#</a></div>'
                f"{body_rendered}</section>"
            )

    flush_para()
    return "\n".join(chunks)


def render_bibliography_html(ctx: CompilerContext) -> str:
    """Render cited BibTeX entries in citation order."""

    if not ctx.cited_order:
        return ""
    items: list[str] = []
    for key in ctx.cited_order:
        num = ctx.cited_num[key]
        anchor_id = f"bib-{sanitize_anchor(key)}"
        entry = ctx.bib_db.get(key)
        if not entry:
            items.append(
                f'<li class="paper-bib-item" id="{anchor_id}"><span class="paper-bib-num">[{num}]</span>'
                f"<div><code>{html.escape(key)}</code></div></li>"
            )
            continue
        f = entry.fields
        author = clean_bib_text(f.get("author", ""))
        title = clean_bib_text(f.get("title", key))
        venue_parts: list[str] = []
        for vk in ("journal", "booktitle", "institution", "howpublished", "publisher"):
            if vk in f:
                venue_parts.append(clean_bib_text(f[vk]))
        if "volume" in f:
            vol = f"vol. {clean_bib_text(f['volume'])}"
            if "number" in f:
                vol += f", no. {clean_bib_text(f['number'])}"
            venue_parts.append(vol)
        if "pages" in f:
            venue_parts.append(f"pp. {clean_bib_text(f['pages'])}")
        if "year" in f:
            venue_parts.append(clean_bib_text(f["year"]))
        if "note" in f:
            venue_parts.append(clean_bib_text(f["note"]))
        if "doi" in f:
            doi = f["doi"].strip()
            venue_parts.append(
                f'DOI: <a href="https://doi.org/{html.escape(doi, quote=True)}" target="_blank" rel="noreferrer">{html.escape(doi)}</a>'
            )
        if "url" in f and "note" not in f:
            u = f["url"].strip()
            venue_parts.append(
                f'<a href="{html.escape(u, quote=True)}" target="_blank" rel="noreferrer">{html.escape(u)}</a>'
            )

        author_prefix = f"{author}, " if author else ""
        venue_suffix = f" <em>{', '.join(venue_parts)}</em>." if venue_parts else "."
        items.append(
            f'<li class="paper-bib-item" id="{anchor_id}">'
            f'<span class="paper-bib-num">[{num}]</span>'
            f'<div>{author_prefix}<span class="paper-bib-title">“{title},”</span>{venue_suffix}</div>'
            f"</li>"
        )
    ctx.toc.append(TocItem(level=2, number="", title_html="References", anchor="references"))
    return (
        '<h2 id="references">References<a class="sec-anchor" href="#references" aria-label="Section link">#</a></h2>\n'
        f'<ol class="paper-bib-list">{"".join(items)}</ol>'
    )


def html_to_plain_summary(rendered_html: str) -> str:
    """Convert rendered HTML containing `\\(...\\)` spans into clean plain text for SEO/JSON-LD/Markdown."""

    text = html.unescape(re.sub(r"<[^>]+>", "", rendered_html))

    def simplify_math(m: re.Match[str]) -> str:
        expr = m.group(1)
        expr = re.sub(r"\\href\{#[^}]*\}\{([^}]*)\}", r"\1", expr)
        expr = re.sub(r"\\(?:mathbb|mathsf|mathcal|mathbf|mathrm|mathit|mathtt|operatorname|text|textit|texttt)\{([^}]*)\}", r"\1", expr)
        expr = (
            expr.replace("\\times", "×")
            .replace("\\le", "≤")
            .replace("\\leq", "≤")
            .replace("\\ge", "≥")
            .replace("\\geq", "≥")
            .replace("\\cap", "∩")
            .replace("\\cup", "∪")
            .replace("\\circ", "∘")
            .replace("\\varnothing", "∅")
            .replace("\\alpha", "α")
            .replace("\\Omega", "Ω")
            .replace("\\Pi", "Π")
            .replace("\\,", "")
        )
        expr = re.sub(r"\\[A-Za-z]+", "", expr)
        expr = expr.replace("{", "").replace("}", "")
        return expr

    text = re.sub(r"\\\((.*?)\\\)", simplify_math, text, flags=re.S)
    text = re.sub(r"\\\[(.*?)\\\]", simplify_math, text, flags=re.S)
    return " ".join(text.split())


def compile_paper(spec: PaperSpec, papers_dir: Path = PAPERS_SOURCE_DIR) -> ParsedPaper:
    """Compile one `.tex` + `.bib` specification into a `ParsedPaper`."""

    tex_path = papers_dir / spec.tex_file
    bib_path = papers_dir / spec.bib_file
    pdf_path = papers_dir / spec.pdf_file
    raw_tex = strip_latex_comments(tex_path.read_text(encoding="utf-8"))
    bib_db = parse_bib_file(bib_path)
    ctx = CompilerContext(spec=spec, bib_db=bib_db)

    # Extract \title{...} and \author{...}
    title_raw = spec.short_title
    t_idx = raw_tex.find("\\title{")
    if t_idx >= 0:
        t_grp = find_brace_group(raw_tex, t_idx + 6)
        if t_grp:
            title_raw = re.sub(r"\\\\(?:\[[^\]]*\])?", " ", t_grp[0])
            title_raw = re.sub(r"\\large\b", "", title_raw)
    title_plain = " ".join(re.sub(r"\\[A-Za-z]+\s*", "", title_raw).replace("{", "").replace("}", "").split())

    author_plain = "Ryan J. Kung"
    thanks_html = ""
    a_idx = raw_tex.find("\\author{")
    if a_idx >= 0:
        a_grp = find_brace_group(raw_tex, a_idx + 7)
        if a_grp:
            a_inner = a_grp[0]
            th_idx = a_inner.find("\\thanks{")
            if th_idx >= 0:
                th_grp = find_brace_group(a_inner, th_idx + 7)
                if th_grp:
                    thanks_html = render_inline_latex(th_grp[0], ctx)

    doc_start = raw_tex.find("\\begin{document}")
    doc_end = raw_tex.rfind("\\end{document}")
    body = raw_tex[doc_start + 16 : doc_end] if doc_start >= 0 and doc_end > doc_start else raw_tex
    body = re.sub(r"\\maketitle\b", "", body)
    body = re.sub(r"\\emergencystretch\s*=\s*[^\s]+", "", body)
    body = re.sub(r"\\bibliographystyle\{[^}]*\}", "", body)
    body = re.sub(r"\\bibliography\{[^}]*\}", "", body)

    abs_raw = ""
    abs_m = re.search(r"\\begin\{abstract\}(.*?)\\end\{abstract\}", body, re.S)
    if abs_m:
        abs_raw = abs_m.group(1).strip()
        body = body[: abs_m.start()] + body[abs_m.end() :]

    kw_raw = ""
    kw_m = re.search(r"\\begin\{IEEEkeywords\}(.*?)\\end\{IEEEkeywords\}", body, re.S)
    if kw_m:
        kw_raw = kw_m.group(1).strip()
        body = body[: kw_m.start()] + body[kw_m.end() :]
    else:
        kw_m2 = re.search(r"\\noindent\\textbf\{Keywords:\}\s*([^\n]+(?:\n[^\n\\]+)*)", body)
        if kw_m2:
            kw_raw = kw_m2.group(1).strip()
            body = body[: kw_m2.start()] + body[kw_m2.end() :]

    sc_raw = ""
    sc_m = re.search(r"\\noindent\\textbf\{Implementation scope\.\}(.*?)(?=\\section\{)", body, re.S)
    if sc_m:
        sc_raw = sc_m.group(1).strip()
        body = body[: sc_m.start()] + body[sc_m.end() :]

    collect_labels_pass1(body, ctx)
    title_html = render_inline_latex(title_raw, ctx)
    abstract_html = render_blocks(abs_raw, ctx, inside_block=True) if abs_raw else ""
    abstract_plain = html_to_plain_summary(abstract_html)
    keywords_html = render_inline_latex(kw_raw, ctx) if kw_raw else ""
    scope_note_html = render_inline_latex(sc_raw, ctx) if sc_raw else ""
    body_html = render_blocks(body, ctx)
    bib_html = render_bibliography_html(ctx)

    pdf_size_kb = max(1, round(pdf_path.stat().st_size / 1024)) if pdf_path.is_file() else 0
    year = spec.updated[:4]
    canonical_bibtex = (
        f"@techreport{{{spec.bibtex_key},\n"
        f"  author      = {{Ryan J. Kung}},\n"
        f"  title       = {{{title_plain}}},\n"
        f"  institution = {{Rings Network}},\n"
        f"  year        = {{{year}}},\n"
        f"  url         = {{{PAPERS_URL}{spec.slug}/}},\n"
        f"  note        = {{PDF: {PAPERS_URL}{spec.pdf_file}}}\n"
        f"}}"
    )

    md_sections = "\n".join(
        f"{'##' if item.level == 2 else '###'} {(item.number + ' ') if item.number else ''}{re.sub(r'<[^>]+>', '', item.title_html)}"
        for item in ctx.toc
    )
    markdown = (
        f"# {title_plain}\n\n"
        f"- **Series**: {spec.series_badge}\n"
        f"- **Author**: {author_plain} (ORCID: https://orcid.org/0009-0000-3026-1111)\n"
        f"- **Web Reader**: {PAPERS_URL}{spec.slug}/\n"
        f"- **PDF Download**: {PAPERS_URL}{spec.pdf_file} ({pdf_size_kb} KB, {spec.pages} pages)\n"
        f"- **LaTeX Source**: https://github.com/RingsNetwork/rings/blob/master/papers/{spec.tex_file}\n\n"
        f"## Abstract\n\n{abstract_plain}\n\n"
        f"## Outline\n\n{md_sections}\n\n"
        f"## Citation (BibTeX)\n\n```bibtex\n{canonical_bibtex}\n```\n"
    )

    return ParsedPaper(
        spec=spec,
        title_plain=title_plain,
        title_html=title_html,
        author_plain=author_plain,
        thanks_html=thanks_html,
        abstract_html=abstract_html,
        abstract_plain=abstract_plain,
        keywords_html=keywords_html,
        scope_note_html=scope_note_html,
        body_html=body_html,
        toc=ctx.toc,
        bib_html=bib_html,
        markdown=markdown,
        pdf_size_kb=pdf_size_kb,
        canonical_bibtex=canonical_bibtex,
    )


def site_header_html() -> str:
    """Return the shared Rings site header with Whitepaper (`Papers`) marked active."""

    return """\
    <header class="rings-header">
      <a class="rings-brand" href="https://rings.rs/">
        <span class="rings-mark" aria-hidden="true"><img src="/assets/icons/rings.svg" alt="" decoding="async"></span>
        <span class="rings-brand-text"><strong>Rings Network</strong><span>Research &amp; Papers</span></span>
      </a>
      <nav class="rings-nav" aria-label="Site">
        <a href="https://rings.rs/">Home</a>
        <a href="https://rings.rs/#guide">Guide</a>
        <a href="https://rings.rs/#node">Node</a>
        <a href="https://rings.rs/blog/">Blog</a>
        <a href="https://rings.rs/docs/">Docs</a>
        <a class="active" href="https://rings.rs/papers/" aria-current="page">Whitepaper</a>
      </nav>
    </header>"""


def site_footer_html() -> str:
    """Return the shared Rings site footer matching `frontend/src/footer.rs`."""

    return """\
    <footer class="rings-footer" aria-label="Site footer">
      <div class="rings-footer-grid">
        <div class="rings-footer-brand">
          <span class="rings-mark" aria-hidden="true"><img src="/assets/icons/rings.svg" alt="" decoding="async"></span>
          <strong>Rings Network</strong>
          <p>A peer-to-peer network for the sovereign age. Browser tabs and native daemons join one overlay, find each other by DID, and talk over direct WebRTC datachannels routed by a Chord DHT.</p>
        </div>
        <nav class="rings-footer-column" aria-label="Product">
          <h3>Product</h3>
          <ul>
            <li><a href="https://rings.rs/">Home</a></li>
            <li><a href="https://rings.rs/#guide">Guide</a></li>
            <li><a href="https://rings.rs/#node">Node</a></li>
          </ul>
        </nav>
        <nav class="rings-footer-column" aria-label="Developers">
          <h3>Developers</h3>
          <ul>
            <li><a href="https://rings.rs/docs/">Docs</a></li>
            <li><a href="https://github.com/RingsNetwork/rings">GitHub</a></li>
            <li><a href="https://github.com/RingsNetwork/rings/releases">Releases</a></li>
            <li><a href="https://crates.io/crates/rings-node">crates.io</a></li>
            <li><a href="https://www.npmjs.com/package/@ringsnetwork/rings-node">npm</a></li>
            <li><a href="https://rings.rs/llms.txt">llms.txt</a></li>
          </ul>
        </nav>
        <nav class="rings-footer-column" aria-label="Project">
          <h3>Project</h3>
          <ul>
            <li><a href="https://rings.rs/blog/">Blog</a></li>
            <li><a href="https://rings.rs/papers/">Whitepaper</a></li>
            <li><a href="https://github.com/RingsNetwork/rings/blob/master/SECURITY.md">Security model</a></li>
            <li><a href="https://github.com/RingsNetwork/rings/blob/master/ROADMAP.md">Roadmap</a></li>
            <li><a href="https://github.com/sponsors/RingsNetwork">Sponsor</a></li>
            <li><a href="https://x.com/RingsNetworkio">X (Twitter)</a></li>
            <li><a href="https://github.com/RingsNetwork/rings/blob/master/LICENSE">License</a></li>
          </ul>
        </nav>
      </div>
      <dl class="rings-footer-notices">
        <div><dt>License</dt><dd>The Rings source code is released under the GNU Affero General Public License, version 3.0 only (AGPL-3.0-only). Works derived from it, including services that offer it over a network, must be released under the same license.</dd></div>
        <div><dt>Commercial use</dt><dd>Under AGPL-3.0-only, commercial use is permitted when its copyleft and network source-disclosure obligations are met. Commercial licensing outside AGPL-3.0-only terms is available from Rings Network on request.</dd></div>
        <div><dt>No warranty</dt><dd>The software and this site are provided as is, without warranty of any kind, express or implied, as set out in sections 15 and 16 of the AGPL.</dd></div>
        <div><dt>Security boundary</dt><dd>Rings combines wallet-backed session delegation, end-to-end encrypted peer-to-peer channels, and multi-hop Sphinx onion routing. Review the security model for cryptographic guarantees and deployment best practices.</dd></div>
        <div><dt>Privacy</dt><dd>rings.rs uses no accounts, no tracking cookies, and no third-party analytics; paper pages record anonymous, cookie-free aggregate read and download counts. Browser-node keys, stores, and settings are kept locally in your browser's IndexedDB and local storage.</dd></div>
      </dl>
      <div class="rings-footer-bottom">
        <span>&copy; 2021&ndash;<span class="rings-footer-year">2026</span> Rings Network</span>
        <span>AGPL-3.0-only &middot; Built with Rust, Yew, and WebAssembly</span>
      </div>
      <script>
        document.querySelector('.rings-footer-year').textContent = String(new Date().getFullYear());
      </script>
    </footer>"""


TELEMETRY_JS = """\
(function () {
  "use strict";

  var ENDPOINT = window.RINGS_TELEMETRY_ENDPOINT || "/api/telemetry";
  var STATS_ENDPOINT = window.RINGS_TELEMETRY_STATS_ENDPOINT || (ENDPOINT + "/stats");

  function emitTelemetry(eventName, props) {
    var payload = {
      event: eventName,
      paper: (props && props.paper) || document.body.getAttribute("data-paper-slug") || "hub",
      format: (props && props.format) || undefined,
      source: (props && props.source) || undefined,
      mode: (props && props.mode) || undefined,
      depth: typeof (props && props.depth) === "number" ? props.depth : undefined,
      engaged_sec: typeof (props && props.engaged_sec) === "number" ? props.engaged_sec : undefined,
      path: window.location.pathname,
      referrer_host: safeReferrerHost(),
      ts: Date.now()
    };

    try {
      window.dispatchEvent(new CustomEvent("rings:paper-telemetry", { detail: payload }));
    } catch (_) {}

    var body = JSON.stringify(payload);
    try {
      if (navigator.sendBeacon) {
        var sent = navigator.sendBeacon(ENDPOINT, new Blob([body], { type: "text/plain;charset=UTF-8" }));
        if (sent) return;
      }
    } catch (_) {}

    try {
      fetch(ENDPOINT, {
        method: "POST",
        headers: { "Content-Type": "text/plain;charset=UTF-8" },
        body: body,
        keepalive: true,
        credentials: "omit"
      }).catch(function () {});
    } catch (_) {}
  }

  function safeReferrerHost() {
    try {
      return document.referrer ? new URL(document.referrer).hostname : "";
    } catch (_) {
      return "";
    }
  }

  window.ringsPaperTelemetry = { emit: emitTelemetry };

  // 1. Track downloads & citation copies across hub and reader pages.
  document.addEventListener("click", function (ev) {
    var dl = ev.target && ev.target.closest ? ev.target.closest("[data-paper-download]") : null;
    if (dl) {
      emitTelemetry("paper_download", {
        paper: dl.getAttribute("data-paper-download") || "rings",
        format: dl.getAttribute("data-paper-format") || "pdf",
        source: dl.getAttribute("data-paper-source") || "reader"
      });
    }

    var citeBtn = ev.target && ev.target.closest ? ev.target.closest("[data-paper-cite]") : null;
    if (citeBtn) {
      var slug = citeBtn.getAttribute("data-paper-cite") || "rings";
      var box = document.getElementById("bibtex-" + slug);
      if (box) {
        box.hidden = !box.hidden;
        var text = box.textContent || "";
        if (navigator.clipboard && navigator.clipboard.writeText) {
          navigator.clipboard.writeText(text.trim()).then(function () {
            var orig = citeBtn.getAttribute("data-label-orig") || citeBtn.textContent;
            citeBtn.setAttribute("data-label-orig", orig);
            citeBtn.textContent = "Copied BibTeX";
            setTimeout(function () { citeBtn.textContent = orig; }, 1800);
          }).catch(function () {});
        }
      }
      emitTelemetry("paper_cite_copy", { paper: slug, source: "cite-button" });
    }
  });

  // 2. Hydrate public anonymous read/download counters when /api/telemetry/stats is reachable.
  var statBadges = document.querySelectorAll("[data-paper-stats]");
  if (statBadges.length > 0 && typeof AbortController !== "undefined") {
    var ctrl = new AbortController();
    var timer = setTimeout(function () { ctrl.abort(); }, 2500);
    fetch(STATS_ENDPOINT, { signal: ctrl.signal, credentials: "omit" })
      .then(function (r) { clearTimeout(timer); return r.ok ? r.json() : null; })
      .then(function (data) {
        if (!data || !data.papers) return;
        statBadges.forEach(function (el) {
          var key = el.getAttribute("data-paper-stats");
          var p = data.papers[key];
          if (p && (p.reads > 0 || p.downloads > 0)) {
            el.textContent = Number(p.reads || 0).toLocaleString() + " reads · " + Number(p.downloads || 0).toLocaleString() + " downloads";
            el.hidden = false;
          }
        });
      })
      .catch(function () {});
  }

  // 3. Hub view vs. Individual Paper Reader lifecycle.
  var pageSlug = document.body.getAttribute("data-paper-slug");
  if (!pageSlug || pageSlug === "hub") {
    emitTelemetry("paper_hub_view", { paper: "hub" });
    return;
  }

  var webView = document.getElementById("paper-web-workspace");
  var pdfPane = document.getElementById("paper-pdf-pane");
  var pdfFrame = document.getElementById("paper-pdf-frame");
  var btnWeb = document.getElementById("btn-mode-web");
  var btnPdf = document.getElementById("btn-mode-pdf");
  var currentMode = "web";

  function setReaderMode(mode, firePreviewEvent) {
    currentMode = mode;
    if (!webView || !pdfPane || !btnWeb || !btnPdf) return;
    if (mode === "pdf") {
      webView.style.display = "none";
      pdfPane.hidden = false;
      btnWeb.classList.remove("paper-btn-active");
      btnPdf.classList.add("paper-btn-active");
      btnWeb.setAttribute("aria-pressed", "false");
      btnPdf.setAttribute("aria-pressed", "true");
      if (pdfFrame && !pdfFrame.getAttribute("src")) {
        var pdfSrc = pdfFrame.getAttribute("data-pdf-src");
        if (pdfSrc) pdfFrame.setAttribute("src", pdfSrc);
      }
      if (firePreviewEvent) {
        emitTelemetry("paper_pdf_preview", { paper: pageSlug, mode: "pdf" });
      }
    } else {
      webView.style.display = "";
      pdfPane.hidden = true;
      btnPdf.classList.remove("paper-btn-active");
      btnWeb.classList.add("paper-btn-active");
      btnPdf.setAttribute("aria-pressed", "false");
      btnWeb.setAttribute("aria-pressed", "true");
    }
  }

  if (btnWeb && btnPdf) {
    btnWeb.addEventListener("click", function () {
      if (window.history && window.history.replaceState) {
        window.history.replaceState(null, "", window.location.pathname);
      }
      setReaderMode("web", false);
    });
    btnPdf.addEventListener("click", function () {
      if (window.history && window.history.replaceState) {
        window.history.replaceState(null, "", window.location.pathname + "?view=pdf");
      }
      setReaderMode("pdf", true);
    });
  }

  var initialPdf = window.location.hash === "#pdf" || /[?&]view=pdf\b/.test(window.location.search);
  if (initialPdf) {
    setReaderMode("pdf", false);
  }
  emitTelemetry("paper_read", { paper: pageSlug, mode: initialPdf ? "pdf" : "web" });

  // 4. Scroll-depth progress bar, TOC spy, and milestone beacons (25%, 50%, 75%, 100%).
  var progressBar = document.getElementById("paper-progress-bar");
  var manuscript = document.getElementById("paper-manuscript");
  var seenDepths = {};
  var maxDepth = 0;

  function updateScrollMetrics() {
    if (!manuscript || currentMode !== "web") return;
    var rect = manuscript.getBoundingClientRect();
    var viewH = window.innerHeight || document.documentElement.clientHeight || 1;
    var total = Math.max(1, manuscript.offsetHeight - viewH);
    var scrolled = Math.min(total, Math.max(0, -rect.top));
    var pct = Math.round((scrolled / total) * 100);
    if (progressBar) progressBar.style.width = pct + "%";
    if (pct > maxDepth) maxDepth = pct;

    [25, 50, 75, 100].forEach(function (milestone) {
      if (pct >= milestone && !seenDepths[milestone]) {
        seenDepths[milestone] = true;
        emitTelemetry("paper_read_progress", { paper: pageSlug, depth: milestone });
      }
    });
  }

  window.addEventListener("scroll", updateScrollMetrics, { passive: true });
  updateScrollMetrics();

  // TOC active section observer
  var tocLinks = Array.prototype.slice.call(document.querySelectorAll(".paper-toc a[href^='#']"));
  if ("IntersectionObserver" in window && tocLinks.length > 0) {
    var linkById = {};
    tocLinks.forEach(function (a) { linkById[a.getAttribute("href").slice(1)] = a; });
    var observer = new IntersectionObserver(function (entries) {
      entries.forEach(function (entry) {
        if (entry.isIntersecting && linkById[entry.target.id]) {
          tocLinks.forEach(function (a) { a.classList.remove("active"); });
          linkById[entry.target.id].classList.add("active");
        }
      });
    }, { rootMargin: "-120px 0px -70% 0px", threshold: 0 });
    Object.keys(linkById).forEach(function (id) {
      var el = document.getElementById(id);
      if (el) observer.observe(el);
    });
  }

  // 5. Engaged reading time heartbeats (30s, 120s) + session exit flush.
  var engagedSec = 0;
  var sent30 = false;
  var sent120 = false;
  var flushed = false;

  setInterval(function () {
    if (document.visibilityState === "visible") {
      engagedSec += 5;
      if (engagedSec >= 30 && !sent30) {
        sent30 = true;
        emitTelemetry("paper_read_progress", { paper: pageSlug, depth: maxDepth, engaged_sec: 30 });
      } else if (engagedSec >= 120 && !sent120) {
        sent120 = true;
        emitTelemetry("paper_read_progress", { paper: pageSlug, depth: maxDepth, engaged_sec: 120 });
      }
    }
  }, 5000);

  function flushSession() {
    if (flushed || engagedSec < 5) return;
    flushed = true;
    emitTelemetry("paper_read_progress", { paper: pageSlug, depth: maxDepth, engaged_sec: engagedSec });
  }

  document.addEventListener("visibilitychange", function () {
    if (document.visibilityState === "hidden") flushSession();
  });
  window.addEventListener("pagehide", flushSession);
})();
"""


def papers_index_schema(papers: list[ParsedPaper]) -> str:
    """Return Schema.org JSON-LD `@graph` for `https://rings.rs/papers/`."""

    payload = json.dumps(
        {
            "@context": "https://schema.org",
            "@graph": [
                {
                    "@type": "BreadcrumbList",
                    "@id": f"{PAPERS_URL}#breadcrumb",
                    "itemListElement": [
                        {"@type": "ListItem", "position": 1, "name": "Rings Network", "item": SITE_URL},
                        {"@type": "ListItem", "position": 2, "name": "Papers & Whitepaper", "item": PAPERS_URL},
                    ],
                },
                {
                    "@type": "CollectionPage",
                    "@id": f"{PAPERS_URL}#collection",
                    "url": PAPERS_URL,
                    "name": "Rings Network Whitepaper & Protocol Specifications",
                    "description": "Read and download the unified Rings 1.0 Whitepaper and DRanking 2.0 Protocol Specification directly on rings.rs in responsive web typography or PDF.",
                    "inLanguage": "en",
                    "isPartOf": {"@id": f"{SITE_URL}#website"},
                    "publisher": {
                        "@type": "Organization",
                        "@id": f"{SITE_URL}#organization",
                        "name": "Rings Network",
                        "url": SITE_URL,
                    },
                    "hasPart": [
                        {
                            "@type": "ScholarlyArticle",
                            "@id": f"{PAPERS_URL}{p.spec.slug}/#article",
                            "headline": p.title_plain,
                            "url": f"{PAPERS_URL}{p.spec.slug}/",
                            "datePublished": p.spec.published,
                            "dateModified": p.spec.updated,
                            "author": {
                                "@type": "Person",
                                "name": p.author_plain,
                                "url": "https://orcid.org/0009-0000-3026-1111",
                            },
                            "encoding": {
                                "@type": "MediaObject",
                                "contentUrl": f"{PAPERS_URL}{p.spec.pdf_file}",
                                "encodingFormat": "application/pdf",
                            },
                        }
                        for p in papers
                    ],
                },
            ],
        },
        ensure_ascii=False,
        separators=(",", ":"),
    ).replace("<", "\\u003c")
    return f'<script type="application/ld+json">{payload}</script>'


def paper_reader_schema(paper: ParsedPaper) -> str:
    """Return Schema.org `ScholarlyArticle` JSON-LD `@graph` for `/papers/<slug>/`."""

    page_url = f"{PAPERS_URL}{paper.spec.slug}/"
    pdf_url = f"{PAPERS_URL}{paper.spec.pdf_file}"
    desc = paper.abstract_plain[:155].rsplit(" ", 1)[0] + "…" if len(paper.abstract_plain) > 160 else paper.abstract_plain
    payload = json.dumps(
        {
            "@context": "https://schema.org",
            "@graph": [
                {
                    "@type": "BreadcrumbList",
                    "@id": f"{page_url}#breadcrumb",
                    "itemListElement": [
                        {"@type": "ListItem", "position": 1, "name": "Rings Network", "item": SITE_URL},
                        {"@type": "ListItem", "position": 2, "name": "Papers", "item": PAPERS_URL},
                        {"@type": "ListItem", "position": 3, "name": paper.spec.short_title, "item": page_url},
                    ],
                },
                {
                    "@type": "ScholarlyArticle",
                    "@id": f"{page_url}#article",
                    "mainEntityOfPage": page_url,
                    "url": page_url,
                    "headline": paper.title_plain,
                    "name": paper.title_plain,
                    "description": desc,
                    "datePublished": paper.spec.published,
                    "dateModified": paper.spec.updated,
                    "inLanguage": "en",
                    "numberOfPages": paper.spec.pages,
                    "keywords": list(paper.spec.tags),
                    "author": {
                        "@type": "Person",
                        "name": paper.author_plain,
                        "url": "https://orcid.org/0009-0000-3026-1111",
                    },
                    "publisher": {
                        "@type": "Organization",
                        "@id": f"{SITE_URL}#organization",
                        "name": "Rings Network",
                        "url": SITE_URL,
                    },
                    "isPartOf": {"@id": f"{PAPERS_URL}#collection"},
                    "breadcrumb": {"@id": f"{page_url}#breadcrumb"},
                    "encoding": {
                        "@type": "MediaObject",
                        "contentUrl": pdf_url,
                        "encodingFormat": "application/pdf",
                    },
                },
            ],
        },
        ensure_ascii=False,
        separators=(",", ":"),
    ).replace("<", "\\u003c")
    return f'<script type="application/ld+json">{payload}</script>'


KATEX_HEAD_HTML = """\
    <link rel="stylesheet" href="https://cdnjs.cloudflare.com/ajax/libs/KaTeX/0.16.9/katex.min.css" crossorigin="anonymous"
      onerror="this.onerror=null;this.href='https://cdn.jsdelivr.net/npm/katex@0.16.11/dist/katex.min.css';">"""

KATEX_SCRIPTS_HTML = """\
    <script>
      (function () {
        var katexOpts = {
          delimiters: [
            { left: "\\\\[", right: "\\\\]", display: true },
            { left: "\\\\(", right: "\\\\)", display: false }
          ],
          throwOnError: false,
          trust: function (ctx) {
            return ctx.command === "\\\\href" && typeof ctx.url === "string" && ctx.url.indexOf("#") === 0;
          }
        };
        function runMath() {
          if (window.renderMathInElement) {
            window.renderMathInElement(document.body, katexOpts);
          }
        }
        function loadScript(src, fallback, next) {
          var s = document.createElement("script");
          s.src = src;
          s.crossOrigin = "anonymous";
          s.onload = next;
          s.onerror = function () {
            if (!fallback) return;
            var fb = document.createElement("script");
            fb.src = fallback;
            fb.crossOrigin = "anonymous";
            fb.onload = next;
            document.head.appendChild(fb);
          };
          document.head.appendChild(s);
        }
        loadScript(
          "https://cdnjs.cloudflare.com/ajax/libs/KaTeX/0.16.9/katex.min.js",
          "https://cdn.jsdelivr.net/npm/katex@0.16.11/dist/katex.min.js",
          function () {
            loadScript(
              "https://cdnjs.cloudflare.com/ajax/libs/KaTeX/0.16.9/contrib/auto-render.min.js",
              "https://cdn.jsdelivr.net/npm/katex@0.16.11/dist/contrib/auto-render.min.js",
              runMath
            );
          }
        );
      })();
    </script>"""


def render_papers_hub_html(papers: list[ParsedPaper]) -> str:
    """Render `https://rings.rs/papers/index.html`."""

    title = "Whitepaper & Protocol Papers | Rings Network"
    description = (
        "Read and download the Rings 1.0 Whitepaper and DRanking 2.0 Specification "
        "in responsive web typography or PDF on rings.rs."
    )
    cards: list[str] = []
    for p in papers:
        reader_url = f"/papers/{p.spec.slug}/"
        pdf_url = f"/papers/{p.spec.pdf_file}"
        md_url = f"/papers/{p.spec.slug}.md"
        tex_url = f"https://github.com/RingsNetwork/rings/blob/master/papers/{p.spec.tex_file}"
        tags_html = "".join(f"<li>{html.escape(t)}</li>" for t in p.spec.tags)
        cards.append(
            f"""\
        <article class="paper-hub-card" id="paper-{html.escape(p.spec.slug)}">
          <div class="paper-card-top">
            <span class="paper-series-badge">{html.escape(p.spec.series_badge)}</span>
            <span class="paper-stats-pill" data-paper-stats="{html.escape(p.spec.slug)}" hidden></span>
          </div>
          <h2><a href="{reader_url}">{p.title_html}</a></h2>
          <div class="paper-byline">
            <span>{html.escape(p.author_plain)} (<a href="https://orcid.org/0009-0000-3026-1111" target="_blank" rel="noreferrer">ORCID</a>)</span>
            <span>&middot;</span>
            <span>Updated <time datetime="{html.escape(p.spec.updated)}">{html.escape(p.spec.updated)}</time></span>
            <span>&middot;</span>
            <span>{p.spec.pages} pages &middot; {p.pdf_size_kb} KB PDF</span>
          </div>
          <div class="paper-abstract-preview">{p.abstract_html}</div>
          <ul class="paper-tags" aria-label="Topics">{tags_html}</ul>
          <div class="paper-card-actions">
            <a class="paper-btn paper-btn-primary" href="{reader_url}">Read Online (Web)</a>
            <a class="paper-btn" href="{reader_url}?view=pdf">Preview PDF</a>
            <a class="paper-btn" href="{pdf_url}?via=ui" download="{html.escape(p.spec.download_name)}" data-paper-download="{html.escape(p.spec.slug)}" data-paper-format="pdf" data-paper-source="hub">Download PDF ({p.pdf_size_kb} KB)</a>
            <button class="paper-btn" type="button" data-paper-cite="{html.escape(p.spec.slug)}">Cite BibTeX</button>
            <a class="paper-btn" href="{md_url}" data-paper-download="{html.escape(p.spec.slug)}" data-paper-format="md" data-paper-source="hub">Markdown</a>
            <a class="paper-btn" href="{tex_url}" target="_blank" rel="noreferrer">LaTeX Source</a>
          </div>
          <pre class="paper-bibtex-box" id="bibtex-{html.escape(p.spec.slug)}" hidden><code>{html.escape(p.canonical_bibtex)}</code></pre>
        </article>"""
        )

    cards_joined = "\n".join(cards)
    return f"""\
<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>{html.escape(title)}</title>
    <meta name="description" content="{html.escape(description, quote=True)}">
    <meta name="robots" content="max-image-preview:large">
    <meta name="theme-color" content="#fbf4e6">
    <link rel="icon" type="image/svg+xml" href="/assets/icons/favicon.svg">
    <link rel="icon" href="/favicon.ico" sizes="16x16 32x32 48x48">
    <link rel="apple-touch-icon" href="/apple-touch-icon.png">
    <link rel="canonical" href="{PAPERS_URL}">
    <link rel="alternate" type="text/markdown" href="{PAPERS_URL}index.md">
    <link rel="llms-txt" href="https://rings.rs/llms.txt">
    <link rel="llms-full-txt" href="https://rings.rs/llms-full.txt">
    <link rel="sitemap" type="application/xml" href="https://rings.rs/sitemap.xml">
    <meta property="og:type" content="website">
    <meta property="og:site_name" content="Rings Network">
    <meta property="og:title" content="{html.escape(title, quote=True)}">
    <meta property="og:description" content="{html.escape(description, quote=True)}">
    <meta property="og:url" content="{PAPERS_URL}">
    <meta property="og:image" content="https://rings.rs/assets/social/rings-card.png">
    <meta name="twitter:card" content="summary_large_image">
    <meta name="twitter:site" content="@RingsNetworkio">
    <meta name="twitter:title" content="{html.escape(title, quote=True)}">
    <meta name="twitter:description" content="{html.escape(description, quote=True)}">
    <meta name="twitter:image" content="https://rings.rs/assets/social/rings-card.png">
    {papers_index_schema(papers)}
{KATEX_HEAD_HTML}
    <style>{PAPERS_CSS}</style>
  </head>
  <body data-paper-slug="hub">
{site_header_html()}
    <main class="papers-hub-shell">
      <section class="papers-hero" aria-labelledby="papers-title">
        <p class="papers-kicker">Rings Network Formal Specifications</p>
        <h1 id="papers-title">Read the Rings protocol papers directly in your browser.</h1>
        <p>Rings publishes complete algebraic specifications, security proofs, empirical benchmarks, and Stateright finite-state verification models across two companion papers—readable natively on desktop and mobile with full mathematical typesetting or downloadable as typeset PDFs.</p>
      </section>
      <section class="papers-grid" aria-label="Protocol papers">
{cards_joined}
      </section>
    </main>
{site_footer_html()}
{KATEX_SCRIPTS_HTML}
    <script src="/papers/papers-telemetry.js" defer></script>
  </body>
</html>
"""


def render_paper_reader_html(paper: ParsedPaper, companion: ParsedPaper) -> str:
    """Render `/papers/<slug>/index.html` with Web Full-Text + Embedded PDF switcher and KaTeX."""

    page_url = f"{PAPERS_URL}{paper.spec.slug}/"
    pdf_path = f"/papers/{paper.spec.pdf_file}"
    md_path = f"/papers/{paper.spec.slug}.md"
    tex_url = f"https://github.com/RingsNetwork/rings/blob/master/papers/{paper.spec.tex_file}"
    page_title = f"{paper.spec.short_title} — {paper.title_plain[:34]}… | Rings" if len(paper.title_plain) > 36 else f"{paper.title_plain} | Rings"
    if len(page_title) > 60:
        page_title = f"{paper.spec.short_title} | Rings Network"
    desc = paper.abstract_plain[:155].rsplit(" ", 1)[0] + "…" if len(paper.abstract_plain) > 160 else paper.abstract_plain

    toc_items_html = "".join(
        f'<li class="{"toc-sub" if item.level == 3 else "toc-sec"}">'
        f'<a href="#{item.anchor}">{(html.escape(item.number) + ". ") if item.number else ""}{item.title_html}</a></li>'
        for item in paper.toc
    )
    kw_block = (
        f'<div class="paper-keywords"><strong>Keywords:</strong> {paper.keywords_html}</div>'
        if paper.keywords_html
        else ""
    )
    scope_block = (
        f'<div class="paper-keywords"><strong>Implementation Scope:</strong> {paper.scope_note_html}</div>'
        if paper.scope_note_html
        else ""
    )
    thanks_block = f'<p class="paper-thanks">{paper.thanks_html}</p>' if paper.thanks_html else ""

    return f"""\
<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>{html.escape(page_title)}</title>
    <meta name="description" content="{html.escape(desc, quote=True)}">
    <meta name="robots" content="max-image-preview:large">
    <meta name="theme-color" content="#fbf4e6">
    <link rel="icon" type="image/svg+xml" href="/assets/icons/favicon.svg">
    <link rel="icon" href="/favicon.ico" sizes="16x16 32x32 48x48">
    <link rel="apple-touch-icon" href="/apple-touch-icon.png">
    <link rel="canonical" href="{page_url}">
    <link rel="alternate" type="application/pdf" href="{PAPERS_URL}{paper.spec.pdf_file}">
    <link rel="alternate" type="text/markdown" href="{PAPERS_URL}{paper.spec.slug}.md">
    <link rel="llms-txt" href="https://rings.rs/llms.txt">
    <link rel="llms-full-txt" href="https://rings.rs/llms-full.txt">
    <link rel="sitemap" type="application/xml" href="https://rings.rs/sitemap.xml">
    <meta property="og:type" content="article">
    <meta property="og:site_name" content="Rings Network">
    <meta property="og:title" content="{html.escape(paper.title_plain, quote=True)}">
    <meta property="og:description" content="{html.escape(desc, quote=True)}">
    <meta property="og:url" content="{page_url}">
    <meta property="og:image" content="https://rings.rs/assets/social/rings-card.png">
    <meta name="citation_title" content="{html.escape(paper.title_plain, quote=True)}">
    <meta name="citation_author" content="{html.escape(paper.author_plain, quote=True)}">
    <meta name="citation_publication_date" content="{html.escape(paper.spec.updated.replace('-', '/'))}">
    <meta name="citation_pdf_url" content="{PAPERS_URL}{paper.spec.pdf_file}">
    <meta name="twitter:card" content="summary_large_image">
    <meta name="twitter:site" content="@RingsNetworkio">
    <meta name="twitter:title" content="{html.escape(paper.title_plain, quote=True)}">
    <meta name="twitter:description" content="{html.escape(desc, quote=True)}">
    <meta name="twitter:image" content="https://rings.rs/assets/social/rings-card.png">
    {paper_reader_schema(paper)}
{KATEX_HEAD_HTML}
    <style>{PAPERS_CSS}</style>
  </head>
  <body data-paper-slug="{html.escape(paper.spec.slug)}">
    <div class="paper-progress-track" aria-hidden="true"><div class="paper-progress-bar" id="paper-progress-bar"></div></div>
{site_header_html()}
    <main class="paper-reader-shell">
      <nav class="paper-breadcrumb" aria-label="Breadcrumb">
        <a href="https://rings.rs/">Rings Network</a> / <a href="/papers/">Papers</a> / <span>{html.escape(paper.spec.short_title)}</span>
      </nav>

      <div class="paper-toolbar" role="toolbar" aria-label="Paper reading controls">
        <div class="paper-mode-switch" role="group" aria-label="Reading format">
          <button class="paper-btn paper-btn-active" id="btn-mode-web" type="button" aria-pressed="true">Web Full-Text</button>
          <button class="paper-btn" id="btn-mode-pdf" type="button" aria-pressed="false">Embedded PDF ({paper.spec.pages} pp)</button>
          <span class="paper-stats-pill" data-paper-stats="{html.escape(paper.spec.slug)}" hidden></span>
        </div>
        <div class="paper-toolbar-actions">
          <a class="paper-btn paper-btn-primary" href="{pdf_path}?via=ui" download="{html.escape(paper.spec.download_name)}" data-paper-download="{html.escape(paper.spec.slug)}" data-paper-format="pdf" data-paper-source="toolbar">Download PDF ({paper.pdf_size_kb} KB)</a>
          <button class="paper-btn" type="button" data-paper-cite="{html.escape(paper.spec.slug)}">Cite BibTeX</button>
          <a class="paper-btn" href="{md_path}" data-paper-download="{html.escape(paper.spec.slug)}" data-paper-format="md" data-paper-source="toolbar">Markdown</a>
          <a class="paper-btn" href="{tex_url}" target="_blank" rel="noreferrer">LaTeX</a>
          <a class="paper-btn" href="/papers/{html.escape(companion.spec.slug)}/">Companion: {html.escape(companion.spec.short_title)} &rarr;</a>
        </div>
      </div>

      <pre class="paper-bibtex-box" id="bibtex-{html.escape(paper.spec.slug)}" hidden><code>{html.escape(paper.canonical_bibtex)}</code></pre>

      <section class="paper-pdf-pane" id="paper-pdf-pane" hidden aria-label="Embedded PDF viewer">
        <iframe class="paper-pdf-frame" id="paper-pdf-frame" title="{html.escape(paper.title_plain)} PDF" data-pdf-src="{pdf_path}?mode=preview&amp;via=ui#view=FitH"></iframe>
        <div class="paper-pdf-fallback">
          <span>Viewing typeset PDF directly from <code>rings.rs{pdf_path}</code>.</span>
          <a class="paper-btn paper-btn-primary" href="{pdf_path}?via=ui" download="{html.escape(paper.spec.download_name)}" data-paper-download="{html.escape(paper.spec.slug)}" data-paper-format="pdf" data-paper-source="pdf-pane">Download {html.escape(paper.spec.download_name)}</a>
        </div>
      </section>

      <div class="paper-workspace" id="paper-web-workspace">
        <aside class="paper-toc" aria-label="Table of contents">
          <p class="paper-toc-title">Contents</p>
          <ol>{toc_items_html}</ol>
        </aside>

        <article class="paper-manuscript" id="paper-manuscript">
          <header class="paper-header">
            <span class="paper-series-badge">{html.escape(paper.spec.series_badge)}</span>
            <h1>{paper.title_html}</h1>
            <div class="paper-byline">
              <span><strong>{html.escape(paper.author_plain)}</strong> (<a href="https://orcid.org/0009-0000-3026-1111" target="_blank" rel="noreferrer">0009-0000-3026-1111</a>)</span>
              <span>&middot;</span>
              <span>Created {html.escape(paper.spec.published)} &middot; Updated <time datetime="{html.escape(paper.spec.updated)}">{html.escape(paper.spec.updated)}</time></span>
            </div>
            {thanks_block}
          </header>

          <section class="paper-abstract" aria-label="Abstract">
            <span class="paper-abstract-label">Abstract</span>
            {paper.abstract_html}
            {kw_block}
            {scope_block}
          </section>

          {paper.body_html}
          {paper.bib_html}
        </article>
      </div>
    </main>
{site_footer_html()}
{KATEX_SCRIPTS_HTML}
    <script src="/papers/papers-telemetry.js" defer></script>
  </body>
</html>
"""


def render_papers_hub_markdown(papers: list[ParsedPaper]) -> str:
    """Render `https://rings.rs/papers/index.md` for AI agents and Markdown readers."""

    lines = [
        "# Rings Network Whitepaper & Protocol Specifications",
        "",
        f"Canonical paper reading hub: {PAPERS_URL}",
        "",
    ]
    for p in papers:
        lines.append(f"## {p.spec.series_badge}: {p.title_plain}")
        lines.append("")
        lines.append(f"- **Web Full-Text Reader**: {PAPERS_URL}{p.spec.slug}/")
        lines.append(f"- **Direct PDF ({p.pdf_size_kb} KB, {p.spec.pages} pp)**: {PAPERS_URL}{p.spec.pdf_file}")
        lines.append(f"- **Markdown Summary**: {PAPERS_URL}{p.spec.slug}.md")
        lines.append(f"- **Abstract**: {p.abstract_plain}")
        lines.append("")
    return "\n".join(lines)


def finish_papers(site: Path, papers_dir: Path = PAPERS_SOURCE_DIR) -> list[str]:
    """Compile all Rings papers into `<site>/papers/` and return canonical URLs for `sitemap.xml`."""

    if not (papers_dir / "rings.tex").is_file():
        return []
    compiled = [compile_paper(spec, papers_dir) for spec in PAPER_SPECS]
    out_dir = site / "papers"
    out_dir.mkdir(parents=True, exist_ok=True)

    (out_dir / "papers-telemetry.js").write_text(TELEMETRY_JS, encoding="utf-8")
    (out_dir / "index.html").write_text(render_papers_hub_html(compiled), encoding="utf-8")
    (out_dir / "index.md").write_text(render_papers_hub_markdown(compiled), encoding="utf-8")

    urls = [PAPERS_URL]
    for idx, paper in enumerate(compiled):
        companion = compiled[1 - idx] if len(compiled) == 2 else compiled[0]
        sub_dir = out_dir / paper.spec.slug
        sub_dir.mkdir(parents=True, exist_ok=True)
        (sub_dir / "index.html").write_text(render_paper_reader_html(paper, companion), encoding="utf-8")
        (sub_dir / "index.md").write_text(paper.markdown, encoding="utf-8")
        (out_dir / f"{paper.spec.slug}.md").write_text(paper.markdown, encoding="utf-8")

        pdf_src = papers_dir / paper.spec.pdf_file
        if pdf_src.is_file():
            shutil.copy2(pdf_src, out_dir / paper.spec.pdf_file)
        bib_src = papers_dir / paper.spec.bib_file
        if bib_src.is_file():
            shutil.copy2(bib_src, out_dir / paper.spec.bib_file)

        urls.append(f"{PAPERS_URL}{paper.spec.slug}/")
    return urls


if __name__ == "__main__":
    import sys

    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} SITE_DIR", file=sys.stderr)
        raise SystemExit(2)
    generated = finish_papers(Path(sys.argv[1]))
    print(f"Papers built: {len(generated)} pages -> {generated}")
