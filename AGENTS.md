# Agent rules for layr

- Do not create Git worktrees for this repository. Develop in layr lines:
  `layr line new <task> main`, commit with `layr add <files>` and `layr save -m`, and publish
  with `layr git push`. In Studio, use the Layr workspace mode for chats of this project.
- Never run layr tests as root, and never in OrbStack or another VM that shares the host home
  folder. Integration test stores live under `$TMPDIR` with their own `LAYR_ROOT`.
- A new integration test picks its CI jobs on line 2 of its file: `# ci: macos overlayfs` (tags:
  `macos`, `macos-plain` (without LAYR_TEST_DIRECT_MAIN), `apfs-case`, `overlayfs`). The btrfs job runs every test. Do not add test names to the
  workflow files.
- Write everything in this repository in English.
- Pull requests merge by squash. Give each pull request a Conventional Commit title:
  `<type>(<scope>): <Summary>`, with a type from `feat`, `fix`, `perf`, `revert`, `docs`, `test`,
  `refactor`, `build`, `ci`, `chore` or `style`, and `!` after the scope for a breaking change.
  The title becomes the commit on main; release-please writes CHANGELOG.md and the release notes
  from these commits. CI checks the title.
