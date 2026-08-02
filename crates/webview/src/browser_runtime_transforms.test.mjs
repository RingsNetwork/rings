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
