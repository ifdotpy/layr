#!/usr/bin/env python3
"""Guard the command, process and receive cuts, including Rust macro token trees."""

import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
# Mask parser-identified literals/comments and test items before inspecting paths.
# Macro token trees remain visible, including grouped imports inside definitions.
EXCLUSIONS = r"""
id: syntax
language: Rust
rule:
  kind: 'string_literal,raw_string_literal,char_literal,line_comment,block_comment'
---
id: attached-attribute
language: Rust
rule:
  kind: attribute_item
---
id: cfg-attribute
language: Rust
rule:
  pattern:
    context: '#[cfg($$$CFG)] fn example() {}'
    selector: attribute_item
---
id: configured-item
language: Rust
rule:
  all:
    - kind: 'mod_item,function_item,impl_item,use_declaration,macro_definition,type_item,struct_item,enum_item,trait_item,const_item,static_item'
    - follows:
        kind: attribute_item
        stopBy:
          not:
            any:
              - kind: attribute_item
              - kind: line_comment
              - kind: block_comment
"""


def production_possible(condition):
    # Three-valued cfg evaluation: test is false; other target/feature atoms are
    # unknown. Exclude only a condition that is certainly false in production.
    tokens = [match[0] for match in re.finditer(
        r'r(?P<hashes>#*)".*?"(?P=hashes)|"(?:\\.|[^"\\])*"|(?:r#)?\w+|[(),=]', condition, re.S
    )]
    position = 0

    def predicate():
        nonlocal position
        name = tokens[position].removeprefix("r#")
        position += 1
        if position < len(tokens) and tokens[position] == "=":
            position += 2  # A configuration key and its string value are unknown.
            return None
        if position == len(tokens) or tokens[position] != "(":
            return {"test": False, "true": True, "false": False}.get(name)
        position += 1
        values = []
        while tokens[position] != ")":
            values.append(predicate())
            if tokens[position] != ",":
                break
            position += 1
        if tokens[position] != ")":
            raise ValueError("unrecognized cfg expression")
        position += 1
        if name == "not" and len(values) == 1:
            return None if values[0] is None else not values[0]
        if name == "all":
            return False if False in values else None if None in values else True
        if name == "any":
            return True if True in values else None if None in values else False
        return None

    try:
        value = predicate()
        if tokens[position:] == [","]:
            position += 1
        return position != len(tokens) or value is not False
    except (IndexError, ValueError):
        return True  # Unknown syntax cannot justify hiding a production edge.


def root_paths(source):
    for match in re.finditer(r"\bcrate\s*::\s*((?:r#)?\w+|[{*])", source):
        if match[1] != "{":
            yield match.start(), match[1].removeprefix("r#")
            continue
        # Anonymous groups retain the root namespace; named prefixes leave it.
        groups = [[True, True]]  # [root namespace, next name starts an item]
        for token in re.findall(r"(?:r#)?\w+|::|[{},*]", source[match.end():]):
            if token == "{":
                at_root = groups[-1][1]
                groups.append([at_root, at_root])
            elif token == "}":
                groups.pop()
                if not groups:
                    break
                groups[-1][1] = False
            elif token == ",":
                groups[-1][1] = groups[-1][0]
            elif token != "::":
                if groups[-1][1]:
                    yield match.start(), token.removeprefix("r#")
                groups[-1][1] = False


def violations(directory):
    directory = Path(directory)
    sources = {}
    for path in (directory / "src").rglob("*.rs"):
        relative = path.relative_to(directory)
        if relative.parts[1] == "cmd" or relative == Path("src/main.rs") or relative.stem.endswith("_tests"):
            continue
        sources[str(relative)] = bytearray(path.read_bytes())
    result = subprocess.run(
        ["ast-grep", "scan", "--inline-rules", EXCLUSIONS, "--json=compact", "src"],
        cwd=directory, capture_output=True, text=True, check=False,
    )
    if result.returncode not in (0, 1):
        raise RuntimeError(result.stderr)
    matches = json.loads(result.stdout)
    # Remove comments before evaluating cfg, while preserving its string values.
    for match in matches:
        data = sources.get(match["file"])
        if data is not None and match["ruleId"] == "syntax" and match["text"].startswith(("//", "/*")):
            span = match["range"]["byteOffset"]
            a, b = span["start"], span["end"]
            data[a:b] = bytes(10 if byte == 10 else 32 for byte in data[a:b])
    attributes, conditions = {}, {}
    for match in matches:
        data = sources.get(match["file"])
        if data is None:
            continue
        span = match["range"]["byteOffset"]
        if match["ruleId"] == "attached-attribute":
            attributes[match["file"], span["end"]] = span["start"]
        elif match["ruleId"] == "cfg-attribute":
            parts = match["metaVariables"]["multi"]["CFG"]
            condition = data[parts[0]["range"]["byteOffset"]["start"]:parts[-1]["range"]["byteOffset"]["end"]] if parts else b""
            conditions[match["file"], span["start"]] = production_possible(condition.decode())
    # Resolve every attachment against intact item bodies before masking any item.
    masked_ranges = []
    for match in matches:
        data = sources.get(match["file"])
        if data is None or match["ruleId"] not in ("syntax", "configured-item"):
            continue
        span = match["range"]["byteOffset"]
        a, b = span["start"], span["end"]
        if match["ruleId"] == "configured-item":
            cursor = a
            while True:
                while cursor and data[cursor - 1] in b" \t\r\n":
                    cursor -= 1
                start = attributes.get((match["file"], cursor))
                if start is None:
                    break
                if not conditions.get((match["file"], start), True):
                    masked_ranges.append((match["file"], a, b))
                    break
                cursor = start
        else:
            masked_ranges.append((match["file"], a, b))
    for name, a, b in masked_ranges:
        data = sources[name]
        data[a:b] = bytes(10 if byte == 10 else 32 for byte in data[a:b])
    found = []
    for name, data in sources.items():
        source = data.decode("latin1")  # Preserve the parser's byte offsets.
        forbidden = {"cmd"}
        if name == "src/sys.rs":
            forbidden.add("pools")
        if name == "src/apfs_stream.rs":
            forbidden.add("recv")
        for offset, module in root_paths(source):
            if module in forbidden or module == "*":
                found.append(f"{name}:{source.count(chr(10), 0, offset) + 1}: forbidden dependency on crate::{module}")
    return found


def main():
    cases = [
        ("src/domain.rs", "fn f() { matches!(x, crate::cmd::X); }", 1),
        ("src/domain.rs", "fn f() { crate::cmd::run(); }", 1),
        ("src/domain.rs", "use crate::cmd::{exit, here};", 1),
        ("src/domain.rs", "use crate::{repo, cmd::Exit};", 1),
        ("src/domain.rs", "use crate :: {cmd};", 1),
        ("src/domain.rs", '#[cfg(test)] mod tests {} #[cfg(not(test))] mod prod { use crate::cmd; }', 1),
        ("src/domain.rs", '#[cfg(test)] mod tests { use crate::cmd; } #[cfg(unix)] mod prod { use crate::cmd; }', 1),
        ("src/domain.rs", '#[cfg(test)] mod tests {} #[doc = "prod"] mod prod { use crate::cmd; }', 1),
        ("src/domain.rs", '#[cfg(test)] mod tests {} #[inline] fn prod() { crate::cmd::run(); }', 1),
        ("src/domain.rs", '#[cfg(test)] mod tests {} #[cfg(unix)] mod a { use crate::cmd; } '
         '#[doc = "prod"] mod b { use crate::cmd; } #[inline] fn c() { crate::cmd::run(); }', 3),
        ("src/domain.rs", '#[cfg(test)] type Error = crate::cmd::Exit;', 0),
        ("src/domain.rs", '#[cfg(test)] macro_rules! m { () => { use crate::cmd; } }', 0),
        ("src/domain.rs", '#[cfg(test)] struct Error(crate::cmd::Exit);', 0),
        ("src/domain.rs", '#[cfg(test)] #[cfg(unix)] mod tests { use crate::cmd; }', 0),
        ("src/domain.rs", '#[cfg(all(/* note */ unix, test))] mod tests { use crate::cmd; }', 0),
        ("src/domain.rs", '#[cfg(all(/* test */ not(test), unix))] mod prod { use crate::cmd; }', 1),
        ("src/domain.rs", '#[cfg(all(feature = r#"test-utils"#, test))] mod tests { use crate::cmd; }', 0),
        ("src/domain.rs", '#[doc = "cfg(test)"] mod prod { use crate::cmd; }', 1),
        ("src/domain.rs", '#[cfg_attr(feature = "test-utils", cfg(test))] mod prod { use crate::cmd; }', 1),
        ("src/domain.rs", '#[cfg(all(unix, not(test)))] mod prod { use crate::cmd; }', 1),
        ("src/domain.rs", '#[cfg(any(unix, test))] mod prod { use crate::cmd; }', 1),
        ("src/domain.rs", '#[cfg(test,)] mod tests { use crate::cmd; }', 0),
        ("src/domain.rs", '#[cfg(all(unix, test))] mod tests { use crate::cmd; }', 0),
        ("src/domain.rs", '#[cfg(all(not(windows), test,))] mod tests { use crate::cmd; }', 0),
        ("src/domain.rs", '#[cfg(any(all(unix, test), all(windows, test)))] mod tests { use crate::cmd; }', 0),
        ("src/domain.rs", '#[cfg(all(feature = "test-utils", test))] mod tests { use crate::cmd; }', 0),
        ("src/domain.rs", "use crate::r#cmd::Exit;", 1),
        ("src/domain.rs", "use crate::{r#cmd};", 1),
        ("src/domain.rs", "use crate::{{cmd}};", 1),
        ("src/domain.rs", "use crate::{repo::{cmd}, {{r#cmd as cli}}, ids};", 1),
        ("src/domain.rs", "use crate::*;", 1),
        ("src/domain.rs", "#[cfg(test)] #[allow(dead_code)] mod tests { use crate::cmd; }", 0),
        ("src/domain.rs", "#[cfg (test)] mod tests { use crate::cmd; }", 0),
        ("src/domain.rs", "#[cfg(all(test, unix))] #[allow(dead_code)] mod tests { use crate::cmd; }", 0),
        ("src/domain.rs", "macro_rules! m { () => { use crate::{cmd}; } }", 1),
        ("src/domain.rs", "#[cfg(test)] mod tests { fn f() { crate::cmd::run(); } }", 0),
        ("src/domain.rs", '#[cfg(all(test, target_os="linux"))] mod tests { fn f() { crate::cmd::run(); } }', 0),
        ("src/domain.rs", 'fn f() { println!("crate::cmd::run()"); }', 0),
        ("src/domain.rs", 'fn f() { let _ = r#"crate::{cmd}"#; /* crate::cmd */ }', 0),
        ("src/domain.rs", "use crate::{repo::{cmd}, ids};", 0),
        ("src/cmd/example.rs", "fn f() { crate::cmd::run(); }", 0),
        ("src/sys.rs", "use crate::{ids, pools};", 1),
        ("src/apfs_stream.rs", "use crate::{ids, recv};", 1),
    ]
    with tempfile.TemporaryDirectory(prefix="command-cycle-guard-", dir=os.environ.get("TMPDIR")) as directory:
        for name, source, expected in cases:
            path = Path(directory, name)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(source)
            if len(violations(directory)) != expected:
                raise AssertionError(f"unexpected guard result for {source}")
            path.unlink()
    found = violations(ROOT)
    for message in found:
        print(message)
    if found:
        raise SystemExit(1)
    print(f"{len(cases)} guard fixtures passed; command-cycle cuts hold")


if __name__ == "__main__":
    main()
