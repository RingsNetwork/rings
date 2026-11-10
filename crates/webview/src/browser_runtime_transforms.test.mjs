import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import "./browser_runtime_transforms.mjs";

const transforms = globalThis.__ringsWebviewTransforms;
const prefix = "/webview/";
const base = "https://example.test/docs/index.html";
const gateway = (url) => prefix + encodeURIComponent(url);
const urls = transforms.createUrlTransformer({
  prefix,
  targetBase: base,
  locationOrigin: "https://webview.rings.test",
  controlledAssetPaths: ["/assets/webview-overlay.js"],
});

test("srcset tokenizer preserves candidate boundaries", () => {
  const cases = JSON.parse(readFileSync(new URL("./srcset_contract.json", import.meta.url)));
  for (const fixture of cases) {
    assert.deepEqual(transforms.parseSrcsetCandidates(fixture.input), fixture.candidates, fixture.name);
    assert.equal(
      transforms.encodeSrcset(fixture.input, base, urls.encodeTarget),
      fixture.js_output,
      `${fixture.name} rewrite output`,
    );
  }
});

test("pure transforms retain safe schemes and rewrite HTTP candidates", () => {
  const encoded = transforms.encodeSrcset(
    "data:image/png;base64,AAAA 1x, javascript:alert(1) 2x, image.png 3x",
    base,
    urls.encodeTarget,
  );
  assert.equal(encoded, `data:image/png;base64,AAAA 1x, javascript:alert(1) 2x, ${gateway("https://example.test/docs/image.png")} 3x`);
  assert.equal(
    transforms.encodeCssText('background:url("quoted image.png"); mask:url(escaped%20image.svg)', urls.encodeTarget),
    `background:url("${gateway("https://example.test/docs/quoted%20image.png")}"); mask:url(${gateway("https://example.test/docs/escaped%20image.svg")})`,
  );
  assert.equal(transforms.htmlAttributeKind("imagesrcset"), "srcset");
  assert.equal(transforms.htmlAttributeKind("aria-label"), "plain");
});

test("controlled asset paths stay same-origin while cross-origin paths route through gateway", () => {
  assert.equal(urls.encodeTarget("/assets/webview-overlay.js"), "/assets/webview-overlay.js");
  assert.equal(
    urls.encodeTarget("https://webview.rings.test/assets/webview-overlay.js?v=1"),
    "/assets/webview-overlay.js?v=1",
  );
  assert.equal(
    urls.encodeTarget("https://evil.test/assets/webview-overlay.js"),
    gateway("https://evil.test/assets/webview-overlay.js"),
  );
  assert.equal(
    urls.encodeTarget("//evil.test/assets/webview-overlay.js"),
    gateway("https://evil.test/assets/webview-overlay.js"),
  );
});

test("encodeTarget guards malformed URLs without throwing or aborting multi-URL attributes", () => {
  assert.equal(urls.encodeTarget("http://[invalid"), "http://[invalid");
  assert.equal(urls.encodeTarget("ftp://example.test/archive.tar"), "ftp://example.test/archive.tar");
  assert.equal(
    transforms.encodeSrcset("http://[invalid 1x, valid.png 2x", base, urls.encodeTarget),
    `http://[invalid 1x, ${gateway("https://example.test/docs/valid.png")} 2x`,
  );
  assert.equal(
    transforms.encodeUrlList("http://[invalid /ping", base, urls.encodeTarget),
    `http://[invalid ${gateway("https://example.test/ping")}`,
  );
});
