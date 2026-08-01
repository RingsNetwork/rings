import assert from "node:assert/strict";
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
  const cases = [
    {
      name: "data URL with descriptor",
      input: "data:image/svg+xml,%3Csvg%3E,%3C/svg%3E 1x, /next.png 2x",
      candidates: [
        { url: "data:image/svg+xml,%3Csvg%3E,%3C/svg%3E", descriptor: "1x" },
        { url: "/next.png", descriptor: "2x" },
      ],
    },
    {
      name: "descriptor-less data URL",
      input: "data:image/svg+xml,%3Csvg%3E,%3C/svg%3E, /next.png 2x",
      candidates: [
        { url: "data:image/svg+xml,%3Csvg%3E,%3C/svg%3E", descriptor: "" },
        { url: "/next.png", descriptor: "2x" },
      ],
    },
    {
      name: "consecutive separators and whitespace",
      input: "  first.png 1x,,,\n /next.png 2x  ",
      candidates: [
        { url: "first.png", descriptor: "1x" },
        { url: "/next.png", descriptor: "2x" },
      ],
    },
    {
      name: "quoted and escaped URLs",
      input: '"quoted image.png" 640w, escaped\\,image.png 2x',
      candidates: [
        { url: "quoted image.png", descriptor: "640w" },
        { url: "escaped,image.png", descriptor: "2x" },
      ],
    },
  ];
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
