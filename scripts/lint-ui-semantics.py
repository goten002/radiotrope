#!/usr/bin/env python3
"""Semantics lint for the Slint UI: keeps new controls from shipping
mouse-only or unnamed.

Two rules, checked in every .slint file under crates/radiotrope-app/ui:

  touch  A component that has a TouchArea must set `accessible-role`
         somewhere in its body: something clickable is a control, and a
         screen reader has to be told which kind. An empty `TouchArea { }`
         (one that only swallows clicks) doesn't count. The check is per
         component, so it finds a control with no semantics at all, not a
         second click area inside a component that has some.

  image  An `Image` must set `accessible-role` or `accessible-label` itself.
         Slint gives every Image the `image` role, so an unnamed one is read
         as "image". Decoration gets `accessible-role: none`.

Exceptions are listed in scripts/ui-semantics-allow.txt, one per line:

  <file under ui/> <Component> <touch|image>   # why

An entry that no longer matches anything fails too, so the list stays short.

Usage: scripts/lint-ui-semantics.py [--root DIR] [--allow FILE]
Exit status: 0 clean, 1 findings, 2 bad allow-list or arguments.
"""

import argparse
import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DEFAULT_ROOT = REPO / "crates" / "radiotrope-app" / "ui"
DEFAULT_ALLOW = Path(__file__).resolve().parent / "ui-semantics-allow.txt"

COMPONENT = re.compile(r"\bcomponent\s+([A-Za-z_][\w-]*)[^{};]*\{")
TOUCH_AREA = re.compile(r"\bTouchArea\s*\{")
IMAGE = re.compile(r"\bImage\s*\{")
# `none` hides an element; it doesn't make a control
ROLE = re.compile(r"\baccessible-role\s*:(?!\s*none\b)")
ROLE_OR_LABEL = re.compile(r"\baccessible-(?:role|label)\s*:")


def string_end(text, start):
    """Index just past the string that opens with the `"` at start. A
    backslash-brace interpolation inside it is code, and may hold strings
    of its own."""
    i = start + 1
    n = len(text)
    while i < n:
        if text[i] == "\\" and text[i + 1 : i + 2] == "{":
            depth = 1
            i += 2
            while i < n and depth > 0:
                if text[i] == '"':
                    i = string_end(text, i)
                    continue
                if text[i] == "{":
                    depth += 1
                elif text[i] == "}":
                    depth -= 1
                i += 1
        elif text[i] == "\\":
            i += 2
        elif text[i] == '"':
            return i + 1
        else:
            i += 1
    return n


def blank_comments_and_strings(text):
    """The text with comments and strings turned into spaces (line breaks
    kept), so braces and names inside them are not seen as code."""
    out = []
    i = 0
    n = len(text)
    while i < n:
        two = text[i : i + 2]
        if two == "//":
            end = text.find("\n", i)
            end = n if end < 0 else end
        elif two == "/*":
            end = text.find("*/", i + 2)
            end = n if end < 0 else end + 2
        elif text[i] == '"':
            end = string_end(text, i)
        else:
            out.append(text[i])
            i += 1
            continue
        out.append("".join(c if c == "\n" else " " for c in text[i:end]))
        i = end
    return "".join(out)


def block_end(code, open_brace):
    """Index just past the `}` that closes the `{` at open_brace."""
    depth = 0
    for i in range(open_brace, len(code)):
        if code[i] == "{":
            depth += 1
        elif code[i] == "}":
            depth -= 1
            if depth == 0:
                return i + 1
    return len(code)


def own_level(code, open_brace, end):
    """The block's text without its nested blocks: its own properties."""
    out = []
    depth = 0
    for i in range(open_brace, end):
        c = code[i]
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
        elif depth == 1:
            out.append(c)
    return "".join(out)


def line_of(code, index):
    return code.count("\n", 0, index) + 1


def check_file(path, rel):
    """Findings in one file, as (rel, line, component, kind, message)."""
    code = blank_comments_and_strings(path.read_text(encoding="utf-8"))
    findings = []
    pos = 0
    while True:
        match = COMPONENT.search(code, pos)
        if not match:
            break
        name = match.group(1)
        start = match.end() - 1
        end = block_end(code, start)
        body = code[start:end]
        pos = end

        touches = [
            m
            for m in TOUCH_AREA.finditer(body)
            if body[m.end() : block_end(body, m.end() - 1) - 1].strip()
        ]
        if touches and not ROLE.search(body):
            findings.append(
                (
                    rel,
                    line_of(code, start + touches[0].start()),
                    name,
                    "touch",
                    "TouchArea in a component with no accessible-role",
                )
            )

        for m in IMAGE.finditer(body):
            image_open = m.end() - 1
            image_end = block_end(body, image_open)
            if not ROLE_OR_LABEL.search(own_level(body, image_open, image_end)):
                findings.append(
                    (
                        rel,
                        line_of(code, start + m.start()),
                        name,
                        "image",
                        "Image with no accessible-role or accessible-label",
                    )
                )
    return findings


def read_allow(path):
    """The allow-list as {(file, component, kind): line number}."""
    allowed = {}
    if not path.exists():
        return allowed
    for number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        parts = line.split()
        if len(parts) != 3 or parts[2] not in ("touch", "image"):
            raise ValueError(f"{path}:{number}: expected '<file> <Component> <touch|image>'")
        allowed[tuple(parts)] = number
    return allowed


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT, help="folder with the .slint files")
    parser.add_argument("--allow", type=Path, default=DEFAULT_ALLOW, help="the allow-list")
    args = parser.parse_args()
    if not args.root.is_dir():
        parser.error(f"{args.root} is not a folder")

    try:
        allowed = read_allow(args.allow)
    except ValueError as error:
        print(error, file=sys.stderr)
        return 2

    used = set()
    failures = []
    for path in sorted(args.root.rglob("*.slint")):
        rel = path.relative_to(args.root).as_posix()
        for finding in check_file(path, rel):
            key = (finding[0], finding[2], finding[3])
            if key in allowed:
                used.add(key)
            else:
                failures.append(finding)

    for rel, line, name, kind, message in failures:
        print(f"{args.root / rel}:{line}: {name}: {message} [{kind}]")
    stale = sorted(set(allowed) - used, key=lambda key: allowed[key])
    for key in stale:
        print(f"{args.allow}:{allowed[key]}: nothing to allow any more: {' '.join(key)}")

    if failures or stale:
        print(
            f"\n{len(failures)} finding(s), {len(stale)} stale allow-list line(s). "
            "Give the control a role (or the image a role or label), "
            f"or list it in {args.allow.name} with the reason.",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
