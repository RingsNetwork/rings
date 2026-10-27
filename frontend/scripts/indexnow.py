#!/usr/bin/env python3
"""Tell the search engines that take IndexNow that rings.rs's pages are new.

Bing takes IndexNow, and through Bing so do the answer engines that search with it; Yandex,
Naver, Seznam and others share what it is told. deploy-pages.yml runs this after each deploy,
so they need not wait to crawl the site to learn of a change:

    frontend/scripts/indexnow.py [--print]     --print shows the request and sends nothing

The pages are those the deployed sitemap.xml lists. The key is the name of `frontend/<key>.txt`,
which the site serves at its root, so the engines can check that the request is the site's.
"""

from __future__ import annotations

import json
import re
import sys
import urllib.error
import urllib.request
from pathlib import Path

HOST = "rings.rs"
SITEMAP_URL = f"https://{HOST}/sitemap.xml"
ENDPOINT = "https://api.indexnow.org/indexnow"
KEY_FILE = re.compile(r"^([0-9a-f]{32})\.txt$")
LOC = re.compile(r"<loc>([^<]+)</loc>")
USER_AGENT = "rings.rs deploy (https://github.com/RingsNetwork/rings)"


def site_key(frontend: Path) -> str:
    """Return the IndexNow key: the name of the one `<key>.txt` in `frontend`."""

    keys = [match.group(1) for path in frontend.iterdir() if (match := KEY_FILE.match(path.name))]
    if len(keys) != 1:
        raise ValueError(f"expected one <key>.txt in {frontend}, found {len(keys)}")
    return keys[0]


def fetch(url: str) -> str:
    """Return the text at `url`."""

    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(request, timeout=30) as response:
        return response.read().decode("utf-8")


def submission(key: str, urls: list[str]) -> dict[str, object]:
    """Return the IndexNow request that announces `urls`, proven by `key`."""

    return {"host": HOST, "key": key, "keyLocation": f"https://{HOST}/{key}.txt", "urlList": urls}


def main(arguments: list[str]) -> int:
    """Announce the deployed sitemap's pages, or show the request with --print."""

    if len(arguments) > 2 or (len(arguments) == 2 and arguments[1] != "--print"):
        print(f"usage: {arguments[0]} [--print]", file=sys.stderr)
        return 2
    try:
        key = site_key(Path(__file__).resolve().parent.parent)
        urls = LOC.findall(fetch(SITEMAP_URL))
        if not urls:
            raise ValueError(f"{SITEMAP_URL} lists no pages")
        body = json.dumps(submission(key, urls)).encode("utf-8")
        if len(arguments) == 2:
            print(f"POST {ENDPOINT}\n{body.decode('utf-8')}")
            return 0
        request = urllib.request.Request(
            ENDPOINT,
            data=body,
            headers={"Content-Type": "application/json; charset=utf-8", "User-Agent": USER_AGENT},
            method="POST",
        )
        with urllib.request.urlopen(request, timeout=30) as response:
            print(f"IndexNow took {len(urls)} pages ({response.status})")
    except (OSError, ValueError, urllib.error.URLError) as error:
        print(f"indexnow: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
