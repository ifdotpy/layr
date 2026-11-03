// `layr version` shows the build identity (LAYR_BUILD_ID) and the day of the build
// (LAYR_BUILD_DATE). A release build sets both in its environment; any other build shows "dev".
// Nothing here watches `.git`: every layr line has its own `.git/HEAD` and index, and a watch made
// each new line, and each `layr add`, rebuild the binary.
fn main() {
    for var in ["LAYR_BUILD_ID", "LAYR_BUILD_DATE"] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    println!("cargo:rerun-if-changed=build.rs");
    let env = |var: &str| std::env::var(var).ok().filter(|s| !s.is_empty());
    println!("cargo:rustc-env=LAYR_BUILD_ID={}", env("LAYR_BUILD_ID").unwrap_or_else(|| "dev".into()));
    println!("cargo:rustc-env=LAYR_BUILD_DATE={}", env("LAYR_BUILD_DATE").unwrap_or_else(|| "dev".into()));
}
