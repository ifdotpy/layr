# Built release files

A release ref names an exact state. Attach files on a trusted machine with write
access to the store and permission to update that release ref:

```sh
layr ref set release/1.2.0 main
layr release attach release/1.2.0 target/layr-linux-arm64 target/layr-macos-arm64
layr release files release/1.2.0
layr release get release/1.2.0 -o downloads
layr release get release/1.2.0 layr-linux-arm64 -o downloads
```

The default remote is `origin`; use `--remote <name>` for another store. Use
`--name <filename>` to rename a single attached file. `--expect <state>` rejects
an attach when the release has moved. A successful attach publishes its signed
record and the release state to that remote. A later attach replaces the displayed
list for the same ref and state. Use `--json` for machine-readable lists.

Files have a 512 MiB limit and each list has at most 64 files. The CLI streams
through a private ciphertext spool. The signed `release.files` record binds the
ref, exact state, filenames, plaintext sizes, SHA-256 digests and random UUID v7
`files/<uuid>.age` keys. Objects use the current project epoch vault recipient.

Downloads verify age authentication, plaintext size and SHA-256 before publishing
a file. Existing output files are preserved. A failed download removes its
unfinished output. `--public` selects anonymous access for a public project.
The site offers downloads up to 256 MiB; larger files show the CLI command.

A moved ref has no files until files are attached to its new state. GC retains
all published lists while their release ref exists. After deleting the ref and
pushing, remote GC frees its files:

```sh
layr ref delete release/1.2.0
layr push origin
layr gc --remote origin
```

An unattached upload has a 24-hour grace period so GC cannot remove an upload in
progress. A failed attach can leave such an orphan. Retry the attach after fixing
the error; UUID keys ensure it cannot overwrite a published object.
