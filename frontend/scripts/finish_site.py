#!/usr/bin/env python3
"""Finish the built rings.rs site for search engines and language models.

deploy-pages.yml runs this on the Pages artifact once Trunk has built the frontend, mdBook the
book under `docs/`, and the shell has been copied to `404.html`; QACI runs it on the book alone.

- Each chapter of the book names its own address as canonical, and describes itself by its
  first paragraph instead of the book's one description, so each can be found on its own.
- The pages kept out of indexes say so: the book's own 404.html, and the site's 404.html, the
  shell copy that boots the WebView gateway on a cold load of a path-routed URL.
- sitemap.xml lists the landing page and every chapter, for robots.txt to point at.
"""

from __future__ import annotations

import html
import re
import sys
from pathlib import Path
from xml.sax.saxutils import escape

SITE_URL = "https://rings.rs/"
BOOK_DIR = "docs"
# The most characters search engines show of a description before cutting it.
DESCRIPTION_LIMIT = 160
NOINDEX = '<meta name="robots" content="noindex">'
# mdBook marks its print and table-of-contents pages noindex itself; its 404.html is ours to mark.
BOOK_NOT_FOUND = "404.html"
DESCRIPTION_META = re.compile(
    r'(<meta (?:name|property)="(?:description|og:description|twitter:description)" content=")[^"]*(")'
)
CHARSET = re.compile(r"<meta charset=[^>]*>", re.I)
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


def chapter_url(relative: Path) -> str:
    """Return the address of the book's page at `relative`, a path under the book."""

    path = relative.as_posix()
    return f"{SITE_URL}{BOOK_DIR}/" if path == "index.html" else f"{SITE_URL}{BOOK_DIR}/{path}"


def finish_chapter(document: str, url: str) -> str:
    """Return a chapter naming `url` as canonical and described by its first paragraph."""

    marker = "</head>"
    if marker not in document:
        raise ValueError("the page has no </head>")
    description = chapter_description(document)
    if description:
        quoted = html.escape(description, quote=True)
        document = DESCRIPTION_META.sub(lambda match: f"{match.group(1)}{quoted}{match.group(2)}", document)
    if 'rel="canonical"' not in document:
        link = (
            f'    <link rel="canonical" href="{html.escape(url)}">\n'
            f'        <meta property="og:url" content="{html.escape(url)}">\n    '
        )
        document = document.replace(marker, f"{link}{marker}", 1)
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
    urls = [SITE_URL, *finish_book(book)]
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
