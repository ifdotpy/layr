#!/usr/bin/env bash
# Compile the binary as a temporary library so rustdoc checks its real types.
set -euo pipefail
cd "$(dirname "$0")/../.."
python3 - <<'PY'
import json, pathlib, re, shutil, subprocess
root = pathlib.Path.cwd()
scratch = root / "target" / "catalog-doctests"
scratch.mkdir(parents=True, exist_ok=True)
manifest = (root / "Cargo.toml").read_text()
manifest, count = re.subn(r"members = \[[^\]]*\]", "members = []", manifest, count=1)
if count != 1:
    raise SystemExit("workspace members not found")
manifest = manifest.replace('[[bin]]\nname = "layr"\npath = "src/main.rs"', '[lib]\npath = "src/main.rs"')
manifest = manifest.replace('[package]\n', '[package]\nautobins = false\n')
manifest = manifest.replace('path = "crates/', f'path = "{root}/crates/')
(scratch / "Cargo.toml").write_text(manifest)
shutil.copyfile(root / "Cargo.lock", scratch / "Cargo.lock")
for name in ["src", "build.rs"]:
    if not (scratch / name).exists():
        (scratch / name).symlink_to(root / name)
command = ["cargo", "--offline"]
options = [ "--manifest-path", str(scratch / "Cargo.toml"), "--target-dir", str(root / "target"), "--lib"]
build = subprocess.run(command + ["rustc"] + options + ["--message-format=json", "--", "--cfg", "layr_doctest"],
                       stdout=subprocess.PIPE, text=True, check=True)
messages = [json.loads(line) for line in build.stdout.splitlines()]
artifacts = [path for item in messages if item.get("reason") == "compiler-artifact" and item["target"]["name"] == "layr"
             for path in item["filenames"] if path.endswith(".rlib")]
dependencies = {str(pathlib.Path(path).parent) for item in messages if item.get("reason") == "compiler-artifact"
                for path in item["filenames"]}
if len(artifacts) != 1:
    raise SystemExit("expected one doctest library artifact")
# rustdoc's examples import the actual application types from the compiled library.
docs = []
for source in (root / "src").rglob("*.rs"):
    for block in re.findall(r"(?m)(?:^///[^\n]*\n)+", source.read_text()):
        if "```" in block:
            docs.append(block + f"pub struct Example{len(docs)};\n")
examples = scratch / "examples.rs"
examples.write_text("\n".join(docs))
subprocess.run(["rustdoc", "--edition", "2021", "--test", str(examples), "--extern", f"layr={artifacts[0]}",
                *[flag for path in sorted(dependencies) for flag in ["-L", f"dependency={path}"]]], check=True)
PY
