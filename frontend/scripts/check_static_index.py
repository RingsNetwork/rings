#!/usr/bin/env python3
"""Verify that the built frontend remains useful before JavaScript or WASM executes."""

from __future__ import annotations

import json
import os
import sys
from dataclasses import dataclass, field
from html.parser import HTMLParser
from pathlib import Path
from typing import Any

STATIC_ROOT_ID = "static-project-introduction"
CANONICAL_URL = "https://rings.rs/"
REPOSITORY_URL = "https://github.com/RingsNetwork/rings"
REQUIRED_LINKS = {
    "https://rings.rs/docs/",
    REPOSITORY_URL,
    "https://github.com/RingsNetwork/rings/blob/master/papers/rings.pdf",
    "https://rings.rs/llms.txt",
}
REQUIRED_PROJECT_CONTENT = {"name", "tagline", "introduction"}
VOID_ELEMENTS = {"area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track", "wbr"}


@dataclass
class Inspection:
    """Semantic facts extracted from the initial HTML document."""

    static_roots: int = 0
    static_root_tag: str | None = None
    static_root_attributes: dict[str, str | None] = field(default_factory=dict)
    static_root_before_scripts: bool = False
    static_depth: int = 0
    static_h1_count: int = 0
    static_h2_count: int = 0
    static_h3_count: int = 0
    project_content_markers: list[str] = field(default_factory=list)
    static_text: list[str] = field(default_factory=list)
    static_links: set[str] = field(default_factory=set)
    description: str | None = None
    canonical: str | None = None
    json_ld: list[str] = field(default_factory=list)


class InitialHtmlParser(HTMLParser):
    """Extract crawler-visible content without executing or rendering the page."""

    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.inspection = Inspection()
        self._seen_executable_script = False
        self._json_ld_buffer: list[str] | None = None

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        self._handle_start(tag, attrs, self_closing=False)

    def handle_startendtag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        self._handle_start(tag, attrs, self_closing=True)

    def _handle_start(self, tag: str, attrs: list[tuple[str, str | None]], self_closing: bool) -> None:
        attributes = dict(attrs)
        if tag == "script":
            if attributes.get("type") == "application/ld+json":
                self._json_ld_buffer = []
            else:
                self._seen_executable_script = True
        elif tag == "meta" and attributes.get("name") == "description":
            self.inspection.description = attributes.get("content")
        elif tag == "link" and attributes.get("rel") == "canonical":
            self.inspection.canonical = attributes.get("href")

        if attributes.get("id") == STATIC_ROOT_ID:
            self.inspection.static_roots += 1
            self.inspection.static_root_tag = tag
            self.inspection.static_root_attributes = attributes
            self.inspection.static_depth = 1
        elif self.inspection.static_depth > 0 and tag not in VOID_ELEMENTS and not self_closing:
            self.inspection.static_depth += 1

        if self.inspection.static_depth > 0:
            if tag == "h1":
                self.inspection.static_h1_count += 1
            elif tag == "h2":
                self.inspection.static_h2_count += 1
            elif tag == "h3":
                self.inspection.static_h3_count += 1
            project_content = attributes.get("data-project-content")
            if project_content:
                self.inspection.project_content_markers.append(project_content)
            href = attributes.get("href")
            if tag == "a" and href:
                self.inspection.static_links.add(href)

    def handle_endtag(self, tag: str) -> None:
        if tag == "script" and self._json_ld_buffer is not None:
            self.inspection.json_ld.append("".join(self._json_ld_buffer))
            self._json_ld_buffer = None
        if self.inspection.static_depth > 0 and tag not in VOID_ELEMENTS:
            self.inspection.static_depth -= 1
            if self.inspection.static_depth == 0 and not self._seen_executable_script:
                self.inspection.static_root_before_scripts = True

    def handle_data(self, data: str) -> None:
        if self._json_ld_buffer is not None:
            self._json_ld_buffer.append(data)
        elif self.inspection.static_depth > 0:
            text = " ".join(data.split())
            if text:
                self.inspection.static_text.append(text)


def validate(inspection: Inspection) -> list[str]:
    """Return every violated initial-document invariant."""

    text = " ".join(inspection.static_text)
    errors: list[str] = []
    if inspection.static_roots != 1:
        errors.append(f"expected one #{STATIC_ROOT_ID} root, found {inspection.static_roots}")
    if inspection.static_root_tag != "main":
        errors.append(f"#{STATIC_ROOT_ID} must be a main element")
    if not inspection.static_root_before_scripts:
        errors.append("static project content must precede every executable script")
    if "hidden" in inspection.static_root_attributes or inspection.static_root_attributes.get("aria-hidden") == "true":
        errors.append("static project content must not be hidden from users or crawlers")
    if inspection.static_h1_count != 1:
        errors.append(f"static project content must contain one h1, found {inspection.static_h1_count}")
    if inspection.static_h2_count < 2:
        errors.append("static project content must contain at least two h2 section headings")
    if inspection.static_h3_count < 4:
        errors.append("static project content must contain at least four h3 feature headings")
    if len(text.split()) < 120:
        errors.append("static project content must contain at least 120 words")
    project_content_markers = inspection.project_content_markers
    if len(project_content_markers) != len(REQUIRED_PROJECT_CONTENT):
        errors.append("static project content must contain exactly three canonical copy markers")
    for missing_marker in sorted(REQUIRED_PROJECT_CONTENT - set(project_content_markers)):
        errors.append(f"static project content lacks canonical copy marker: {missing_marker}")
    for required_link in sorted(REQUIRED_LINKS - inspection.static_links):
        errors.append(f"static project content lacks required link: {required_link}")
    if not inspection.description:
        errors.append("document lacks a meta description")
    if inspection.canonical != CANONICAL_URL:
        errors.append(f"document canonical URL must be {CANONICAL_URL}")
    errors.extend(validate_structured_data(inspection.json_ld))
    return errors


def validate_structured_data(documents: list[str]) -> list[str]:
    """Validate the project-level SoftwareSourceCode JSON-LD record."""

    records: list[Any] = []
    for document in documents:
        try:
            records.append(json.loads(document))
        except json.JSONDecodeError as error:
            return [f"invalid JSON-LD: {error}"]
    source_records = [
        record
        for record in records
        if isinstance(record, dict) and record.get("@type") == "SoftwareSourceCode"
    ]
    if len(source_records) != 1:
        return [f"expected one SoftwareSourceCode JSON-LD record, found {len(source_records)}"]
    source = source_records[0]
    errors: list[str] = []
    if source.get("@context") != "https://schema.org":
        errors.append("SoftwareSourceCode JSON-LD must use the https://schema.org context")
    if source.get("name") != "Rings Network":
        errors.append("SoftwareSourceCode JSON-LD must name Rings Network")
    if source.get("url") != CANONICAL_URL:
        errors.append(f"SoftwareSourceCode JSON-LD URL must be {CANONICAL_URL}")
    if source.get("codeRepository") != REPOSITORY_URL:
        errors.append(f"SoftwareSourceCode codeRepository must be {REPOSITORY_URL}")
    return errors


def main(arguments: list[str]) -> int:
    """Inspect one built index file and report a compact pass/fail result."""

    if len(arguments) > 2:
        print(f"usage: {arguments[0]} [BUILT_INDEX_HTML]", file=sys.stderr)
        return 2
    if len(arguments) == 2:
        index_path = Path(arguments[1])
    else:
        staging_dir = os.environ.get("TRUNK_STAGING_DIR")
        if not staging_dir:
            print("BUILT_INDEX_HTML or TRUNK_STAGING_DIR is required", file=sys.stderr)
            return 2
        index_path = Path(staging_dir) / "index.html"
    try:
        document = index_path.read_text(encoding="utf-8")
    except OSError as error:
        print(f"cannot read {index_path}: {error}", file=sys.stderr)
        return 2
    parser = InitialHtmlParser()
    parser.feed(document)
    errors = validate(parser.inspection)
    if errors:
        for error in errors:
            print(f"static index check failed: {error}", file=sys.stderr)
        return 1
    print(f"Static index check passed: {index_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
