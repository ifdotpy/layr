# Runnable snapshots and OCI registries

This page is part of the [Layr documentation](../README.md#documentation).

## Runnable snapshots and OCI registries

Open Container Initiative (OCI) images come from immutable snapshot files.
A push never reads unsaved files and never executes a build command.
Put the runnable definitions in `layr.toml` at the project root:

```toml
[runnable.app]
layers = [["runtime"], ["deps"], ["app", "config"]]
entrypoint = ["/runtime/bin/python"]
cmd = ["/app/server.py"]
env = { MODE = "production" }
working_dir = "/app"
ports = ["8080/tcp"]
platform = { os = "linux", arch = "amd64" }
chart = "deployment"
```

| Field | Meaning |
|---|---|
| `layers` | Required ordered list of path sets. Each set produces one OCI layer. An empty set produces an empty layer. |
| `entrypoint`, `cmd` | Argument arrays. At least one array must contain an argument. No shell interprets these arrays. |
| `env` | Map of environment names to string values. Default: empty. |
| `working_dir` | Absolute path inside the container. Default: `/`. |
| `ports` | Exposed ports, such as `8080/tcp` or `5353/udp`. Default protocol: `tcp`. |
| `platform` | Required OCI `os` and `arch`. Use `linux` and `amd64` or `arm64`, for example. |
| `chart` | Optional chart folder in the same snapshot. `Chart.yaml` must use the runnable name and a SemVer version. |

Paths are relative to the snapshot root.
A path can name a file, folder, or glob (`*`, `?`, `[...]`).
The path `.` selects all shared files except `.git`.
Paths cannot be absolute or contain `..`.
Unknown runnable fields and missing paths produce errors.
Each runnable has one platform.
Use a separate runnable name for each platform.

A structured layer can use `{ paths = ["app"], private = false, secret = false }`.
OCI export rejects a layer when `private` or `secret` is true.
Export also rejects overlay projects, states from lines with attached overlays, and paths in the project setting `secret.paths`.
The setting `oci.secret_paths` adds OCI-specific exclusions.
Special files and OCI whiteout names (`.wh.*`) are rejected.
File names and symbolic-link targets must use UTF-8.
Symbolic links are archived without access to their targets.

```sh
layr remote add images oci://registry.example/project/app
layr push images main --runnable app --json
```

The remote URL names the image repository.
With one runnable, `--runnable` is optional.
For credentials, set `LAYR_OCI_USERNAME` and `LAYR_OCI_PASSWORD` in the command environment.
Credentials come only from these variables. Project settings are signed records that every machine and store remote receives, so the settings `remote.<name>.username` and `remote.<name>.password` are refused.
A push checks the whole exported tree with the secret detector before any upload.
The client supports Basic and Bearer authentication through the [OCI Distribution API](https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md).
HTTPS is the default.
For a local test registry, set `remote.<name>.insecure` to `true` to use HTTP.

A layer uses sorted tar entries, timestamp zero, owner and group zero, and fixed gzip settings.
File modes, link targets, paths, and file bytes determine its identity.
Symbolic-link modes are fixed at `0777` because host systems report different modes for identical links.
The cache uses content hashes.
Compressed layers live in detached Core states with `kind = "derived"`, `context = "oci-layer-v1"`, and `derived_from` set to the source state.
Refs named `derived/oci-layer-v1/<blob-hash>` keep these states through garbage collection.
Identical layer content reuses the same derived state, including across source revisions.
The cache metadata, image configs, and manifests live in `meta/oci`.
A repeated export reuses compressed blobs.
A push checks each blob in the registry before upload.
The default output shows the runnable, destination, digests, and blob counts.
With `--json`, the result includes `state`, `runnable`, `image`, `chart`, `tag`, `uploaded_blobs`, and `skipped_blobs`.

A chart uses the same revision as its image.
Layr sets `image.digest` in `values.yaml` to the exact image manifest digest.
Chart templates must use this value, for example `{{ .Values.image.repository }}@{{ .Values.image.digest }}`.
Layr uses the [Helm OCI config and chart layer media types](https://helm.sh/docs/topics/registries/).
A push writes the chart to the sibling repository `project/charts/app`, with its chart version as the tag.
Helm replaces `+` with `_` in a version tag.
Chart archives have fixed metadata and sorted entries.
Chart folders cannot contain symbolic links or paths excluded as secrets.

To enable the read-only registry in the Layr service:

```sh
layr config registry.listen 127.0.0.1:5080
layr config registry.user layr-registry
layr config registry.public_url https://layr.example
layr access token alice --until 1d
# Restart the Layr daemon after a registry.listen or registry.user change.
```

The token command prints one opaque token and records only its SHA256 hash.
A new token replaces the user's token on this machine.
`layr access token --revoke alice` revokes that user's token on this machine.
The default token lifetime is one day.
Token issue and revocation require the `access` permission.
The token belongs to one user and one Layr machine.
Use a TLS reverse proxy for access outside the local machine.
Set `registry.public_url` to the public origin so Bearer challenges use the correct token endpoint.
The listener itself speaks HTTP.
The root daemon accepts the connection without reading it.
The HTTP child permanently drops its user, group, and supplementary groups before it reads a request.
It then starts a clean process to remove the daemon's memory and unrelated file descriptors.
`registry.user` selects this user and defaults to `nobody` with its primary group.
The daemon refuses user ID zero or a privilege-drop failure.
Projects on the same listener must use the same user.
Use a dedicated service account.
This account can use only the registry protocol on the service socket, which prevents access through the ordinary CLI protocol.
The executable and the Unix socket's parent folders must allow this user to traverse them.

The HTTP child sends a typed request over the existing service Unix socket.
The root backend checks the socket peer, token, and ACL before it returns manifest or blob bytes.
The backend accepts no file paths or network descriptors.
The HTTP child permits a 4096-byte request line and 16384-byte headers.
It permits one request per connection, with a ten-second read timeout and a sixty-second write timeout.

The service accepts a Bearer token on each request.
It also offers `/token` for the Basic credential exchange that Helm and skopeo use.
Use any username and the issued token as the password for these clients.
The service checks the user's current project reader policy on each request, including requests for cached blobs.
The current policy applies to every ref in the project.
The authorization function is the extension point for future per-ref policies.
Anonymous access requires both `registry.anonymous = true` and the project's `*` reader role.
Anonymous access is off by default.

The endpoints support `GET` and `HEAD` for manifests and blobs, plus tag lists with `n` and `last` pagination.
All write requests fail with HTTP 405.
Image paths use `<project>/<runnable>`.
Chart paths use `<project>/charts/<runnable>`.

| OCI tag | Layr revision |
|---|---|
| `line-main` | Line `main` |
| `state-<id>` | Exact state `<id>` |
| `prod`, `staging` | `env/prod`, `env/staging` |
| `1.4.2` | `release/1.4.2` |
| `change-<id>` | `changes/<id>` |
| `view-<id>` | `views/<id>` |

Environment, release, and change tags use the Core revision resolver.
Tag lists include lines, environment refs, release refs, and changes.
An unavailable exact view returns `MANIFEST_UNKNOWN`.
A manifest digest remains readable after the service has generated or exported that manifest.

```sh
skopeo copy --src-creds alice:TOKEN docker://layr.example/project/app:line-main dir:./image
helm pull oci://layr.example/project/charts/app@sha256:DIGEST --username alice --password TOKEN
```

The test file `tests/t30-oci.sh` needs Linux, Btrfs, `docker-registry`, `skopeo`, `helm`, `htpasswd`, `curl`, and Python 3.
It checks external pulls, Basic and Bearer push, service pulls, token revocation, and secret rejection.
It also checks privilege separation, Core tags, and derived-state reuse.

