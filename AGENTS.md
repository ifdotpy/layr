# Agent rules for layr

- Do not create Git worktrees for this repository. Develop in layr lines:
  `layr line new <task> main`, commit with `layr add <files>` and `layr save -m`, and publish
  with `layr git push`. In Studio, use the Layr workspace mode for chats of this project.
- Never run layr tests as root, and never in OrbStack or another VM that shares the host home
  folder. Integration test stores live under `$TMPDIR` with their own `LAYR_ROOT`.
- Write everything in this repository in English.
- Give every commit a Conventional Commit subject: `<type>(<scope>): <Summary>`, with a type
  from `feat`, `fix`, `perf`, `revert`, `docs`, `test`, `refactor`, `build`, `ci`, `chore` or
  `style`, and `!` after the scope for a breaking change. release-please writes CHANGELOG.md and
  the release notes from these subjects, and CI checks them in each pull request.
