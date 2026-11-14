# Workspace checks

Run these from the repository root with the pinned Rust toolchain and Python 3:

```sh
python3 tools/test-check-clippy.py
python3 tools/check-clippy.py
python3 tools/check-layers.py
cargo deny check
taplo fmt --check
lychee --no-progress 'docs/*.md' README.md
ast-grep scan
tokei # reporting only
```

The Clippy baseline records diagnostic counts from Linux, all workspace targets and all features. Reduce a package's ceiling when fixing a warning; new packages start at zero. The check caps lint levels for this invocation so existing denied diagnostics can be counted, while normal builds retain workspace lint levels. Platform-specific code needs its own baseline and CI job before its counts can be ratcheted.

Declare every package and its allowed direct internal dependencies in `workspace.metadata.layers`. Leaf packages reject heavy transitive dependencies; exceptional packages can declare `allow-heavy`. The metadata check considers all features and target-specific dependencies conservatively. The link check runs offline and validates local links; its cache is local and ignored by version control.
