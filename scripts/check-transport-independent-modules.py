#!/usr/bin/env python3
"""Check that the dummy job may own the transport-independent modules alone.

`scripts/ci-transport-independent-modules.txt` names modules whose tests only the dummy job runs
(see `.github/CI.md`). Law: a test present in the native and the dummy build of its target behaves
the same in both, unless it executes a divergence point, code where the two builds differ.

This script enumerates the divergence points from the builds themselves. `cargo tree` resolves each
build's features: the native job's workspace build, and each dummy target's package build. In every
workspace crate of a target, each `cfg`, `cfg_attr` and `cfg!` predicate is evaluated in both
builds. Where its value differs, in code both builds contain, it marks one of these points:

- an item the builds define differently (a substituted item);
- a statement, expression, match arm or field initializer inside a function;
- a field or variant of a type both builds define;
- an attribute only one build applies;
- an impl one build adds to a type both builds define, or the items of a macro invocation.

An item only one build defines is no point, because code both builds contain cannot name it.
A substituted constant, static or function is a value, and so is every constant, static or
`const fn` that reads a value. Every other function both builds contain that reads a value is a
point (a reader).

`scripts/ci-dummy-divergence.txt` gives each point outside a test function a class. The first
four classes are claims about the point's own code:

- `backend`: it selects the backend, or acts only on values the dummy backend alone produces;
- `transport-only`: it runs only where a transport, processor or swarm exists: its receiver or an
  argument holds one, or only such code constructs its receiver;
- `inert-hook`: the difference is test state that only one build's test code writes, reads or
  arms; unarmed, both builds act alike;
- `failure-bound`: the value only bounds how long a failing wait takes.

The fifth is a claim about callers, so review holds it:

- `unreached`: a reader that takes no transport, and only `transport-only` code and tests outside
  the listed modules reach it.

The check fails on a point without an entry, on an entry without a point, and on a listed module
that contains any divergent code, exists in one build only, or names an `unreached` reader (its
impl's self type, else the function). The dummy job runs the listed modules with
`RINGS_DUMMY_TRANSPORT_FORBIDDEN` set, so a test that constructs a transport aborts. A listed test
therefore reaches no backend or transport-only point, an inert hook and a failure bound act as the
native build does, and it calls no `unreached` reader directly: the listed tests behave the same
in both builds, up to an `unreached` reader called through code outside the listed modules.
"""

import argparse
from dataclasses import dataclass
from dataclasses import field
import itertools
from pathlib import Path
import re
import subprocess
import sys

# Each listed target's crate, as `cargo tree -p` names it.
TARGETS = {"rings_core": "rings-core", "rings_node": "rings-node"}
# The builds of qaci.yml: the native job's workspace build, and each dummy target's package build.
NATIVE_BUILD = ["--workspace"]
DUMMY_FEATURES = ["--features", "dummy"]
CLASSES = {"backend", "transport-only", "inert-hook", "failure-bound", "unreached"}
# Scopes whose direct children are items rather than statements, fields or match arms.
ITEM_SCOPES = {"file", "mod", "impl", "trait", "extern"}
# The predicate of code no `cfg` gates.
ALWAYS = ("all", ())
# The cfgs each crate's build script derives from its features and target, as predicates over
# those. They mirror crates/<crate>/build.rs; the check fails on a cfg a build script emits that
# is missing here.
BUILD_SCRIPT_CFGS = {
    "rings-transport": {
        "rings_transport_backend": 'any(feature = "dummy", feature = "native-webrtc", '
                                   'all(feature = "web-sys-webrtc", target_family = "wasm"))',
        "rings_transport_webrtc": 'any(all(feature = "native-webrtc", not(target_family = "wasm")), '
                                  'all(feature = "web-sys-webrtc", target_family = "wasm"))',
        "rings_transport_runtime": 'any(all(feature = "tokio", not(target_family = "wasm")), '
                                   'all(feature = "web-sys-webrtc", target_family = "wasm"))',
    },
    "rings-node": {
        "rings_browser": 'all(target_family = "wasm", feature = "browser")',
        "rings_native": 'all(not(target_family = "wasm"), feature = "node")',
    },
}

# Lexing: a raw string's opening, a char literal (as opposed to a lifetime), a cfg predicate token.
RAW_STRING = re.compile(r'b?r(#*)"')
CHAR = re.compile(r"'(\\.|\\u\{[0-9a-fA-F]+\}|\\x[0-9a-fA-F]{2}|[^\\'])'")
TOKEN = re.compile(r'\s*(?:(?P<ident>[A-Za-z_]\w*)|(?P<string>"[^"]*")|(?P<punct>[(),=]))')
# Attributes: a `cfg` or `cfg_attr` and its argument, any attribute's path, a test attribute's path.
CFG_ATTRIBUTE = re.compile(r"#!?\[\s*(cfg|cfg_attr)\s*\((.*)\)\s*\]$", re.S)
ATTRIBUTE_PATH = re.compile(r"#!?\[\s*((?:\w+\s*::\s*)*\w+)")
TEST_PATH = re.compile(r"(?:\w+\s*::\s*)*(?:\w+_)?test")
# Items: an identifier, a `fn`, `const` or `static` and its name (not `*const`, `&'static`,
# `const fn` or `static mut`), an item's keyword after its qualifiers, a macro invocation.
IDENT = re.compile(r"[A-Za-z_]\w*")
VALUE_ITEM = re.compile(r"(?<!['*&\w])(fn|const|static)\s+(?!fn\b|mut\b|unsafe\b|async\b|extern\b)([A-Za-z_]\w*)")
ITEM_HEAD = re.compile(
    r"(?:pub(?:\([^)]*\))?\s+)?(?:(?:unsafe|async|const|default|extern\s+\S+)\s+)*"
    r"(fn|const|static|struct|enum|union|type|trait|mod|use|impl|macro_rules!)\b\s*(.*)",
    re.S,
)
MACRO_INVOCATION = re.compile(r"[A-Za-z_][\w:]*!")
# A `mod` declaration's `#[path]`, and the start of a feature predicate.
PATH_ATTRIBUTE = re.compile(r'#\[\s*path\s*=\s*"([^"]+)"\s*\]')
FEATURE_ATOM = re.compile(r'feature\s*=\s*"')


def blank(source: str) -> str:
    """`source` with comments and the contents of string and char literals replaced by spaces.

    Offsets and newlines are kept, so a position in the result is the same position in `source`.
    """
    out, i, n = list(source), 0, len(source)

    def clear(start: int, end: int) -> None:
        for k in range(start, min(end, n)):
            if out[k] != "\n":
                out[k] = " "

    while i < n:
        c = source[i]
        if source.startswith("//", i):
            end = source.find("\n", i)
            end = n if end < 0 else end
            clear(i, end)
            i = end
        elif source.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                step = 2 if source.startswith(("/*", "*/"), j) else 1
                depth += source.startswith("/*", j) - source.startswith("*/", j)
                j += step
            clear(i, j)
            i = j
        elif c in "br" and (raw := RAW_STRING.match(source, i)) and not (
            i and (source[i - 1].isalnum() or source[i - 1] == "_")
        ):
            close = '"' + raw.group(1)
            end = source.find(close, raw.end())
            end = n if end < 0 else end
            clear(raw.end(), end)
            i = end + len(close)
        elif c == '"':
            j = i + 1
            while j < n and source[j] != '"':
                j += 2 if source[j] == "\\" else 1
            clear(i + 1, j)
            i = j + 1
        elif c == "'" and (literal := CHAR.match(source, i)):
            clear(i + 1, literal.end() - 1)
            i = literal.end()
        else:
            i += 1
    return "".join(out)


def group_end(code: str, start: int, opening: str, closing: str) -> int:
    """The offset just past the bracket group that opens at the first `opening` from `start`."""
    j, depth = code.index(opening, start), 0
    while True:
        depth += (code[j] == opening) - (code[j] == closing)
        if depth == 0 and code[j] == closing:
            return j + 1
        j += 1


def parse_predicate(text: str) -> tuple:
    """Parse a cfg predicate into `("atom", name)`, `("not", p)`, `("all", ps)` or `("any", ps)`."""
    tokens, pos = [], 0
    while text[pos:].strip():
        token = TOKEN.match(text, pos)
        if token is None:
            raise ValueError(f"cannot parse cfg predicate: {text!r}")
        tokens.append(token.group("ident") or token.group("string") or token.group("punct"))
        pos = token.end()

    def predicate(k: int) -> tuple[tuple, int]:
        name = tokens[k]
        if name in ("all", "any", "not") and tokens[k + 1 : k + 2] == ["("]:
            k, args = k + 2, []
            while tokens[k] != ")":
                arg, k = predicate(k)
                args.append(arg)
                k += tokens[k] == ","
            return (("not", args[0]) if name == "not" else (name, tuple(args))), k + 1
        if tokens[k + 1 : k + 2] == ["="]:
            return ("atom", f"{name} = {tokens[k + 2]}"), k + 3
        return ("atom", name), k + 1

    result, end = predicate(0)
    if end != len(tokens):
        raise ValueError(f"cannot parse cfg predicate: {text!r}")
    return result


def atoms(predicate: tuple) -> set[str]:
    """The atoms a predicate mentions."""
    if predicate[0] == "atom":
        return {predicate[1]}
    if predicate[0] == "not":
        return atoms(predicate[1])
    return set().union(*map(atoms, predicate[1]))


def holds(predicate: tuple, env: dict) -> bool:
    """Evaluate a predicate; `env` assigns every atom it mentions, a value or a derived predicate."""
    kind, arg = predicate
    if kind == "atom":
        return holds(env[arg], env) if isinstance(env[arg], tuple) else env[arg]
    if kind == "not":
        return not holds(arg, env)
    return (all if kind == "all" else any)(holds(p, env) for p in arg)


def split_top_level(text: str) -> tuple[str, str]:
    """Split `text` at its first comma outside parentheses."""
    depth = 0
    for k, c in enumerate(text):
        depth += (c == "(") - (c == ")")
        if c == "," and depth == 0:
            return text[:k], text[k + 1 :]
    return text, ""


def attribute_meaning(attribute: str) -> tuple[str | None, tuple | None, bool]:
    """(`"cfg"`, `"cfg_attr"` or None; its predicate; whether it marks a test function)."""
    attribute = " ".join(attribute.split())
    if cfg := CFG_ATTRIBUTE.match(attribute):
        if cfg.group(1) == "cfg":
            return "cfg", parse_predicate(cfg.group(2)), False
        predicate, applied = split_top_level(cfg.group(2))
        marks_test = any(TEST_PATH.fullmatch(part.split("(")[0].strip()) for part in applied.split(","))
        return "cfg_attr", parse_predicate(predicate), marks_test
    path = ATTRIBUTE_PATH.match(attribute)
    return None, None, bool(path and TEST_PATH.fullmatch(path.group(1)))


@dataclass
class Frame:
    """An open brace: what it opened, the cfg predicates in force inside it, and the attribute
    group awaiting the next item, statement, field or arm directly inside it."""

    kind: str
    label: str | None
    cond: list
    in_test: bool
    paren: int = 0
    group: list = field(default_factory=list)
    group_test: bool = False

    def end_group(self) -> None:
        """A boundary: the pending attribute group's code has ended."""
        self.group, self.group_test = [], False


@dataclass
class Site:
    """An attribute group or a `cfg!` invocation, and where it sits."""

    line: int
    context: str
    scope: tuple
    ctx: list
    own: list
    attr: list
    head: str
    in_test: bool
    in_parens: bool = False


@dataclass
class Item:
    """A `fn`, `const` or `static`, and the extent of its body or initializer."""

    kind: str
    name: str
    start: int
    end: int
    line: int
    ctx: list
    in_test: bool
    scope: tuple
    const_fn: bool
    visibility: str


@dataclass
class Module:
    """One file of a crate's library module tree and what the walk found in it."""

    path: Path
    cond: list
    source: str
    code: str = ""
    sites: list = field(default_factory=list)
    items: list = field(default_factory=list)
    spans: list = field(default_factory=list)
    inner: list = field(default_factory=list)
    children: list = field(default_factory=list)


def frame_kind(parent: Frame, head: str) -> tuple[str, str | None]:
    """What a `{` opens, given the enclosing frame and the code since the previous boundary."""
    if parent.kind == "fields":
        return "fields", None
    if parent.kind not in ITEM_SCOPES:
        return "body", None
    head = " ".join(re.sub(r"#!?\[[^\]]*\]", " ", head).split())
    if function := re.search(r"\bfn\s+(\w+)", head):
        return "body", f"fn {function.group(1)}"
    if MACRO_INVOCATION.search(head):
        return "body", None
    if module := re.search(r"(?:^|\s)mod\s+(\w+)", head):
        return "mod", f"mod {module.group(1)}"
    for kind in ("impl", "trait"):
        if re.search(rf"(?:^|\s){kind}\b", head):
            return kind, re.sub(r"\s+where\s.*$", "", head[head.index(kind) :])
    if re.search(r"(?:^|\s)extern\b", head):
        return "extern", None
    if shape := re.search(r"(?:^|\s)(struct|enum|union)\s+(\w+)", head):
        return "fields", f"{shape.group(1)} {shape.group(2)}"
    return "body", None


def value_end(code: str, item: re.Match) -> int:
    """The end of the `fn` body, or `const` or `static` initializer, that `item` starts."""
    j, depth = item.end(), 0
    while j < len(code):
        c = code[j]
        depth += (c in "([") - (c in ")]")
        if c == "{" and depth == 0:
            j = group_end(code, j, "{", "}")
            if item.group(1) == "fn":
                return j
            continue
        if c == ";" and depth == 0:
            return j + 1
        j += 1
    return j


def walk(module: Module) -> None:
    """Record a file's attribute groups, `cfg!` invocations, items, `mod` declarations and spans."""
    code, source = module.code, module.source
    frames = [Frame("file", None, list(module.cond), False)]
    group, group_start, head_start, i = [], 0, 0, 0

    def scope() -> tuple:
        return tuple(f.label for f in frames if f.label)

    while i < len(code):
        frame, c = frames[-1], code[i]
        if c == "#" and code.startswith(("#[", "#!["), i):
            end = group_end(code, i, "[", "]")
            kind, predicate, marks_test = attribute_meaning(source[i:end])
            if kind:
                module.spans.append((i, end))
            if code.startswith("#![", i):
                if kind == "cfg":
                    frame.cond.append(predicate)
                    module.inner.append((source.count("\n", 0, i) + 1, predicate))
                frame.in_test |= marks_test
                i = end
                continue
            group_start = group_start if group else i
            group.append((kind, predicate, marks_test))
            i = thing = re.compile(r"\s*").match(code, end).end()
            if code.startswith("#[", thing):
                continue
            own = [p for k, p, _ in group if k == "cfg"]
            module.sites.append(Site(
                line=source.count("\n", 0, group_start) + 1, context=frame.kind, scope=scope(),
                ctx=list(frame.cond), own=own, attr=[p for k, p, _ in group if k == "cfg_attr"],
                head=" ".join(source[thing : source.find("\n", thing)].split()),
                in_test=frame.in_test or any(t for _, _, t in group), in_parens=frame.paren > 0,
            ))
            if frame.paren == 0:
                frame.group, frame.group_test = own, any(t for _, _, t in group)
            group = []
            continue
        if c == "c" and code.startswith("cfg!", i) and not (i and (code[i - 1].isalnum() or code[i - 1] == "_")):
            end = group_end(code, i, "(", ")")
            module.spans.append((i, end))
            module.sites.append(Site(
                line=source.count("\n", 0, i) + 1, context="body", scope=scope(), ctx=list(frame.cond),
                own=[parse_predicate(source[code.index("(", i) + 1 : end - 1])], attr=[],
                head=" ".join(source[i : source.find("\n", i)].split()), in_test=frame.in_test,
            ))
            i = end
            continue
        if c in "fcs" and (item := VALUE_ITEM.match(code, i)):
            if not (item.group(1) == "const" and code[item.end() : item.end() + 1] == "("):
                module.items.append(Item(
                    kind=item.group(1), name=item.group(2), start=i, end=value_end(code, item),
                    line=source.count("\n", 0, i) + 1, ctx=frame.cond + frame.group,
                    in_test=frame.in_test or frame.group_test, scope=scope(),
                    const_fn=item.group(1) == "fn"
                    and bool(re.search(r"\bconst\s+(?:unsafe\s+)?$", code[head_start:i])),
                    visibility=visibility(code[head_start:i]),
                ))
        if c in "([":
            frame.paren += 1
        elif c in ")]":
            frame.paren -= 1
        elif c == "{":
            kind, label = frame_kind(frame, code[head_start:i])
            entering = (frame.group, frame.group_test) if frame.paren == 0 else ([], False)
            frames.append(Frame(kind, label, frame.cond + entering[0], frame.in_test or entering[1]))
            frame.end_group()
            head_start = i + 1
        elif c == "}":
            if len(frames) > 1:
                frames.pop()
            frames[-1].end_group()
            head_start = i + 1
        elif c == ";" and frame.paren == 0:
            declaration = re.search(r"(?:^|\s)mod\s+(\w+)\s*$", code[head_start:i])
            if declaration and frame.kind in ITEM_SCOPES:
                path = PATH_ATTRIBUTE.search(source[head_start:i])
                inline = [f.label.split()[1] for f in frames if f.kind == "mod"]
                module.children.append((declaration.group(1), frame.cond + frame.group, inline, path and path.group(1)))
            frame.end_group()
            head_start = i + 1
        elif c == "," and frame.paren == 0 and frame.kind not in ITEM_SCOPES:
            frame.end_group()
            head_start = i + 1
        i += 1


def visibility(qualifiers: str) -> str:
    """The visibility the qualifiers before an item's keyword give it: `""` (private), `"pub"`,
    or what `pub(...)` restricts it to (`"crate"`, `"super"`, `"self"`, `"in <path>"`)."""
    marker = list(re.finditer(r"\bpub\b\s*(?:\(\s*([^)]*?)\s*\))?", qualifiers))
    return "" if not marker else (" ".join(marker[-1].group(1).split()) if marker[-1].group(1) else "pub")


def library_modules(lib: Path) -> list[Module]:
    """Every file of a crate's library module tree, with the cfg predicates of its declarations."""
    modules, queue = [], [(lib, [])]
    while queue:
        path, cond = queue.pop()
        source = path.read_text()
        module = Module(path, cond, source, blank(source))
        walk(module)
        modules.append(module)
        directory = path.parent if path.name in ("lib.rs", "main.rs", "mod.rs") else path.parent / path.stem
        for name, declared, inline, path_attribute in module.children:
            if path_attribute:
                child = path.parent.joinpath(*inline, path_attribute)
            else:
                child = directory.joinpath(*inline, f"{name}.rs")
                child = child if child.is_file() else directory.joinpath(*inline, name, "mod.rs")
            if not child.is_file():
                raise ValueError(f"{path}: cannot find the file of `mod {name}`")
            queue.append((child, declared))
    return modules


def resolved_features(repo: Path, build: list[str]) -> dict[str, tuple[Path, set[str]]]:
    """Each workspace crate's directory and enabled features in the build `cargo tree` resolves."""
    output = subprocess.check_output(
        ["cargo", "tree", "--locked", "--color", "never", "--prefix", "none", "-e", "normal,dev",
         "-f", "{p}|{f}", *build],
        cwd=repo, text=True,
    )
    crates = {}
    for line in output.splitlines():
        head, _, features = line.removesuffix(" (*)").partition("|")
        package = re.fullmatch(r"(\S+) v\S+ \(([^)]*)\)", head.strip())
        if package and repo in Path(package.group(2)).resolve().parents:
            crate = crates.setdefault(package.group(1), (Path(package.group(2)).resolve(), set()))
            crate[1].update(f for f in features.split(",") if f)
    return crates


@dataclass
class Builds:
    """The atom values one crate has in the native and in the dummy build of one target.

    Atoms that are neither `test` nor a feature (the target, `debug_assertions`, build-script
    cfgs) are the same in both builds, so every assignment of them is tried.
    """

    native: dict
    dummy: dict

    def worlds(self, predicates: list):
        """Each assignment of the free atoms, as the (native, dummy) environments it yields."""
        derived = [value for value in self.native.values() if isinstance(value, tuple)]
        free = sorted(set().union(*map(atoms, predicates + derived)) - self.native.keys())
        for values in itertools.product((False, True), repeat=len(free)):
            yield {**self.native, **dict(zip(free, values))}, {**self.dummy, **dict(zip(free, values))}

    def differ(self, ctx: list, own: list) -> bool:
        """Whether, where `ctx` holds in both builds, `own` holds in exactly one of them."""
        return any(
            all(holds(p, n) and holds(p, d) for p in ctx)
            and all(holds(p, n) for p in own) != all(holds(p, d) for p in own)
            for n, d in self.worlds(ctx + own)
        )

    def shared(self, ctx: list) -> bool:
        """Whether code under `ctx` is present in both builds for some value of the free atoms."""
        return any(all(holds(p, n) and holds(p, d) for p in ctx) for n, d in self.worlds(ctx))


def crate_builds(native: set[str], dummy: set[str], crate: str, target_crate: str, modules: list[Module]) -> Builds:
    """`test`, every feature atom and the build script's cfgs, in each build of one crate."""
    derived = {name: parse_predicate(text) for name, text in BUILD_SCRIPT_CFGS.get(crate, {}).items()}
    predicates = [p for m in modules for s in m.sites for p in s.ctx + s.own + s.attr]
    predicates += [p for m in modules for _, p in m.inner] + [p for m in modules for p in m.cond]
    mentioned = {a for a in set().union(*map(atoms, predicates + list(derived.values()))) if a.startswith("feature = ")}

    def build(features: set[str]) -> dict:
        enabled = {atom: atom[len('feature = "') : -1] in features for atom in mentioned}
        return {"test": crate == target_crate, **derived, **enabled}

    return Builds(build(native), build(dummy))


def item_names(head: str) -> tuple[str | None, set[str]]:
    """The kind of item a site gates and the names it defines; an impl is named by its head."""
    item = ITEM_HEAD.match(head)
    if item is None:
        return ("macro" if MACRO_INVOCATION.match(head) else None), set()
    kind, rest = item.groups()
    if kind == "use":
        return kind, {part.split(" as ")[-1].split("::")[-1].strip() for part in re.split(r"[{},;]", rest)} - {"", "*"}
    if kind == "impl":
        return kind, {"impl " + rest.split("{")[0].strip()}
    name = re.match(r"(?:mut\s+)?(\w+)", rest)
    return kind, {name.group(1)} if name else set()


@dataclass
class Point:
    """Divergent code: its shape, where it is, and whether it needs a ledger entry."""

    shape: str
    file: Path
    line: int
    scope: str
    head: str
    needs_entry: bool
    # A reader's name in callers' code, its crate's source root, and the module subtree that can
    # name it (`None`: any module of any crate).
    namer: tuple | None = None


def scope_label(scope: tuple) -> str:
    """The innermost two named scopes of a site: the middle field of its ledger key."""
    return " / ".join(scope[-2:]) or "-"


def site_points(module: Module, relative: Path, builds: Builds) -> tuple[list[Point], set[str]]:
    """The divergent attribute groups and `cfg!` invocations of one file, and the names of the
    values (substituted constants, statics and functions) it defines."""
    divergent = [
        site for site in module.sites
        if builds.shared(site.ctx)
        and any(builds.differ(site.ctx, own) for own in [site.own, *([p] for p in site.attr)] if own)
    ]
    item_sites = {id(s) for s in divergent if s.context in ITEM_SCOPES and not s.in_parens and not s.attr}
    named = {}
    for site in (s for s in divergent if id(s) in item_sites):
        kind, names = item_names(site.head)
        for name in names:
            named.setdefault((site.scope, kind == "impl", name), []).append(site)
    substituted = {id(s) for sites in named.values() if len(sites) > 1 for s in sites}
    one_build_types = {
        name for s in divergent if id(s) in item_sites and id(s) not in substituted
        for kind, names in [item_names(s.head)] if kind in ("struct", "enum", "union", "type") for name in names
    }
    points, values = [], set()
    for site in divergent:
        kind, names = item_names(site.head) if id(site) in item_sites else (None, set())
        needs_entry = not site.in_test
        if site.attr:
            shape = "attribute"
        elif id(site) in substituted and kind in ("const", "static", "fn"):
            # A value: the functions that read it are the points that need entries.
            shape, needs_entry = "substituted value", False
            values |= names
        elif id(site) in substituted:
            shape = "substituted item"
        elif id(site) in item_sites and kind in ("impl", "macro", None):
            shared_type = not set(IDENT.findall(site.head.split("{")[0])) & one_build_types
            shape = {"impl": "impl on a shared type", "macro": "macro item"}.get(kind, "unrecognized item")
            needs_entry &= kind != "impl" or shared_type
        elif id(site) in item_sites:
            shape, needs_entry = "item in one build only", False
        else:
            shape = {"fields": "field or variant", "body": "statement"}.get(site.context, "parameter or tuple field")
        points.append(Point(shape, relative, site.line, scope_label(site.scope), site.head, needs_entry))
    return points, values


def item_reads(module: Module, builds: Builds):
    """Each item both builds contain, with the identifiers it reads directly: one inside a nested
    item belongs to the innermost one."""
    items = sorted(module.items, key=lambda item: item.start)
    for item in items:
        if not builds.shared(item.ctx):
            continue
        text = list(module.code[item.start : item.end])
        for inner in items:
            if inner is not item and item.start < inner.start and inner.end <= item.end:
                text[inner.start - item.start : inner.end - item.start] = " " * (inner.end - inner.start)
        # `_` names an anonymous constant and also matches every wildcard pattern.
        yield item, set(IDENT.findall("".join(text)[len(item.kind) :])) - {item.name, "_"}


@dataclass
class Target:
    """What the walk of one target's crates found."""

    points: list = field(default_factory=list)
    one_build_files: set = field(default_factory=set)
    problems: list = field(default_factory=list)


def target_points(repo: Path, target_crate: str, native: dict, parsed: dict) -> Target:
    """The divergence points of one target's pair of builds, as `cargo tree` resolves them."""
    dummy = resolved_features(repo, ["-p", target_crate, *DUMMY_FEATURES])
    crates = []
    for crate, (directory, features) in sorted(dummy.items()):
        lib = directory / "src" / "lib.rs"
        if crate not in native or not lib.is_file():
            continue
        if lib not in parsed:
            parsed[lib] = library_modules(lib)
        builds = crate_builds(native[crate][1], features, crate, target_crate, parsed[lib])
        crates.append((parsed[lib], builds, lib.parent))
    found = inventory(repo, crates)
    for crate, (directory, _) in sorted(dummy.items()):
        script = directory / "build.rs"
        emitted = set(re.findall(r"rustc-cfg=(\w+)", script.read_text())) if script.is_file() else set()
        found.problems.extend(f"{script.relative_to(repo)} emits cfg `{name}`; define it in BUILD_SCRIPT_CFGS"
                              for name in sorted(emitted - BUILD_SCRIPT_CFGS.get(crate, {}).keys()))
    return found


def inventory(repo: Path, crates: list[tuple[list[Module], Builds, Path]]) -> Target:
    """The divergence points of the crates of one target, each given with its builds and source
    root: the cfg sites, then the readers of the closure of the substituted values."""
    found, values = Target(), set()
    for modules, builds, _ in crates:
        divergent = {a for a in builds.native if a.startswith("feature = ") and builds.native[a] != builds.dummy[a]}
        for module in modules:
            relative = module.path.relative_to(repo)
            if builds.differ([], module.cond or [ALWAYS]):
                found.one_build_files.add(relative)
            found.problems.extend(
                f"{relative}:{line}: an inner cfg differs between the builds; gate the `mod` declaration instead"
                for line, predicate in module.inner if builds.differ([], [predicate])
            )
            found.problems.extend(f"{relative}:{line}: a feature predicate outside cfg, cfg_attr and cfg!"
                                  for line in unparsed_feature_atoms(module, divergent))
            points, defined = site_points(module, relative, builds)
            found.points.extend(points)
            values |= defined
    reads = [(module, item, read, source_root) for modules, builds, source_root in crates for module in modules
             for item, read in item_reads(module, builds)]
    grew = True
    while grew:
        constants = {item.name for _, item, read, _ in reads
                     if (item.kind in ("const", "static") or item.const_fn) and read & values}
        grew = not constants <= values
        values |= constants
    for module, item, read, source_root in reads:
        if item.kind == "fn" and not item.const_fn and read & values:
            scope = item.scope + (f"fn {item.name}",)
            found.points.append(Point(
                "reader", module.path.relative_to(repo), item.line, scope_label(scope),
                "reads " + ", ".join(sorted(read & values)), not item.in_test,
                namer(item, scope, module.path, source_root),
            ))
    return found


def module_path(source_root: Path, file: Path, scope: tuple) -> list[str]:
    """The module path of code in `file` under the named `scope` labels."""
    parts = list(file.relative_to(source_root).with_suffix("").parts)
    if parts[-1] in ("lib", "main", "mod"):
        parts.pop()
    return parts + [label.split()[1] for label in scope if label.startswith("mod ")]


def namer(item: Item, scope: tuple, file: Path, source_root: Path) -> tuple:
    """How callers name a reader, and which module subtree of its crate can (`None`: any)."""
    if any(label.startswith("impl") for label in scope):
        return reached_name(" / ".join(scope)), source_root, None
    home = module_path(source_root, file, scope)
    subtree = {"": home, "self": home, "super": home[:-1], "crate": []}.get(item.visibility)
    if subtree is None and item.visibility.startswith("in "):
        subtree = item.visibility.removeprefix("in ").removeprefix("crate").strip(":").split("::")
        subtree = [part for part in subtree if part]
    return item.name, source_root, subtree


def unparsed_feature_atoms(module: Module, divergent: set[str]):
    """Lines of divergent feature predicates outside every parsed cfg attribute and `cfg!`."""
    names = {a[len('feature = "') : -1] for a in divergent}
    for atom in FEATURE_ATOM.finditer(module.code):
        literal = module.source[atom.end() : module.source.find('"', atom.end())]
        if literal in names and not any(start <= atom.start() < end for start, end in module.spans):
            yield module.source.count("\n", 0, atom.start()) + 1


def ledger_entries(path: Path) -> dict[tuple, str]:
    """Read `<class> | <file> | <scope> | <code>` lines into {(file, scope, code): class}."""
    entries = {}
    for number, line in enumerate(path.read_text().splitlines(), 1):
        if not line.strip() or line.startswith("#"):
            continue
        parts = [part.strip() for part in line.split(" | ", 3)]
        if len(parts) != 4 or parts[0] not in CLASSES:
            raise ValueError(
                f"{path}:{number}: expected `<class> | <file> | <scope> | <code>`, class one of {sorted(CLASSES)}"
            )
        if tuple(parts[1:]) in entries:
            raise ValueError(f"{path}:{number}: duplicate entry")
        entries[tuple(parts[1:])] = parts[0]
    return entries


def listed_modules(path: Path) -> list[tuple[str, str]]:
    """Read `<target> <module>` lines, ignoring comments and blank lines."""
    lines = [line.strip() for line in path.read_text().splitlines()]
    return [tuple(line.split(" ", 1)) for line in lines if line and not line.startswith("#")]


def module_files(root: Path, module: str) -> list[Path]:
    """The files of a file-backed module: `a/b.rs` and everything under `a/b/`."""
    *parents, name = module.split("::")
    directory = root.joinpath(*parents)
    files = [directory / f"{name}.rs"] if (directory / f"{name}.rs").is_file() else []
    if (directory / name).is_dir():
        files.extend(sorted((directory / name).rglob("*.rs")))
    if not files:
        raise ValueError(f"listed module has no file of its own (inline modules cannot be listed): {module}")
    return files


def keyed(points: list[Point]) -> dict[tuple, Point]:
    """The points that need a ledger entry, by key; a repeated key gains ` #2`, ` #3`, ..."""
    keys = {}
    for point in sorted(points, key=lambda p: (str(p.file), p.line, p.head)):
        if not point.needs_entry:
            continue
        base = (str(point.file), point.scope, point.head)
        key, copy = base, 1
        while key in keys and keys[key].line != point.line:
            copy += 1
            key = base[:2] + (f"{base[2]} #{copy}",)
        keys[key] = point
    return keys


def check(repo: Path, modules_path: Path, ledger_path: Path) -> list[str]:
    """Return one message per violation."""
    native, parsed = resolved_features(repo, NATIVE_BUILD), {}
    targets = {target: target_points(repo, crate, native, parsed) for target, crate in TARGETS.items()}
    problems = [problem for found in targets.values() for problem in found.problems]
    points = keyed([point for found in targets.values() for point in found.points])
    ledger = ledger_entries(ledger_path)
    problems += [
        f"{point.file}:{point.line}: unclassified {point.shape}; add `<class> | {' | '.join(key)}` to {ledger_path}"
        for key, point in points.items() if key not in ledger
    ]
    problems += [f"{ledger_path}: `{' | '.join(key)}` matches no divergence point"
                 for key in ledger.keys() - points.keys()]
    unreached = [points[key].namer for key, class_ in ledger.items() if class_ == "unreached" and key in points]
    for target, module in listed_modules(modules_path):
        if target in targets:
            problems += listed_module_problems(repo, target, module, targets[target], unreached)
        else:
            problems.append(f"unknown target {target!r} for module {module}")
    return problems


def listed_module_problems(repo: Path, target: str, module: str, found: Target, unreached: list) -> list[str]:
    """Why one listed module may not be owned by the dummy job alone, if it may not."""
    source_root = repo / "crates" / TARGETS[target].removeprefix("rings-") / "src"
    try:
        files = {file.relative_to(repo) for file in module_files(source_root, module)}
    except ValueError as error:
        return [f"{target} {module}: {error}"]
    problems = [f"{target} {module}: {file} exists in one build only" for file in sorted(files & found.one_build_files)]
    problems += [f"{target} {module}: {point.file}:{point.line} is a divergent {point.shape}"
                 for point in found.points if point.file in files]
    names = {
        name for name, root, subtree in unreached
        if subtree is None or (root == source_root and module.split("::")[: len(subtree)] == subtree)
    }
    for file in sorted(files):
        named = names & set(IDENT.findall(blank((repo / file).read_text())))
        problems += [f"{target} {module}: {file} names {name}, an `unreached` reader of a value"
                     for name in sorted(named)]
    return problems


def reached_name(scope: str) -> str:
    """The name a caller of an `unreached` reader uses: its impl's self type, else the function."""
    labels = scope.split(" / ")
    impl = next((label for label in labels if label.startswith("impl")), None)
    if impl is None:
        return labels[-1].removeprefix("fn ")
    rest = impl.removeprefix("impl").strip()
    if rest.startswith("<"):
        rest = rest[group_end(rest, 0, "<", ">") :].strip()
    rest = rest.split(" for ", 1)[-1]
    return re.match(r"[\w:]+", rest).group().split("::")[-1]


def main() -> None:
    """Report every violation and fail if there is one."""
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("modules", type=Path, help="The `<target> <module>` list")
    parser.add_argument("ledger", type=Path, help="The `<class> | <file> | <scope> | <code>` ledger")
    args = parser.parse_args()
    repo = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
    problems = check(repo, args.modules, args.ledger)
    for problem in problems:
        print(problem, file=sys.stderr)
    if problems:
        sys.exit(1)


if __name__ == "__main__":
    main()
