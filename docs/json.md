# JSON output

This page is part of the [Layr documentation](../README.md#documentation).

Many commands write JSON with `--json`. For the commands below, the repository has a JSON Schema
(draft 2020-12) in `schemas/`. Each field of the output has a description in its schema.

| Schema         | Command                          |
| -------------- | -------------------------------- |
| `status`       | `layr status --json`             |
| `line-list`    | `layr line list --json`          |
| `queue-list`   | `layr queue list --json`         |
| `change-list`  | `layr change list --json`        |
| `change-show`  | `layr change show <id> --json`   |
| `attestations` | `layr attestations <rev> --json` |

## Get a schema

The binary holds the schemas of its version. Use the schema of the installed layr:

```sh
layr schema               # lists the names
layr schema status        # prints schemas/status.json
```

## Version

Each schema has a top-level `"version"` (an integer). The current version of each schema is 1.

- A new field does not change the version. Objects in the schemas allow more fields
  (`"additionalProperties": true`), so a consumer that validates the output keeps working
  when a release adds a field. Do not use `additionalProperties: false` in a consumer.
- A field that is removed or renamed, or that changes its type or meaning, increases the version
  of that schema by 1.

`"version"` is not a JSON Schema keyword. A validator in strict mode (for example Ajv) must
know it: use `ajv.addKeyword("version")`, or turn strict mode off.

## Keep the schemas correct

`tests/t240-json-schemas.sh` runs each command in a test store and checks its output against
the schema with `tests/json-schema.py` (Python standard library only). The check is strict: a
field that the output has and the schema does not describe fails the test. A change of the
output must therefore change the schema in the same pull request. The test also checks that
`layr schema <name>` prints the file in `schemas/`.

To add a schema:

1. Write `schemas/<name>.json`. Give it `$schema`, `$id` (`urn:layr:schema:<name>`),
   `"version": 1`, `title`, `description`, and a `description` for each property.
2. Add the file to `SCHEMAS` in `src/schemas.rs`.
3. Add the command to `tests/t240-json-schemas.sh` and to the table above.

`tests/json-schema.py` knows a subset of JSON Schema: `type`, `properties`, `required`,
`additionalProperties`, `items`, `enum`, `const`, `anyOf`, `pattern`, `minimum`, `$ref` to
`#/$defs/...`, and the annotations `title`, `description` and `examples`. `json-schema.py lint`
refuses a schema with another keyword.

## Known limits

- In `change show --json`, an item of `checks` is the result of a check. For a check
  attestation that `layr attest check` made by hand there is no result; the item is then the
  record data, with an `attestation` member.
- `status` in `queue-list`, `kind` in `attestations` and similar fields are strings with
  `examples`, not closed lists: a new value is not an incompatible change.
