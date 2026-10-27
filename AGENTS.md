# Agent rules for layr

- Do not create Git worktrees for this repository. Develop in layr lines:
  `layr line new <task> main`, commit with `layr add <files>` and `layr save -m`, and publish
  with `layr git push`. In Studio, use the Layr workspace mode for chats of this project.
- Never run layr tests as root, and never in OrbStack or another VM that shares the host home
  folder. Integration test stores live under `$TMPDIR` with their own `LAYR_ROOT`.
- Write everything in this repository in English.
