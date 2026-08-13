// Pure browser-gateway transformations. This file has no DOM or network effects.
function gatewayPathFromText(input, prefix, locationOrigin) {
  const text = String(input);
  if (text.startsWith(prefix)) return text;
  try {
    const url = new URL(text);
    if (locationOrigin && url.origin === locationOrigin && url.pathname.startsWith(prefix)) {
      return `${url.pathname}${url.search}${url.hash}`;
    }
  } catch (_error) {}
  return undefined;
}

function isUnrewritableTarget(value) {
  const lower = String(value).trim().toLowerCase();
  return !lower || ["javascript:", "mailto:", "tel:", "data:", "blob:", "about:"].some((scheme) => lower.startsWith(scheme));
}

function createUrlTransformer({ prefix, targetBase, locationOrigin, controlledAssetPaths }) {
  const controlledPaths = new Set(controlledAssetPaths);
  function controlledAssetPath(input) {
    if (!locationOrigin) return undefined;
    try {
      const url = new URL(String(input), locationOrigin);
      if (controlledPaths.has(url.pathname)) return `${url.pathname}${url.search}${url.hash}`;
    } catch (_error) {}
    return undefined;
  }
  function gatewayPath(input) {
    return gatewayPathFromText(input, prefix, locationOrigin);
  }
  function decodeGatewayTarget(input) {
    const path = gatewayPath(input);
    if (!path?.startsWith(prefix)) return undefined;
    const encoded = path.slice(prefix.length);
    if (!encoded) return undefined;
    try {
      return decodeURIComponent(encoded);
    } catch (_error) {
      return undefined;
    }
  }
  function encodeTarget(input, base = targetBase) {
    const text = String(input);
    if (isUnrewritableTarget(text)) return text;
    const controlled = controlledAssetPath(text);
    if (controlled) return controlled;
    const gatewayPath = gatewayPathFromText(text, prefix, locationOrigin);
    if (gatewayPath) return gatewayPath;
    return prefix + encodeURIComponent(new URL(text, base).href);
  }
  return Object.freeze({ gatewayPath, decodeGatewayTarget, encodeTarget });
}

function encodeUrlList(input, base, encodeTarget) {
  return String(input).trim().split(/\s+/).filter(Boolean).map((value) => encodeTarget(value, base)).join(" ");
}

function normalizeSrcsetUrl(url) {
  const quoted = /^(?:"([\s\S]*)"|'([\s\S]*)')$/.exec(url);
  const value = quoted ? (quoted[1] ?? quoted[2]) : url;
  return value.replace(/\\(.)/g, "$1");
}

// HTML's srcset splitting loop: commas terminate an URL token only at its end.
function parseSrcsetCandidates(input) {
  const text = String(input);
  const candidates = [];
  let cursor = 0;
  while (cursor < text.length) {
    while (cursor < text.length && /[\t\n\f\r ,]/.test(text[cursor])) cursor += 1;
    const urlStart = cursor;
    let quote;
    let escaped = false;
    while (cursor < text.length) {
      const character = text[cursor];
      if (escaped) {
        escaped = false;
      } else if (character === "\\") {
        escaped = true;
      } else if (quote) {
        if (character === quote) quote = undefined;
      } else if (character === "'" || character === '"') {
        quote = character;
      } else if (/[\t\n\f\r ]/.test(character)) {
        break;
      }
      cursor += 1;
    }
    let url = text.slice(urlStart, cursor);
    if (url.endsWith(",")) {
      url = normalizeSrcsetUrl(url.replace(/,+$/, ""));
      if (url) candidates.push({ url, descriptor: "" });
      continue;
    }
    while (cursor < text.length && /[\t\n\f\r ]/.test(text[cursor])) cursor += 1;
    const descriptorStart = cursor;
    while (cursor < text.length && text[cursor] !== ",") cursor += 1;
    const descriptor = text.slice(descriptorStart, cursor).trim();
    url = normalizeSrcsetUrl(url);
    if (url) candidates.push({ url, descriptor });
    if (text[cursor] === ",") cursor += 1;
  }
  return candidates;
}

function encodeSrcset(input, base, encodeTarget) {
  return parseSrcsetCandidates(input).map(({ url, descriptor }) => {
    const rewritten = encodeTarget(url, base);
    return descriptor ? `${rewritten} ${descriptor}` : rewritten;
  }).join(", ");
}

function encodeCssText(input, encodeTarget) {
  const imports = String(input).replace(/(@import\s+)(['"])([^'"]+)\2/gi, function(match, importPrefix, quote, value) {
    try {
      return `${importPrefix}${quote}${encodeTarget(value)}${quote}`;
    } catch (_error) {
      return match;
    }
  });
  return imports.replace(/url\(\s*(['"]?)(.*?)\1\s*\)/gi, function(match, quote, value) {
    try {
      const delimiter = quote || "";
      return `url(${delimiter}${encodeTarget(value)}${delimiter})`;
    } catch (_error) {
      return match;
    }
  });
}

function encodeRefreshText(input, encodeTarget) {
  return String(input).replace(/(\burl\s*=\s*)(['"]?)([^'"]+)\2/i, function(match, prefix, quote, value) {
    try {
      const delimiter = quote || "";
      return `${prefix}${delimiter}${encodeTarget(value.trim())}${delimiter}`;
    } catch (_error) {
      return match;
    }
  });
}

function htmlAttributeKind(name) {
  const lower = String(name).toLowerCase();
  if (lower === "srcdoc") return "srcdoc";
  if (lower === "style") return "style";
  if (lower === "ping") return "url-list";
  if (["href", "src", "action", "poster", "data", "cite", "formaction", "manifest", "xlink:href"].includes(lower)) return "url";
  if (["srcset", "imagesrcset"].includes(lower)) return "srcset";
  return "plain";
}

globalThis.__ringsWebviewTransforms = Object.freeze({
  createUrlTransformer,
  encodeCssText,
  encodeRefreshText,
  encodeSrcset,
  encodeUrlList,
  htmlAttributeKind,
  parseSrcsetCandidates,
});
