# Wave 4: Desk conveniences, deploy, probes in the queue (lead)

Status: decided 2026-10-24. Items 5 to 8 of `docs/cloud-repo-design.md`, and `queue.probes` of
`docs/wave2-design.md` section 6. Overlay sync follows `docs/overlay-sync-design.md`.

## 1. Keychain lock, unlock and status (`layr mount`)

`layr mount` already keeps the decrypted store identity in the login Keychain
(`src/disk/keystore.rs`, service `ws.layr.mount`, account = sha256 of the store URL). New flags,
in the style of the existing `--install` and `--remove` (a remote may be named `lock`, so these
are flags, not subcommands):

- `layr mount --unlock <remote or url>`: opens the store identity with the machine slot, the
  paper key (`--paper-key-file` or `--paper-key-stdin`) or the person key
  (`LAYR_PERSON_KEY_FILE`), checks the pin, and saves it in the Keychain. Nothing is served.
- `layr mount --lock <remote or url>`: removes the Keychain item. A running mount answers 423
  (Locked) for file reads until the next unlock, and lists folders only.
- `layr mount --status [--json]`: one line per known store (named remotes of the caller's
  projects, installed mounts, pins of the person index): URL, `locked` or `unlocked`, installed
  or not, served address. It never prints a key.
- macOS only for the Keychain. On Linux the three flags exit 2 with "the Keychain is macOS only".

## 2. One disk for all stores

- `layr mount --all [--listen 127.0.0.1:8095]` serves one WebDAV root. It has one folder per
  store the caller can know: named store remotes of the caller's projects and the pins of the
  person index. The folder name is the remote name, or `<project>-<short project id>` for a
  pin without a remote; names are unique.
- A locked store shows as an empty folder with one file `LOCKED.txt`: "Run: layr mount --unlock
  <name>". An unlocked store shows its lines as `layr mount <remote>` does today.
- `layr mount --install --all` installs one launchd service and one Finder disk named `Layr`.
  `layr mount --remove --all` removes them. The single-store `--install` stays as it is.
- Read only, as today. No new network listener beyond 127.0.0.1.

## 3. AWS login for `s3://`

- Credentials for `s3://` come from, in order: the environment variables (as today), then the
  AWS profile named by `AWS_PROFILE` or by the remote setting `remote.<name>.aws-profile` (a
  profile name is not a secret, so it may be in config), then `default`.
- A profile can hold static keys (`~/.aws/credentials`), `credential_process` is NOT supported
  (it runs a program; refuse with a clear message), and IAM Identity Center (SSO) through
  `sso_session` or the old `sso_start_url` keys: Layr reads the token in `~/.aws/sso/cache`,
  refreshes it with its refresh token when near its end, and calls GetRoleCredentials. Port
  Desk `crates/layr-desk/src/profile.rs` (layr-desk 31610ce), including `AWS_CONFIG_FILE`,
  `AWS_SHARED_CREDENTIALS_FILE`, `AWS_ENDPOINT_URL_SSO` and `AWS_ENDPOINT_URL_SSO_OIDC`.
- `layr remote login <name>` runs the SSO device authorization flow (prints the URL and code,
  waits) as Desk `login` did, and writes the token to the standard SSO cache file (mode 0600).
- Shared code goes into `crates/layr-store` (aws), so the gateway and `layr-cloud deploy` use it.
- Credentials are never written to Layr config, records or logs.

## 4. Deploy with one command (layr-cloud)

- New binary `layr-cloud` in `crates/layr-cloud-cli` with `layr-cloud deploy --region <r>
  --stack-prefix <p> --release <dir>`: creates or updates the CloudFormation stacks of
  `infra/aws` in order, uploads the gateway and executor release archives, waits for each stack
  with a bounded wait, and prints the outputs (gateway URL, bucket names). Port the stack logic of
  Desk `crates/layr-desk/src/setup.rs` (CreateStack, UpdateStack, DescribeStacks, "no updates"
  is success, a failed create names the events).
- Credentials through the same AWS chain as section 3 (`--profile`).
- `layr-cloud deploy --dry-run` prints the stacks and parameters it would use, without calls.
- Tests: unit tests against a local fake CloudFormation HTTP endpoint
  (`AWS_ENDPOINT_URL_CLOUDFORMATION`). No live AWS in CI.

## 5. Probes in the merge queue

- Setting `queue.probes = true` (per project). When set, `layr queue run` runs every probe on
  the queue entry's base (the target head) and on its merge result, with the existing `probe run`
  mechanism (results are reused for equal argv and state), before the target moves.
- A probe may set a limit in its definition: `fail_over = 0.30` means the entry fails when any
  metric, or the size of any output file, grows by more than 30 % from base to result. Without
  `fail_over`, a probe result never fails an entry; its diff is only stored.
- A failed entry names the probe, the metric or file, both values and the limit, for example
  `probe bench-checkout: p95_ms 41 → 52 (+27 %) is over the limit +20 %`.
- `layr queue list` shows the probe outcome per entry. The change's "Behavior" section
  (`layr change show`) reads the same stored results.
- Probes still never run implicitly elsewhere.
