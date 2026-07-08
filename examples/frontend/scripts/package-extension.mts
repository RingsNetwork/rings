#!/usr/bin/env node

/**
 * Packages the Trunk web build as a Chrome Manifest V3 extension.
 */

import { execFile } from "node:child_process";
import { cp, mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

/**
 * One icon output variant derived from the source Rings SVG.
 */
type IconState = {
  readonly file: string;
  readonly color: string;
};

/**
 * Options for generating extension HTML shells.
 */
type HtmlShellOptions = {
  readonly includeNodeBridge: boolean;
};

/**
 * Manifest V3 structure emitted for the packaged browser extension.
 */
type ChromeManifest = {
  readonly manifest_version: 3;
  readonly name: string;
  readonly short_name: string;
  readonly version: string;
  readonly description: string;
  readonly minimum_chrome_version: string;
  readonly icons: Record<string, string>;
  readonly action: {
    readonly default_title: string;
    readonly default_icon: Record<string, string>;
  };
  readonly background: {
    readonly service_worker: string;
  };
  readonly side_panel: {
    readonly default_path: string;
  };
  readonly options_page: string;
  readonly permissions: readonly string[];
  readonly host_permissions: readonly string[];
  readonly content_security_policy: {
    readonly extension_pages: string;
  };
};

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
} as const satisfies Record<string, IconState>;

let sourceRoot = sourceDist;
let files = await readdir(sourceRoot);
if (!files.some((file: string): boolean => file.endsWith(".js"))) {
  const stageDist = join(sourceDist, ".stage");
  const stageFiles = await readdir(stageDist).catch((): string[] => []);
  if (stageFiles.some((file: string): boolean => file.endsWith(".js"))) {
    sourceRoot = stageDist;
    files = stageFiles;
  }
}
const sourceHtml = await readFile(join(sourceRoot, "index.html"), "utf8").catch((): string => "");
const jsFile =
  entryFileFromHtml(sourceHtml, files, /import\s+init[\s\S]*?from\s+['"]([^'"]+\.js)['"]/) ??
  entryFileFromHtml(sourceHtml, files, /<link[^>]+rel="modulepreload"[^>]+href="([^"]+\.js)"/) ??
  singleFile(files, (file: string): boolean => file.endsWith(".js"), "generated JS bundle");
const wasmFile =
  entryFileFromHtml(sourceHtml, files, /module_or_path:\s*['"]([^'"]+_bg\.wasm)['"]/) ??
  entryFileFromHtml(sourceHtml, files, /<link[^>]+rel="preload"[^>]+href="([^"]+_bg\.wasm)"/) ??
  singleFile(files, (file: string): boolean => file.endsWith("_bg.wasm"), "generated wasm bundle");

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

/**
 * Finds exactly one generated file matching a predicate.
 */
function singleFile(
  fileList: readonly string[],
  predicate: (file: string) => boolean,
  label: string,
): string {
  const matches = fileList.filter(predicate);
  if (matches.length !== 1) {
    throw new Error(`Expected one ${label} in ${sourceRoot}, found ${matches.length}`);
  }
  const [match] = matches;
  if (!match) {
    throw new Error(`Expected one ${label} in ${sourceRoot}, found none`);
  }
  return match;
}

/**
 * Extracts a generated asset filename from Trunk HTML when available.
 */
function entryFileFromHtml(html: string, fileList: readonly string[], pattern: RegExp): string | undefined {
  const match = html.match(pattern);
  const rawFile = match?.[1];
  if (!rawFile) {
    return undefined;
  }
  const file = rawFile.replace(/^\.\//, "").replace(/^\//, "");
  return fileList.includes(file) ? file : undefined;
}

/**
 * Converts a Cargo semver string into the Chrome extension version format.
 */
function chromeVersion(version: string): string {
  const parts = version.split(".").map((part: string): string => part.replace(/\D.*$/, "") || "0");
  while (parts.length < 3) {
    parts.push("0");
  }
  return parts.slice(0, 4).join(".");
}

/**
 * Writes all extension icon variants into the package directory.
 */
async function writeExtensionIcons(): Promise<void> {
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

/**
 * Recolors the source SVG while preserving its shape.
 */
function tintIconSvg(svg: string, color: string): string {
  const tinted = svg.replace(/\sfill="(?!none\b)[^"]*"/gi, ` fill="${color}"`);
  if (tinted !== svg) {
    return tinted;
  }
  return svg.replace(
    /<svg\b([^>]*)>/i,
    `<svg$1>\n<style>path,circle,rect,polygon,polyline,ellipse{fill:${color};}</style>`,
  );
}

/**
 * Renders one SVG icon to PNG using the first available local renderer.
 */
async function renderSvgToPng(svgPath: string, pngPath: string, size: number): Promise<void> {
  const renderers: ReadonlyArray<readonly [string, readonly string[]]> = [
    ["sips", ["-s", "format", "png", "-z", String(size), String(size), svgPath, "--out", pngPath]],
    ["rsvg-convert", ["-w", String(size), "-h", String(size), "-o", pngPath, svgPath]],
    ["magick", [svgPath, "-resize", `${size}x${size}`, pngPath]],
  ];
  const errors: string[] = [];
  for (const [command, args] of renderers) {
    try {
      await execFileAsync(command, [...args]);
      return;
    } catch (error: unknown) {
      errors.push(`${command}: ${errorMessage(error)}`);
    }
  }
  throw new Error(
    `Unable to render ${svgPath} to ${pngPath}. Tried sips, rsvg-convert, and magick.\n${errors.join("\n")}`,
  );
}

/**
 * Builds the extension page HTML without inline scripts so MV3 CSP accepts it.
 */
function htmlShell(jsFileName: string, wasmFileName: string, options: HtmlShellOptions): string {
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
    <link rel="modulepreload" href="./${jsFileName}" />
    <link rel="preload" href="./${wasmFileName}" as="fetch" type="application/wasm" />
  </head>
  <body>
    <script type="module" src="./wallet_bridge.js"></script>
    ${options.includeNodeBridge ? '<script type="module" src="./node_bridge.js"></script>' : ''}
    <script type="module" src="./bootstrap.js"></script>
  </body>
</html>
`;
}

/**
 * Builds the external bootstrap module that loads wasm-bindgen output.
 */
function bootstrapScript(jsFileName: string, wasmFileName: string): string {
  return `import init, * as bindings from "./${jsFileName}";

const wasm = await init({
  module_or_path: new URL("./${wasmFileName}", import.meta.url),
});

globalThis.wasmBindings = bindings;
globalThis.dispatchEvent(new CustomEvent("TrunkApplicationStarted", { detail: { wasm } }));
`;
}

/**
 * Builds the Chrome Manifest V3 JSON payload.
 */
function manifest(version: string): ChromeManifest {
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

/**
 * Converts an unknown renderer failure into a message string.
 */
function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
