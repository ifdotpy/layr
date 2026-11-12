//! `layr schema [<name>]`: the JSON Schemas of the `--json` output of commands (schemas/, docs/json.md), built
//! into the binary, so a tool reads the schema of the installed version.

use anyhow::Result;

/// Each schema by name: the file `schemas/<name>.json`.
pub const SCHEMAS: &[(&str, &str)] = &[
    ("attestations", include_str!("../schemas/attestations.json")),
    ("change-list", include_str!("../schemas/change-list.json")),
    ("change-show", include_str!("../schemas/change-show.json")),
    ("line-list", include_str!("../schemas/line-list.json")),
    ("queue-list", include_str!("../schemas/queue-list.json")),
    ("status", include_str!("../schemas/status.json")),
];

pub fn run(args: &[String]) -> Result<i32> {
    match args {
        [] => {
            for (name, _) in SCHEMAS {
                println!("{name}");
            }
            Ok(0)
        }
        [name] => match SCHEMAS.iter().find(|(n, _)| n == name) {
            Some((_, text)) => {
                print!("{text}");
                Ok(0)
            }
            None => {
                let names: Vec<&str> = SCHEMAS.iter().map(|(n, _)| *n).collect();
                eprintln!("layr schema: unknown schema '{name}'; the schemas are: {}", names.join(", "));
                Ok(1)
            }
        },
        _ => {
            eprintln!("usage: layr schema [<name>]");
            Ok(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SCHEMAS;

    /// Each file of schemas/ is built in, and each one is a draft 2020-12 schema with its name and a version.
    #[test]
    fn every_schema_is_built_in_and_well_formed() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schemas");
        let mut files: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter_map(|n| n.strip_suffix(".json").map(str::to_owned))
            .collect();
        files.sort();
        let names: Vec<String> = SCHEMAS.iter().map(|(n, _)| n.to_string()).collect();
        assert_eq!(files, names, "schemas/ and SCHEMAS (sorted by name) must list the same schemas");
        for (name, text) in SCHEMAS {
            let schema: serde_json::Value = serde_json::from_str(text).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(schema["$schema"], "https://json-schema.org/draft/2020-12/schema", "{name}");
            assert_eq!(schema["$id"], format!("urn:layr:schema:{name}"), "{name}");
            assert!(schema["version"].as_u64().is_some_and(|v| v >= 1), "{name}: version");
            assert!(schema["title"].as_str().is_some_and(|t| t.starts_with("layr ")), "{name}: title");
        }
    }
}
