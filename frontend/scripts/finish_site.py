#!/usr/bin/env python3
"""Finish the built rings.rs site for search engines and language models.

deploy-pages.yml runs this on the Pages artifact once Trunk has built the frontend, mdBook the
book under `docs/`, and the shell has been copied to `404.html`; QACI runs it on the book alone.

- Each chapter of the book names its own address as canonical, describes itself by its first
  paragraph instead of the book's one description, and carries a connected TechArticle +
  BreadcrumbList JSON-LD @graph so each chapter can be found and cited on its own.
- `llms-full.txt` and `index.md` are composed directly from `llms.txt` and `docs/src/README.md`
  with zero duplicate source files in the repository.
- The pages kept out of indexes say so: the book's own 404.html, and the site's 404.html, the
  shell copy that boots the WebView gateway on a cold load of a path-routed URL.
- sitemap.xml lists the landing page and every chapter, for robots.txt to point at.
"""

from __future__ import annotations

import html
import json
import re
import sys
from pathlib import Path
from xml.sax.saxutils import escape

from build_blog import finish_blog

SITE_URL = "https://rings.rs/"
BOOK_DIR = "docs"
REPO_ROOT = Path(__file__).resolve().parents[2]
# The most characters search engines show of a description before cutting it.
DESCRIPTION_LIMIT = 160
NOINDEX = '<meta name="robots" content="noindex">'
# mdBook marks its print and table-of-contents pages noindex itself; its 404.html is ours to mark.
BOOK_NOT_FOUND = "404.html"
DESCRIPTION_META = re.compile(
    r'(<meta (?:name|property)="(?:description|og:description|twitter:description)" content=")[^"]*(")'
)
CHARSET = re.compile(r"<meta charset=[^>]*>", re.I)
TITLE = re.compile(r"<title>(.*?)</title>", re.S | re.I)
MAIN = re.compile(r"<main>(.*?)</main>", re.S)
PARAGRAPH = re.compile(r"<p>(.*?)</p>", re.S)
TAG = re.compile(r"<[^>]+>")


def is_noindex(document: str) -> bool:
    """Return whether a page asks to be kept out of indexes."""

    return NOINDEX in document


def mark_noindex(document: str) -> str:
    """Return `document` asking to be kept out of indexes, once, after its charset, which stays
    first in the head."""

    if is_noindex(document):
        return document
    charset = CHARSET.search(document)
    if charset:
        return f"{document[: charset.end()]}\n    {NOINDEX}{document[charset.end() :]}"
    head, found, rest = document.partition("<head>")
    if not found:
        raise ValueError("the page has no <head>")
    return f"{head}<head>\n    {NOINDEX}{rest}"


def first_sentences(text: str, limit: int = DESCRIPTION_LIMIT) -> str:
    """Return the whole sentences of `text` that fit in `limit` characters, or else its words
    that fit, ending in an ellipsis."""

    words = " ".join(text.split())
    if len(words) <= limit:
        return words
    sentences = re.split(r"(?<=[.!?])\s+", words)
    kept = ""
    for sentence in sentences:
        candidate = f"{kept} {sentence}".strip()
        if len(candidate) > limit:
            break
        kept = candidate
    if kept:
        return kept
    cut = words[: limit - 1].rsplit(" ", 1)[0].rstrip(",;:")
    return f"{cut}…"


def chapter_description(document: str) -> str | None:
    """Return a chapter's description: its first paragraph's text, within the limit."""

    main = MAIN.search(document)
    if not main:
        return None
    for paragraph in PARAGRAPH.findall(main.group(1)):
        text = html.unescape(TAG.sub("", paragraph)).strip()
        if text:
            return first_sentences(text)
    return None


def chapter_title(document: str) -> str:
    """Return the plain title text of a book chapter."""

    match = TITLE.search(document)
    if not match:
        return "Rings Network Documentation"
    return " ".join(html.unescape(TAG.sub("", match.group(1))).split()) or "Rings Network Documentation"


def chapter_schema(url: str, title: str, description: str | None) -> str:
    """Return a compact connected BreadcrumbList + TechArticle JSON-LD script for a book page."""

    docs_root = f"{SITE_URL}{BOOK_DIR}/"
    breadcrumbs = [
        {"@type": "ListItem", "position": 1, "name": "Rings Network", "item": SITE_URL},
        {"@type": "ListItem", "position": 2, "name": "Docs", "item": docs_root},
    ]
    if url != docs_root:
        breadcrumbs.append({"@type": "ListItem", "position": 3, "name": title, "item": url})
    article: dict[str, object] = {
        "@type": "TechArticle",
        "@id": f"{url}#article",
        "url": url,
        "headline": title,
        "name": title,
        "inLanguage": "en",
        "isPartOf": {"@id": f"{SITE_URL}#website"},
        "publisher": {
            "@type": "Organization",
            "@id": f"{SITE_URL}#organization",
            "name": "Rings Network",
            "url": SITE_URL,
        },
        "breadcrumb": {"@id": f"{url}#breadcrumb"},
    }
    if description:
        article["description"] = description
    payload = json.dumps(
        {
            "@context": "https://schema.org",
            "@graph": [
                {
                    "@type": "BreadcrumbList",
                    "@id": f"{url}#breadcrumb",
                    "itemListElement": breadcrumbs,
                },
                article,
            ],
        },
        ensure_ascii=False,
        separators=(",", ":"),
    ).replace("<", "\\u003c")
    return f'<script type="application/ld+json">{payload}</script>'


def chapter_url(relative: Path) -> str:
    """Return the address of the book's page at `relative`, a path under the book."""

    path = relative.as_posix()
    return f"{SITE_URL}{BOOK_DIR}/" if path == "index.html" else f"{SITE_URL}{BOOK_DIR}/{path}"


def finish_chapter(document: str, url: str) -> str:
    """Return a chapter naming `url` as canonical, described by its first paragraph, with JSON-LD."""

    marker = "</head>"
    if marker not in document:
        raise ValueError("the page has no </head>")
    description = chapter_description(document)
    if description:
        quoted = html.escape(description, quote=True)
        document = DESCRIPTION_META.sub(lambda match: f"{match.group(1)}{quoted}{match.group(2)}", document)
    additions: list[str] = []
    if 'rel="canonical"' not in document:
        additions.append(f'    <link rel="canonical" href="{html.escape(url)}">')
        additions.append(f'        <meta property="og:url" content="{html.escape(url)}">')
    if 'type="application/ld+json"' not in document:
        additions.append(f"        {chapter_schema(url, chapter_title(document), description)}")
    if additions:
        joined = "\n".join(additions)
        document = document.replace(marker, f"{joined}\n    {marker}", 1)
    return document


def finish_book(book: Path) -> list[str]:
    """Finish every page of the book under `book`; return the addresses of those to index."""

    urls: list[str] = []
    for page in sorted(book.rglob("*.html")):
        relative = page.relative_to(book)
        document = page.read_text(encoding="utf-8")
        if relative.as_posix() == BOOK_NOT_FOUND:
            page.write_text(mark_noindex(document), encoding="utf-8")
            continue
        if is_noindex(document) or "<main>" not in document:
            continue
        url = chapter_url(relative)
        page.write_text(finish_chapter(document, url), encoding="utf-8")
        urls.append(url)
    # The book's start page first, then its chapters in order of their paths.
    return sorted(urls, key=lambda url: (url != f"{SITE_URL}{BOOK_DIR}/", url))


def emit_agent_artifacts(site: Path) -> None:
    """Compose `index.md` and `llms-full.txt` from `llms.txt` and `docs/src/README.md`."""

    book_readme = REPO_ROOT / "docs" / "src" / "README.md"
    if not (site / "index.html").is_file() or not book_readme.is_file():
        return
    clean_readme = re.sub(r"^<img\b[^>]*>\n+", "", book_readme.read_text(encoding="utf-8"), flags=re.M)
    markdown_home = clean_readme.replace("# Start Here", "# Rings Network", 1)
    (site / "index.md").write_text(markdown_home, encoding="utf-8")
    llms_path = site / "llms.txt"
    if not llms_path.is_file():
        llms_path = REPO_ROOT / "llms.txt"
    if llms_path.is_file():
        llms_text = llms_path.read_text(encoding="utf-8").rstrip()
        (site / "llms-full.txt").write_text(f"{llms_text}\n\n---\n\n{markdown_home}", encoding="utf-8")


def sitemap(urls: list[str]) -> str:
    """Return the site's map of `urls`."""

    entries = "".join(f"  <url>\n    <loc>{escape(url)}</loc>\n  </url>\n" for url in urls)
    return (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n'
        f"{entries}</urlset>\n"
    )


def finish_site(site: Path) -> list[str]:
    """Finish the built site under `site`; return the addresses its map lists."""

    book = site / BOOK_DIR
    if not book.is_dir():
        raise ValueError(f"{book} holds no book")
    not_found = site / "404.html"
    if not_found.is_file():
        not_found.write_text(mark_noindex(not_found.read_text(encoding="utf-8")), encoding="utf-8")
    emit_agent_artifacts(site)
    urls = [SITE_URL, *finish_blog(site), *finish_book(book)]
    (site / "sitemap.xml").write_text(sitemap(urls), encoding="utf-8")
    return urls


def main(arguments: list[str]) -> int:
    """Finish the site in the directory named, and report what its map lists."""

    if len(arguments) != 2:
        print(f"usage: {arguments[0]} SITE_DIR", file=sys.stderr)
        return 2
    try:
        urls = finish_site(Path(arguments[1]))
    except (OSError, ValueError) as error:
        print(f"finish_site: {error}", file=sys.stderr)
        return 1
    print(f"Site finished: sitemap.xml lists {len(urls)} pages")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
