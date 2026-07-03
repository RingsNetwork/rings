#!/usr/bin/env node

import { execFile } from "node:child_process";
import { cp, mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const projectRoot = resolve(scriptDir, "..");
const sourceDist = resolve(projectRoot, process.argv[2] ?? "dist");
const extensionDist = resolve(projectRoot, process.argv[3] ?? "dist-extension");
const execFileAsync = promisify(execFile);
const sourceIconSvg = join(projectRoot, "assets", "icons", "rings.svg");
const extensionAssets = resolve(projectRoot, "extension-assets");

const cargoToml = await readFile(join(projectRoot, "Cargo.toml"), "utf8");
const crateVersion = cargoToml.match(/^version\s*=\s*"([^"]+)"/m)?.[1] ?? "0.1.0";
const extensionVersion = chromeVersion(crateVersion);

const ICON_STATES = {
  neutral: {
    file: "rings",
    color: "#00e5ff",
  },
  disconnected: {
    file: "rings-disconnected",
    color: "#d8fbff",
  },
  connecting: {
    file: "rings-connecting",
    color: "#ffcb6b",
  },
  connected: {
    file: "rings-connected",
    color: "#54ffd0",
  },
};

let sourceRoot = sourceDist;
let files = await readdir(sourceRoot);
if (!files.some((file) => file.endsWith(".js"))) {
  const stageDist = join(sourceDist, ".stage");
  const stageFiles = await readdir(stageDist).catch(() => []);
  if (stageFiles.some((file) => file.endsWith(".js"))) {
    sourceRoot = stageDist;
    files = stageFiles;
  }
}
const sourceHtml = await readFile(join(sourceRoot, "index.html"), "utf8").catch(() => "");
const jsFile =
  entryFileFromHtml(sourceHtml, files, /import\s+init[\s\S]*?from\s+['"]([^'"]+\.js)['"]/) ??
  entryFileFromHtml(sourceHtml, files, /<link[^>]+rel="modulepreload"[^>]+href="([^"]+\.js)"/) ??
  singleFile(files, (file) => file.endsWith(".js"), "generated JS bundle");
const wasmFile =
  entryFileFromHtml(sourceHtml, files, /module_or_path:\s*['"]([^'"]+_bg\.wasm)['"]/) ??
  entryFileFromHtml(sourceHtml, files, /<link[^>]+rel="preload"[^>]+href="([^"]+_bg\.wasm)"/) ??
  singleFile(files, (file) => file.endsWith("_bg.wasm"), "generated wasm bundle");

await rm(extensionDist, { force: true, recursive: true });
await mkdir(extensionDist, { recursive: true });
await cp(join(sourceRoot, jsFile), join(extensionDist, jsFile));
await cp(join(sourceRoot, wasmFile), join(extensionDist, wasmFile));

await writeFile(
  join(extensionDist, "index.html"),
  htmlShell(jsFile, wasmFile, { includeNodeBridge: true }),
  "utf8",
);
await writeFile(
  join(extensionDist, "offscreen.html"),
  htmlShell(jsFile, wasmFile, { includeNodeBridge: false }),
  "utf8",
);
await writeFile(
  join(extensionDist, "bootstrap.js"),
  bootstrapScript(jsFile, wasmFile),
  "utf8",
);
await cp(join(extensionAssets, "wallet_bridge.js"), join(extensionDist, "wallet_bridge.js"));
await cp(join(extensionAssets, "node_bridge.js"), join(extensionDist, "node_bridge.js"));
await cp(join(extensionAssets, "service_worker.js"), join(extensionDist, "service_worker.js"));
await writeExtensionIcons();
await writeFile(
  join(extensionDist, "manifest.json"),
  `${JSON.stringify(manifest(extensionVersion), null, 2)}\n`,
  "utf8",
);

console.log(`Packaged Chrome MV3 extension at ${extensionDist}`);

function singleFile(files, predicate, label) {
  const matches = files.filter(predicate);
  if (matches.length !== 1) {
    throw new Error(`Expected one ${label} in ${sourceRoot}, found ${matches.length}`);
  }
  return matches[0];
}

function entryFileFromHtml(html, files, pattern) {
  const match = html.match(pattern);
  if (!match) {
    return undefined;
  }
  const file = match[1].replace(/^\.\//, "").replace(/^\//, "");
  return files.includes(file) ? file : undefined;
}

function chromeVersion(version) {
  const parts = version.split(".").map((part) => part.replace(/\D.*$/, "") || "0");
  while (parts.length < 3) {
    parts.push("0");
  }
  return parts.slice(0, 4).join(".");
}

async function writeExtensionIcons() {
  const iconsDir = join(extensionDist, "icons");
  await mkdir(iconsDir, { recursive: true });
  const iconSvg = await readFile(sourceIconSvg, "utf8");
  await writeFile(join(iconsDir, "rings.svg"), iconSvg, "utf8");
  for (const state of Object.values(ICON_STATES)) {
    const tempDir = await mkdtemp(join(tmpdir(), "rings-icon-"));
    const svgPath = join(tempDir, `${state.file}.svg`);
    try {
      await writeFile(svgPath, tintIconSvg(iconSvg, state.color), "utf8");
      for (const size of [16, 32, 48, 128]) {
        await renderSvgToPng(svgPath, join(iconsDir, `${state.file}-${size}.png`), size);
      }
    } finally {
      await rm(tempDir, { force: true, recursive: true });
    }
  }
}

function tintIconSvg(svg, color) {
  const tinted = svg.replace(/\sfill="(?!none\b)[^"]*"/gi, ` fill="${color}"`);
  if (tinted !== svg) {
    return tinted;
  }
  return svg.replace(
    /<svg\b([^>]*)>/i,
    `<svg$1>\n<style>path,circle,rect,polygon,polyline,ellipse{fill:${color};}</style>`,
  );
}

async function renderSvgToPng(svgPath, pngPath, size) {
  const renderers = [
    ["sips", ["-s", "format", "png", "-z", String(size), String(size), svgPath, "--out", pngPath]],
    ["rsvg-convert", ["-w", String(size), "-h", String(size), "-o", pngPath, svgPath]],
    ["magick", [svgPath, "-resize", `${size}x${size}`, pngPath]],
  ];
  const errors = [];
  for (const [command, args] of renderers) {
    try {
      await execFileAsync(command, args);
      return;
    } catch (error) {
      errors.push(`${command}: ${error.message}`);
    }
  }
  throw new Error(
    `Unable to render ${svgPath} to ${pngPath}. Tried sips, rsvg-convert, and magick.\n${errors.join("\n")}`,
  );
}

function htmlShell(jsFile, wasmFile, { includeNodeBridge }) {
  return `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Rings</title>
    <link rel="icon" type="image/svg+xml" href="./icons/rings.svg" />
    <link rel="icon" type="image/png" sizes="16x16" href="./icons/rings-16.png" />
    <link rel="icon" type="image/png" sizes="32x32" href="./icons/rings-32.png" />
    <link rel="apple-touch-icon" href="./icons/rings-128.png" />
    <meta name="theme-color" content="#020c10" />
    <link rel="modulepreload" href="./${jsFile}" />
    <link rel="preload" href="./${wasmFile}" as="fetch" type="application/wasm" />
  </head>
  <body>
    <script type="module" src="./wallet_bridge.js"></script>
    ${includeNodeBridge ? '<script type="module" src="./node_bridge.js"></script>' : ''}
    <script type="module" src="./bootstrap.js"></script>
  </body>
</html>
`;
}

function bootstrapScript(jsFile, wasmFile) {
  return `import init, * as bindings from "./${jsFile}";

const wasm = await init({
  module_or_path: new URL("./${wasmFile}", import.meta.url),
});

globalThis.wasmBindings = bindings;
globalThis.dispatchEvent(new CustomEvent("TrunkApplicationStarted", { detail: { wasm } }));
`;
}

function manifest(version) {
  return {
    manifest_version: 3,
    name: "Rings Frontend",
    short_name: "Rings",
    version,
    description: "Rings browser frontend for WebRTC peer connectivity and Chord topology.",
    minimum_chrome_version: "116",
    icons: {
      16: "icons/rings-16.png",
      32: "icons/rings-32.png",
      48: "icons/rings-48.png",
      128: "icons/rings-128.png",
    },
    action: {
      default_title: "Rings",
      default_icon: {
        16: "icons/rings-disconnected-16.png",
        32: "icons/rings-disconnected-32.png",
        48: "icons/rings-disconnected-48.png",
        128: "icons/rings-disconnected-128.png",
      },
    },
    background: {
      service_worker: "service_worker.js",
    },
    side_panel: {
      default_path: "index.html",
    },
    options_page: "index.html",
    permissions: ["activeTab", "offscreen", "scripting", "sidePanel", "storage"],
    host_permissions: ["http://*/*", "https://*/*"],
    content_security_policy: {
      extension_pages: "script-src 'self' 'wasm-unsafe-eval'; object-src 'self';",
    },
  };
}
