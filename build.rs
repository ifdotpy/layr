// `layr version` shows the build identity (LAYR_BUILD_ID) and the day of the build
// (LAYR_BUILD_DATE). A release build sets both in its environment; any other build shows "dev".
// Nothing here watches `.git`: every layr line has its own `.git/HEAD` and index, and a watch made
// each new line, and each `layr add`, rebuild the binary.
//
// `layr <command> --help` prints the usage texts of the command: the string literals in `src` that
// start with "usage: layr ", as the commands print them on a wrong call (src/cmd/help.rs). They are
// collected here, so each text has one source.
fn main() {
    for var in ["LAYR_BUILD_ID", "LAYR_BUILD_DATE"] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    println!("cargo:rerun-if-changed=build.rs");
    let env = |var: &str| std::env::var(var).ok().filter(|s| !s.is_empty());
    println!("cargo:rustc-env=LAYR_BUILD_ID={}", env("LAYR_BUILD_ID").unwrap_or_else(|| "dev".into()));
    println!("cargo:rustc-env=LAYR_BUILD_DATE={}", env("LAYR_BUILD_DATE").unwrap_or_else(|| "dev".into()));
    usage_texts();
}

fn usage_texts() {
    let src = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("src");
    println!("cargo:rerun-if-changed={}", src.display());
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    files.sort();
    let mut texts: Vec<String> = Vec::new();
    for file in files {
        let code = std::fs::read_to_string(&file).unwrap();
        let mut rest = code.as_str();
        while let Some(k) = rest.find("\"usage: layr ") {
            let body = &rest[k + 1..];
            let end = literal_end(body);
            let text = &body[..end];
            rest = &body[(end + 1).min(body.len())..];
            // A text names a command. A format string (`{}`, `{sub}`) is not a fixed text.
            let names = text["usage: layr ".len()..].starts_with(|c: char| c.is_ascii_lowercase());
            if names && !text.contains('{') && !texts.iter().any(|t| t == text) {
                texts.push(text.to_string());
            }
        }
    }
    // The literals keep their escapes (`\n`, `\x20`, a `\` line end): the compiler reads them again.
    let mut out = String::from("pub const USAGES: &[&str] = &[\n");
    for text in texts {
        out.push_str(&format!("    \"{text}\",\n"));
    }
    out.push_str("];\n");
    let dest = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("usage_texts.rs");
    std::fs::write(dest, out).unwrap();
}

fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The length of a string literal's content: up to the first quote that no backslash escapes.
fn literal_end(body: &str) -> usize {
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return i,
            _ => i += 1,
        }
    }
    bytes.len()
}
