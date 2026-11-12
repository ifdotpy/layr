"""A small JSON Schema (draft 2020-12) validator for the schemas in schemas/, with the Python standard library only.

usage: json-schema.py lint <schema>
       json-schema.py check <schema> <json file> [--strict]

`lint` checks the schema itself: the draft, an integer `version`, only the keywords that this validator knows, and a
description for each property. `check` validates a command's JSON output. With --strict, each object member must also be
named in `properties` where the schema lists properties: an output field that the schema does not describe fails, so the
schema cannot fall behind the output. The published schemas allow more members (`additionalProperties: true`), so a
consumer keeps working when a release adds a field.
"""
import json
import re
import sys

DRAFT = "https://json-schema.org/draft/2020-12/schema"
KNOWN = {
    "$schema", "$id", "$ref", "$defs", "version", "title", "description", "examples", "type", "properties", "required",
    "additionalProperties", "items", "enum", "const", "anyOf", "pattern", "minimum",
}
TYPES = {
    "object": lambda v: isinstance(v, dict),
    "array": lambda v: isinstance(v, list),
    "string": lambda v: isinstance(v, str),
    "integer": lambda v: isinstance(v, int) and not isinstance(v, bool),
    "number": lambda v: isinstance(v, (int, float)) and not isinstance(v, bool),
    "boolean": lambda v: isinstance(v, bool),
    "null": lambda v: v is None,
}


def resolve(root, ref):
    if not ref.startswith("#/"):
        raise ValueError(f"only local references are supported: {ref}")
    node = root
    for part in ref[2:].split("/"):
        node = node[part]
    return node


def validate(root, schema, value, path="$", strict=False):
    """Return a list of error strings."""
    if schema is True:
        return []
    if schema is False:
        return [f"{path}: no value is allowed here"]
    errors = []
    if "$ref" in schema:
        errors += validate(root, resolve(root, schema["$ref"]), value, path, strict)
    if "type" in schema:
        kinds = schema["type"] if isinstance(schema["type"], list) else [schema["type"]]
        if not any(TYPES[k](value) for k in kinds):
            return errors + [f"{path}: expected {'/'.join(kinds)}, got {json.dumps(value)[:80]}"]
    if "const" in schema and value != schema["const"]:
        errors.append(f"{path}: expected {json.dumps(schema['const'])}, got {json.dumps(value)[:80]}")
    if "enum" in schema and value not in schema["enum"]:
        errors.append(f"{path}: {json.dumps(value)[:80]} is not one of {schema['enum']}")
    if "pattern" in schema and isinstance(value, str) and not re.search(schema["pattern"], value):
        errors.append(f"{path}: {value!r} does not match {schema['pattern']}")
    if "minimum" in schema and TYPES["number"](value) and value < schema["minimum"]:
        errors.append(f"{path}: {value} is less than {schema['minimum']}")
    if "anyOf" in schema:
        results = [validate(root, branch, value, path, strict) for branch in schema["anyOf"]]
        if all(results):
            errors.append(f"{path}: matches no branch of anyOf: " + "; ".join(r[0] for r in results))
    if isinstance(value, dict):
        props = schema.get("properties", {})
        for name in schema.get("required", []):
            if name not in value:
                errors.append(f"{path}: the required member '{name}' is missing")
        extra = schema.get("additionalProperties", True)
        for name, member in value.items():
            if name in props:
                errors += validate(root, props[name], member, f"{path}.{name}", strict)
            elif isinstance(extra, dict) or extra is False:
                errors += validate(root, extra, member, f"{path}.{name}", strict)
            elif strict and "properties" in schema:
                errors.append(f"{path}: the member '{name}' is not described in the schema")
    if isinstance(value, list) and "items" in schema:
        for i, item in enumerate(value):
            errors += validate(root, schema["items"], item, f"{path}[{i}]", strict)
    return errors


def lint(schema):
    errors = []
    if schema.get("$schema") != DRAFT:
        errors.append(f"$schema must be {DRAFT}")
    if not isinstance(schema.get("version"), int) or isinstance(schema.get("version"), bool) or schema["version"] < 1:
        errors.append("version must be an integer from 1")
    for key in ("$id", "title", "description"):
        if not isinstance(schema.get(key), str) or not schema[key]:
            errors.append(f"{key} is missing")

    def walk(node, path):
        if isinstance(node, bool):
            return
        if not isinstance(node, dict):
            errors.append(f"{path}: a schema must be an object or a boolean")
            return
        for key in node:
            if key not in KNOWN:
                errors.append(f"{path}: the keyword '{key}' is not supported by tests/json-schema.py")
        if "$ref" in node:
            try:
                resolve(schema, node["$ref"])
            except (KeyError, ValueError) as e:
                errors.append(f"{path}: bad $ref {node['$ref']}: {e}")
        for name, sub in node.get("properties", {}).items():
            if isinstance(sub, dict) and "description" not in sub and "description" not in resolve_or_empty(sub):
                errors.append(f"{path}.properties.{name}: no description")
            walk(sub, f"{path}.properties.{name}")
        for name, sub in node.get("$defs", {}).items():
            walk(sub, f"{path}.$defs.{name}")
        for key in ("items", "additionalProperties"):
            if isinstance(node.get(key), dict):
                walk(node[key], f"{path}.{key}")
        for i, sub in enumerate(node.get("anyOf", [])):
            walk(sub, f"{path}.anyOf[{i}]")

    def resolve_or_empty(sub):
        try:
            return resolve(schema, sub["$ref"]) if "$ref" in sub else {}
        except (KeyError, ValueError):
            return {}

    walk(schema, "$")
    return errors


def main(argv):
    if len(argv) == 3 and argv[1] == "lint":
        errors = lint(json.load(open(argv[2])))
    elif len(argv) in (4, 5) and argv[1] == "check" and argv[4:] in ([], ["--strict"]):
        schema = json.load(open(argv[2]))
        errors = validate(schema, schema, json.load(open(argv[3])), strict=argv[4:] == ["--strict"])
    else:
        print(__doc__.split("\n\n")[1], file=sys.stderr)
        return 2
    for e in errors[:20]:
        print(e)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
