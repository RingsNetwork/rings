#!/usr/bin/env python3
"""Render the Rings Network engineering blog (`blog/`) into the built site (`<site>/blog/`).

Invoked by `finish_site.py` during both `qaci.yml` and `deploy-pages.yml` using only the Python
standard library so CI needs no extra packages. Emits:
- `<site>/blog/index.html` and `<site>/blog/index.md`
- `<site>/blog/feed.xml` (Atom 1.0 feed)
- `<site>/blog/<slug>/index.html` and `<site>/blog/<slug>.md` for every post in `blog/posts.json`
"""

from __future__ import annotations

import html
import json
import re
from dataclasses import dataclass
from pathlib import Path
from xml.sax.saxutils import escape as xml_escape

SITE_URL = "https://rings.rs/"
BLOG_URL = f"{SITE_URL}blog/"
REPO_ROOT = Path(__file__).resolve().parents[2]
BLOG_SOURCE_DIR = REPO_ROOT / "blog"

INLINE_CODE_RE = re.compile(r"`([^`]+)`")
IMAGE_LINE_RE = re.compile(r"^!\[([^\]]*)\]\(([^)]+)\)$")
LINK_RE = re.compile(r"\[([^\]]+)\]\(([^)]+)\)")
BOLD_RE = re.compile(r"\*\*([^*]+)\*\*")
ITALIC_RE = re.compile(r"\*([^*]+)\*")


@dataclass(frozen=True)
class BlogPost:
    """One engineering blog post loaded from `blog/posts.json` and `blog/<slug>.md`."""

    slug: str
    title: str
    published: str
    author: str
    author_url: str
    medium_url: str
    cover: str
    summary: str
    tags: tuple[str, ...]
    markdown: str

    @property
    def url(self) -> str:
        return f"{BLOG_URL}{self.slug}/"

    @property
    def markdown_url(self) -> str:
        return f"{BLOG_URL}{self.slug}.md"

    @property
    def cover_url(self) -> str:
        if self.cover.startswith("http://") or self.cover.startswith("https://"):
            return self.cover
        return f"{SITE_URL}{self.cover.lstrip('/')}"


def load_posts(blog_dir: Path = BLOG_SOURCE_DIR) -> list[BlogPost]:
    """Load and validate every post declared in `blog/posts.json`."""

    manifest_path = blog_dir / "posts.json"
    if not manifest_path.is_file():
        raise ValueError(f"missing blog manifest: {manifest_path}")
    entries = json.loads(manifest_path.read_text(encoding="utf-8"))
    posts: list[BlogPost] = []
    for entry in entries:
        slug = str(entry["slug"]).strip()
        md_path = blog_dir / f"{slug}.md"
        if not md_path.is_file():
            raise ValueError(f"missing markdown file for blog post '{slug}': {md_path}")
        posts.append(
            BlogPost(
                slug=slug,
                title=str(entry["title"]).strip(),
                published=str(entry["published"]).strip(),
                author=str(entry["author"]).strip(),
                author_url=str(entry["author_url"]).strip(),
                medium_url=str(entry["medium_url"]).strip(),
                cover=str(entry["cover"]).strip(),
                summary=str(entry["summary"]).strip(),
                tags=tuple(str(tag).strip() for tag in entry.get("tags", [])),
                markdown=md_path.read_text(encoding="utf-8").strip(),
            )
        )
    return posts


def render_inline(text: str) -> str:
    """Render inline Markdown (code spans, links, bold, emphasis) into safe HTML."""

    code_tokens: list[str] = []

    def stash_code(match: re.Match[str]) -> str:
        index = len(code_tokens)
        code_tokens.append(f"<code>{html.escape(match.group(1))}</code>")
        return f"\x00CODE{index}\x00"

    stashed = INLINE_CODE_RE.sub(stash_code, text)
    escaped = html.escape(stashed, quote=False)

    def replace_link(match: re.Match[str]) -> str:
        label = match.group(1)
        href = html.escape(html.unescape(match.group(2)), quote=True)
        return f'<a href="{href}">{label}</a>'

    linked = LINK_RE.sub(replace_link, escaped)
    bolded = BOLD_RE.sub(r"<strong>\1</strong>", linked)
    italicized = ITALIC_RE.sub(r"<em>\1</em>", bolded)
    for index, token in enumerate(code_tokens):
        italicized = italicized.replace(f"\x00CODE{index}\x00", token)
    return italicized


def slugify_heading(text: str) -> str:
    """Turn a section heading into a stable anchor id."""

    plain = re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")
    return plain or "section"


def render_markdown(markdown: str, skip_leading_cover: str | None = None) -> str:
    """Render a blog post's Markdown body into semantic HTML."""

    lines = markdown.splitlines()
    blocks: list[str] = []
    index = 0
    seen_first_block = False

    while index < len(lines):
        line = lines[index]
        stripped = line.strip()
        if not stripped:
            index += 1
            continue

        if stripped.startswith("```"):
            language = stripped[3:].strip()
            code_lines: list[str] = []
            index += 1
            while index < len(lines) and not lines[index].strip().startswith("```"):
                code_lines.append(lines[index])
                index += 1
            if index < len(lines):
                index += 1
            lang_attr = f' class="language-{html.escape(language, quote=True)}"' if language else ""
            code_html = html.escape("\n".join(code_lines))
            blocks.append(f"<pre><code{lang_attr}>{code_html}</code></pre>")
            seen_first_block = True
            continue

        image_match = IMAGE_LINE_RE.match(stripped)
        if image_match:
            alt_text = html.escape(image_match.group(1), quote=True)
            src = html.escape(image_match.group(2), quote=True)
            index += 1
            if not seen_first_block and skip_leading_cover and image_match.group(2) == skip_leading_cover:
                seen_first_block = True
                continue
            caption = f"<figcaption>{html.escape(image_match.group(1))}</figcaption>" if alt_text else ""
            blocks.append(
                f'<figure class="blog-figure"><img src="{src}" alt="{alt_text}" loading="lazy" decoding="async">{caption}</figure>'
            )
            seen_first_block = True
            continue

        if stripped.startswith("### "):
            heading_raw = stripped[4:].strip()
            anchor = slugify_heading(heading_raw)
            blocks.append(f'<h3 id="{anchor}">{render_inline(heading_raw)}</h3>')
            index += 1
            seen_first_block = True
            continue

        if stripped.startswith("## "):
            heading_raw = stripped[3:].strip()
            anchor = slugify_heading(heading_raw)
            blocks.append(f'<h2 id="{anchor}">{render_inline(heading_raw)}</h2>')
            index += 1
            seen_first_block = True
            continue

        if stripped.startswith("# "):
            heading_raw = stripped[2:].strip()
            anchor = slugify_heading(heading_raw)
            blocks.append(f'<h2 id="{anchor}">{render_inline(heading_raw)}</h2>')
            index += 1
            seen_first_block = True
            continue

        if stripped.startswith("> "):
            quote_lines: list[str] = []
            while index < len(lines) and lines[index].strip().startswith(">"):
                quote_lines.append(lines[index].strip().lstrip(">").strip())
                index += 1
            blocks.append(f"<blockquote><p>{render_inline(' '.join(quote_lines))}</p></blockquote>")
            seen_first_block = True
            continue

        if re.match(r"^\d+\.\s+", stripped):
            items: list[str] = []
            while index < len(lines) and re.match(r"^\d+\.\s+", lines[index].strip()):
                item_text = re.sub(r"^\d+\.\s+", "", lines[index].strip())
                items.append(f"<li>{render_inline(item_text)}</li>")
                index += 1
            blocks.append(f"<ol>{''.join(items)}</ol>")
            seen_first_block = True
            continue

        if stripped.startswith("- "):
            items = []
            while index < len(lines) and lines[index].strip().startswith("- "):
                items.append(f"<li>{render_inline(lines[index].strip()[2:].strip())}</li>")
                index += 1
            blocks.append(f"<ul>{''.join(items)}</ul>")
            seen_first_block = True
            continue

        paragraph_lines: list[str] = []
        while (
            index < len(lines)
            and lines[index].strip()
            and not lines[index].strip().startswith(("```", "# ", "## ", "### ", "> ", "- "))
            and not IMAGE_LINE_RE.match(lines[index].strip())
            and not re.match(r"^\d+\.\s+", lines[index].strip())
        ):
            paragraph_lines.append(lines[index].strip())
            index += 1
        blocks.append(f"<p>{render_inline(' '.join(paragraph_lines))}</p>")
        seen_first_block = True

    return "\n".join(blocks)


BLOG_STYLESHEET = (BLOG_SOURCE_DIR / "blog.css").read_text(encoding="utf-8").strip()


def site_header_html() -> str:
    """Return the shared Rings site header with Blog marked active."""

    return """\
    <header class="rings-header">
      <a class="rings-brand" href="https://rings.rs/">
        <span class="rings-mark" aria-hidden="true"><img src="/assets/icons/rings.svg" alt="" decoding="async"></span>
        <span class="rings-brand-text"><strong>Rings Network</strong><span>Engineering Blog</span></span>
      </a>
      <nav class="rings-nav" aria-label="Site">
        <a href="https://rings.rs/">Home</a>
        <a href="https://rings.rs/#guide">Guide</a>
        <a href="https://rings.rs/#node">Node</a>
        <a class="active" href="https://rings.rs/blog/" aria-current="page">Blog</a>
        <a href="https://rings.rs/docs/">Docs</a>
        <a href="https://github.com/RingsNetwork/rings/blob/master/papers/rings.pdf">Whitepaper</a>
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
            <li><a href="https://github.com/RingsNetwork/rings/blob/master/papers/rings.pdf">Whitepaper</a></li>
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
        <div><dt>Commercial use</dt><dd>Rings is open-source under the GNU General Public License v3.0 (GPL-3.0), giving you full freedom to run, inspect, modify, and self-host nodes and applications.</dd></div>
        <div><dt>No warranty</dt><dd>The software and this site are provided as is, without warranty of any kind, express or implied, as set out in sections 15 and 16 of the AGPL.</dd></div>
        <div><dt>Security boundary</dt><dd>Rings combines wallet-backed session delegation, end-to-end encrypted peer-to-peer channels, and multi-hop Sphinx onion routing. Review the security model for cryptographic guarantees and deployment best practices.</dd></div>
        <div><dt>Privacy</dt><dd>rings.rs is a static site: no accounts, no analytics, no tracking cookies. Browser-node settings are kept in your browser's local storage.</dd></div>
      </dl>
      <div class="rings-footer-bottom">
        <span>&copy; 2021&ndash;2026 Rings Network</span>
        <span>AGPL-3.0-only &middot; Built with Rust, Yew, and WebAssembly</span>
      </div>
    </footer>"""


def blog_index_schema(posts: list[BlogPost]) -> str:
    """Return JSON-LD @graph for `https://rings.rs/blog/`."""

    payload = json.dumps(
        {
            "@context": "https://schema.org",
            "@graph": [
                {
                    "@type": "BreadcrumbList",
                    "@id": f"{BLOG_URL}#breadcrumb",
                    "itemListElement": [
                        {"@type": "ListItem", "position": 1, "name": "Rings Network", "item": SITE_URL},
                        {"@type": "ListItem", "position": 2, "name": "Blog", "item": BLOG_URL},
                    ],
                },
                {
                    "@type": "Blog",
                    "@id": f"{BLOG_URL}#blog",
                    "url": BLOG_URL,
                    "name": "Rings Network Engineering Blog",
                    "description": "Engineering essays on decentralized WebRTC P2P, Sphinx/AEZ onion routing, stateless cryptographic TURN, Chord DHT convergence, and algebraic correctness.",
                    "inLanguage": "en",
                    "isPartOf": {"@id": f"{SITE_URL}#website"},
                    "publisher": {
                        "@type": "Organization",
                        "@id": f"{SITE_URL}#organization",
                        "name": "Rings Network",
                        "url": SITE_URL,
                    },
                    "blogPost": [
                        {
                            "@type": "BlogPosting",
                            "@id": f"{post.url}#article",
                            "headline": post.title,
                            "url": post.url,
                            "datePublished": post.published,
                            "author": {"@type": "Person", "name": post.author, "url": post.author_url},
                        }
                        for post in posts
                    ],
                },
            ],
        },
        ensure_ascii=False,
        separators=(",", ":"),
    ).replace("<", "\\u003c")
    return f'<script type="application/ld+json">{payload}</script>'


def blog_post_schema(post: BlogPost) -> str:
    """Return JSON-LD @graph for an individual blog post."""

    payload = json.dumps(
        {
            "@context": "https://schema.org",
            "@graph": [
                {
                    "@type": "BreadcrumbList",
                    "@id": f"{post.url}#breadcrumb",
                    "itemListElement": [
                        {"@type": "ListItem", "position": 1, "name": "Rings Network", "item": SITE_URL},
                        {"@type": "ListItem", "position": 2, "name": "Blog", "item": BLOG_URL},
                        {"@type": "ListItem", "position": 3, "name": post.title, "item": post.url},
                    ],
                },
                {
                    "@type": "BlogPosting",
                    "@id": f"{post.url}#article",
                    "mainEntityOfPage": post.url,
                    "url": post.url,
                    "headline": post.title,
                    "name": post.title,
                    "description": post.summary,
                    "datePublished": post.published,
                    "dateModified": post.published,
                    "inLanguage": "en",
                    "image": post.cover_url,
                    "keywords": list(post.tags),
                    "author": {
                        "@type": "Person",
                        "name": post.author,
                        "url": post.author_url,
                    },
                    "publisher": {
                        "@type": "Organization",
                        "@id": f"{SITE_URL}#organization",
                        "name": "Rings Network",
                        "url": SITE_URL,
                    },
                    "isPartOf": {"@id": f"{BLOG_URL}#blog"},
                    "breadcrumb": {"@id": f"{post.url}#breadcrumb"},
                    "sameAs": [post.medium_url] if post.medium_url else [],
                    "speakable": {
                        "@type": "SpeakableSpecification",
                        "cssSelector": ["h1", ".blog-prose p:first-of-type", ".blog-prose h2"],
                    },
                },
            ],
        },
        ensure_ascii=False,
        separators=(",", ":"),
    ).replace("<", "\\u003c")
    return f'<script type="application/ld+json">{payload}</script>'


def render_blog_index_html(posts: list[BlogPost]) -> str:
    """Render `https://rings.rs/blog/index.html`."""

    title = "Rings Network Blog — P2P, Onion Routing & Distributed Systems"
    description = (
        "Engineering articles from Rings Network on browser-native WebRTC P2P, "
        "multi-hop onion TURN relays, Chord DHT convergence, and algebraic correctness."
    )
    cards: list[str] = []
    for post in posts:
        tags_html = "".join(f"<li>{html.escape(tag)}</li>" for tag in post.tags)
        cards.append(
            f"""\
        <article class="blog-card">
          <a class="blog-card-media" href="{html.escape(post.url)}">
            <img src="{html.escape(post.cover)}" alt="{html.escape(post.title, quote=True)}" loading="lazy" decoding="async">
          </a>
          <div class="blog-card-body">
            <div class="blog-meta">
              <time datetime="{html.escape(post.published)}">{html.escape(post.published)}</time>
              <span>&middot;</span>
              <a href="{html.escape(post.author_url)}" target="_blank" rel="noreferrer">{html.escape(post.author)}</a>
              <span>&middot;</span>
              <a href="{html.escape(post.medium_url)}" target="_blank" rel="noreferrer">Medium</a>
              <span>&middot;</span>
              <a href="{html.escape(post.markdown_url)}">Markdown</a>
            </div>
            <h2><a href="{html.escape(post.url)}">{html.escape(post.title)}</a></h2>
            <p>{html.escape(post.summary)}</p>
            <ul class="blog-tags" aria-label="Tags">{tags_html}</ul>
          </div>
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
    <link rel="canonical" href="{BLOG_URL}">
    <link rel="alternate" type="application/atom+xml" title="Rings Network Blog" href="{BLOG_URL}feed.xml">
    <link rel="alternate" type="text/markdown" href="{BLOG_URL}index.md">
    <link rel="llms-txt" href="https://rings.rs/llms.txt">
    <link rel="llms-full-txt" href="https://rings.rs/llms-full.txt">
    <link rel="sitemap" type="application/xml" href="https://rings.rs/sitemap.xml">
    <meta property="og:type" content="website">
    <meta property="og:site_name" content="Rings Network">
    <meta property="og:title" content="{html.escape(title, quote=True)}">
    <meta property="og:description" content="{html.escape(description, quote=True)}">
    <meta property="og:url" content="{BLOG_URL}">
    <meta property="og:image" content="{html.escape(posts[0].cover_url, quote=True)}">
    <meta name="twitter:card" content="summary_large_image">
    <meta name="twitter:site" content="@RingsNetworkio">
    <meta name="twitter:title" content="{html.escape(title, quote=True)}">
    <meta name="twitter:description" content="{html.escape(description, quote=True)}">
    <meta name="twitter:image" content="{html.escape(posts[0].cover_url, quote=True)}">
    {blog_index_schema(posts)}
    <style>{BLOG_STYLESHEET}</style>
  </head>
  <body>
{site_header_html()}
    <main class="blog-shell">
      <section class="blog-hero" aria-labelledby="blog-title">
        <p class="blog-kicker">Rings Network Engineering Blog</p>
        <h1 id="blog-title">Notes on peer-to-peer architecture, onion privacy, and correctness.</h1>
        <p>Deep dives into how Rings connects browsers and native nodes directly over WebRTC, routes anonymous circuits with Sphinx/AEZ and stateless cryptographic TURN, and verifies distributed state transitions.</p>
        <div class="blog-hero-links">
          <a href="{BLOG_URL}feed.xml">Atom Feed</a>
          <span>&middot;</span>
          <a href="{BLOG_URL}index.md">Markdown Index</a>
          <span>&middot;</span>
          <a href="https://medium.com/@ryankung" target="_blank" rel="noreferrer">Medium Archive (@ryankung)</a>
        </div>
      </section>
      <section class="blog-grid" aria-label="Articles">
{cards_joined}
      </section>
    </main>
{site_footer_html()}
  </body>
</html>
"""


def render_blog_post_html(post: BlogPost, newer: BlogPost | None, older: BlogPost | None) -> str:
    """Render `https://rings.rs/blog/<slug>/index.html`."""

    page_title = f"{post.title} | Rings Network Blog"
    body_html = render_markdown(post.markdown, skip_leading_cover=post.cover)
    tags_html = "".join(f"<li>{html.escape(tag)}</li>" for tag in post.tags)
    nav_links: list[str] = []
    if newer:
        nav_links.append(f'<a rel="prev" href="{html.escape(newer.url)}">&larr; {html.escape(newer.title)}</a>')
    else:
        nav_links.append(f'<a href="{BLOG_URL}">&larr; All articles</a>')
    if older:
        nav_links.append(f'<a rel="next" href="{html.escape(older.url)}">{html.escape(older.title)} &rarr;</a>')
    nav_html = "\n          ".join(nav_links)

    return f"""\
<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>{html.escape(page_title)}</title>
    <meta name="description" content="{html.escape(post.summary, quote=True)}">
    <meta name="robots" content="max-image-preview:large">
    <meta name="theme-color" content="#fbf4e6">
    <link rel="icon" type="image/svg+xml" href="/assets/icons/favicon.svg">
    <link rel="icon" href="/favicon.ico" sizes="16x16 32x32 48x48">
    <link rel="apple-touch-icon" href="/apple-touch-icon.png">
    <link rel="canonical" href="{html.escape(post.url)}">
    <link rel="alternate" type="application/atom+xml" title="Rings Network Blog" href="{BLOG_URL}feed.xml">
    <link rel="alternate" type="text/markdown" href="{html.escape(post.markdown_url)}">
    <link rel="llms-txt" href="https://rings.rs/llms.txt">
    <link rel="llms-full-txt" href="https://rings.rs/llms-full.txt">
    <link rel="sitemap" type="application/xml" href="https://rings.rs/sitemap.xml">
    <meta property="og:type" content="article">
    <meta property="og:site_name" content="Rings Network">
    <meta property="og:title" content="{html.escape(post.title, quote=True)}">
    <meta property="og:description" content="{html.escape(post.summary, quote=True)}">
    <meta property="og:url" content="{html.escape(post.url)}">
    <meta property="og:image" content="{html.escape(post.cover_url, quote=True)}">
    <meta property="article:published_time" content="{html.escape(post.published)}T00:00:00Z">
    <meta property="article:author" content="{html.escape(post.author, quote=True)}">
    <meta name="twitter:card" content="summary_large_image">
    <meta name="twitter:site" content="@RingsNetworkio">
    <meta name="twitter:title" content="{html.escape(post.title, quote=True)}">
    <meta name="twitter:description" content="{html.escape(post.summary, quote=True)}">
    <meta name="twitter:image" content="{html.escape(post.cover_url, quote=True)}">
    {blog_post_schema(post)}
    <style>{BLOG_STYLESHEET}</style>
  </head>
  <body>
{site_header_html()}
    <main class="blog-article-shell">
      <nav class="blog-breadcrumb" aria-label="Breadcrumb">
        <a href="https://rings.rs/">Rings Network</a> / <a href="{BLOG_URL}">Blog</a> / <span>{html.escape(post.title)}</span>
      </nav>
      <article>
        <header class="blog-article-header">
          <div class="blog-meta">
            <time datetime="{html.escape(post.published)}">{html.escape(post.published)}</time>
            <span>&middot;</span>
            <a href="{html.escape(post.author_url)}" target="_blank" rel="noreferrer">{html.escape(post.author)}</a>
            <span>&middot;</span>
            <a href="{html.escape(post.medium_url)}" target="_blank" rel="noreferrer">Read on Medium</a>
            <span>&middot;</span>
            <a href="{html.escape(post.markdown_url)}">Raw Markdown</a>
          </div>
          <h1>{html.escape(post.title)}</h1>
          <ul class="blog-tags" aria-label="Tags">{tags_html}</ul>
        </header>
        <figure class="blog-article-cover">
          <img src="{html.escape(post.cover)}" alt="{html.escape(post.title, quote=True)}" decoding="async">
        </figure>
        <div class="blog-prose">
{body_html}
        </div>
        <nav class="blog-post-nav" aria-label="Article navigation">
          {nav_html}
        </nav>
      </article>
    </main>
{site_footer_html()}
  </body>
</html>
"""


def render_blog_feed_xml(posts: list[BlogPost]) -> str:
    """Render an Atom 1.0 feed at `https://rings.rs/blog/feed.xml`."""

    updated = f"{posts[0].published}T00:00:00Z" if posts else "2026-01-01T00:00:00Z"
    entries: list[str] = []
    for post in posts:
        entries.append(
            f"""\
  <entry>
    <title>{xml_escape(post.title)}</title>
    <link href="{xml_escape(post.url)}" rel="alternate"/>
    <id>{xml_escape(post.url)}</id>
    <published>{xml_escape(post.published)}T00:00:00Z</published>
    <updated>{xml_escape(post.published)}T00:00:00Z</updated>
    <author><name>{xml_escape(post.author)}</name><uri>{xml_escape(post.author_url)}</uri></author>
    <summary>{xml_escape(post.summary)}</summary>
  </entry>"""
        )
    joined = "\n".join(entries)
    return f"""\
<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Rings Network Engineering Blog</title>
  <subtitle>Engineering notes on decentralized WebRTC P2P, Sphinx/AEZ onion routing, stateless cryptographic TURN, and Chord DHT correctness.</subtitle>
  <link href="{BLOG_URL}feed.xml" rel="self"/>
  <link href="{BLOG_URL}" rel="alternate"/>
  <id>{BLOG_URL}</id>
  <updated>{updated}</updated>
{joined}
</feed>
"""


def render_blog_index_markdown(posts: list[BlogPost]) -> str:
    """Render `https://rings.rs/blog/index.md` for AI agents and Markdown readers."""

    lines = [
        "# Rings Network Engineering Blog",
        "",
        "Canonical engineering articles from Rings Network (https://rings.rs/blog/).",
        "",
    ]
    for post in posts:
        lines.append(f"- [{post.title}]({post.url}) ({post.published}) — [Markdown]({post.markdown_url})")
        lines.append(f"  {post.summary}")
    lines.append("")
    return "\n".join(lines)


def render_post_markdown_artifact(post: BlogPost) -> str:
    """Render `https://rings.rs/blog/<slug>.md` with title and canonical frontmatter block."""

    return (
        f"# {post.title}\n\n"
        f"- Published: {post.published}\n"
        f"- Author: [{post.author}]({post.author_url})\n"
        f"- Canonical URL: {post.url}\n"
        f"- Medium URL: {post.medium_url}\n\n"
        f"{post.markdown}\n"
    )


def finish_blog(site: Path, blog_dir: Path = BLOG_SOURCE_DIR) -> list[str]:
    """Build the blog under `<site>/blog/` and return all indexable blog URLs."""

    if not (blog_dir / "posts.json").is_file():
        return []
    posts = load_posts(blog_dir)
    out_dir = site / "blog"
    out_dir.mkdir(parents=True, exist_ok=True)
    (out_dir / "index.html").write_text(render_blog_index_html(posts), encoding="utf-8")
    (out_dir / "index.md").write_text(render_blog_index_markdown(posts), encoding="utf-8")
    (out_dir / "feed.xml").write_text(render_blog_feed_xml(posts), encoding="utf-8")

    urls = [BLOG_URL]
    for idx, post in enumerate(posts):
        newer = posts[idx - 1] if idx > 0 else None
        older = posts[idx + 1] if idx + 1 < len(posts) else None
        post_dir = out_dir / post.slug
        post_dir.mkdir(parents=True, exist_ok=True)
        (post_dir / "index.html").write_text(render_blog_post_html(post, newer, older), encoding="utf-8")
        (out_dir / f"{post.slug}.md").write_text(render_post_markdown_artifact(post), encoding="utf-8")
        urls.append(post.url)
    return urls
