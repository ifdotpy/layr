//! layr: local version control on btrfs.

// Unsafe code is allowed only in `sys` (system calls without safe wrappers).
#![deny(unsafe_code)]

mod busy;
mod pools;
#[macro_use]
mod ctx;
mod access;
#[cfg(target_os = "macos")]
mod apfs_commit;
#[cfg(any(target_os = "macos", feature = "overlayfs"))]
mod apfs_stream;
#[cfg(any(target_os = "macos", feature = "overlayfs"))]
mod stream_fs;
#[cfg(all(feature = "overlayfs", not(target_os = "linux")))]
compile_error!("the overlayfs feature requires Linux");
mod args;
mod cache_overlay;
mod changes;
mod cmd;
mod db;
mod dedup;
mod diff;
mod disk;
mod follow;
mod free;
mod fsutil;
mod gitbridge;
mod ids;
mod owners;
use layr_store::http;
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
mod btrfs_scan;
mod ignore_rules;
mod layrfs;
mod lineops;
mod linepaths;
mod merge;
mod model;
mod objstore;
mod object_tree;
#[cfg(not(target_os = "macos"))]
mod source;
mod oci;
mod oplog;
mod overlay;
mod overlay_managers;
mod person;
mod privs;
mod probes;
mod project_copy;
mod project_move;
mod radar;
mod recv;
mod repo;
mod revs;
mod rootcheck;
mod runner;
mod schemas;
mod secrets;
#[cfg(target_os = "linux")]
mod send_cache;
mod service;
mod source_times;
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
#[path = "btrfs.rs"]
mod storage;
#[cfg(feature = "overlayfs")]
#[path = "overlayfs.rs"]
mod storage;
#[cfg(target_os = "macos")]
#[path = "apfs.rs"]
mod storage;
mod store;
mod store_remote;
mod store_remove;
mod store_version;
mod stream;
#[allow(unsafe_code)]
mod sys;
#[cfg(target_os = "macos")]
mod thin;
mod trace;
mod tree;
mod usage;
#[cfg(target_os = "macos")]
mod views;
#[cfg(target_os = "linux")]
#[path = "views_linux.rs"]
mod views;
mod volumes;

use std::collections::HashMap;

mod cache_hook;

fn main() {
    // Started as `rg` or `find` (a link on an agent's PATH): LayrFS answers from its index.
    let argv0 = std::env::args_os().next().map(std::path::PathBuf::from);
    if let Some(tool) = argv0.as_deref().and_then(|p| p.file_name()).and_then(|n| n.to_str()) {
        if layrfs::TOOLS.contains(&tool) {
            let args: Vec<String> = std::env::args().skip(1).collect();
            std::process::exit(layrfs::run_tool(tool, &args));
        }
    }
    // The cache adapters of the binary (issue #167). Core reaches them only through `cache_hook`.
    cache_hook::register(layr_cache::adapters());
    if std::env::args().nth(1).as_deref() == Some(cache_hook::WORKER_ARG) {
        // The worker of a cache hook: runs one adapter as the line owner (`cache_hook`).
        std::process::exit(cache_hook::worker());
    }
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("__empty-trash") && argv.len() == 2 {
        // The trash of a store, emptied in the background after a command (`store::Lock`).
        crate::sys::lower_process_priority();
        if let Err(e) = crate::store::empty_trash_process(std::path::Path::new(&argv[1])) {
            eprintln!("layr empty trash: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(target_os = "macos")]
    if argv.first().map(String::as_str) == Some("__keyframe") && (argv.len() == 4 || argv.len() == 5 && argv[4] == "final") {
        // A complete tree for a thin line head with a long chain, after the command that moved it, or for the
        // final state of a deleted line (`free`).
        if let Err(e) = crate::free::keyframe_process(std::path::Path::new(&argv[1]), &argv[2], &argv[3], argv.len() == 5) {
            eprintln!("layr keyframe: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    // `layr tool <rg|find> <args>`: before the global options and help, because the tool's own
    // arguments (`rg -h`, `find -C`) belong to the tool.
    if argv.first().map(String::as_str) == Some("tool") {
        match argv.get(1).map(String::as_str) {
            Some(tool) if layrfs::TOOLS.contains(&tool) => std::process::exit(layrfs::run_tool(tool, &argv[2..])),
            Some("--help" | "-h") => {
                println!("usage: layr tool <{}> [<args>]", layrfs::TOOLS.join("|"));
                return;
            }
            _ => {
                eprintln!("usage: layr tool <{}> [<args>]", layrfs::TOOLS.join("|"));
                std::process::exit(2);
            }
        }
    }
    if let Some(code) = project_copy::hidden(&argv) {
        std::process::exit(code);
    }
    if argv.first().map(String::as_str) == Some("__registry-http") {
        let result = (|| -> anyhow::Result<()> {
            anyhow::ensure!(argv.len() == 3, "invalid internal registry arguments");
            anyhow::ensure!(!rustix::process::geteuid().is_root(), "registry HTTP process must not run as root");
            let fd = rustix::io::dup(std::io::stdin())?;
            crate::oci::registry::handle(std::net::TcpStream::from(fd), std::path::Path::new(&argv[1]), &argv[2])
        })();
        if let Err(e) = result {
            eprintln!("layr registry HTTP: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(any(target_os = "macos", feature = "overlayfs"))]
    if argv.first().map(String::as_str) == Some("__apfs-send") {
        let result = (|| -> anyhow::Result<()> {
            // `__apfs-send <child> [<parent> <parent subvolume UUID>]`
            let parent = match argv.len() {
                2 => None,
                4 => Some((std::path::Path::new(&argv[2]), argv[3].as_str())),
                _ => anyhow::bail!("invalid internal send arguments"),
            };
            crate::apfs_stream::send(std::path::Path::new(&argv[1]), parent, &mut std::io::stdout().lock())
        })();
        if let Err(e) = result {
            eprintln!("fatal: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    // Global options before the command.
    while let Some(first) = argv.first().cloned() {
        match first.as_str() {
            "-C" if argv.len() > 1 => {
                if let Err(e) = std::env::set_current_dir(&argv[1]) {
                    eprintln!("fatal: cannot change to '{}': {e}", argv[1]);
                    std::process::exit(128);
                }
                argv.drain(..2);
            }
            "--root" if argv.len() > 1 => {
                std::env::set_var("LAYR_ROOT", &argv[1]);
                argv.drain(..2);
            }
            "--project" if argv.len() > 1 => {
                std::env::set_var("LAYR_PROJECT", &argv[1]);
                argv.drain(..2);
            }
            "--line" if argv.len() > 1 => {
                std::env::set_var("LAYR_LINE", &argv[1]);
                argv.drain(..2);
            }
            "--no-pager" | "--no-optional-locks" | "-P" => {
                argv.remove(0);
            }
            "-c" if argv.len() > 1 => {
                argv.drain(..2);
            }
            _ => break,
        }
    }
    // `layr help`, `layr help <command>`, `layr <command> ... --help` (cmd/help.rs).
    if let Some(asked) = cmd::help::wanted(&argv) {
        match cmd::help::text(&asked) {
            Some(text) => print!("{text}"),
            None => {
                eprintln!("layr: '{}' is not a layr command. See 'layr help'.", asked[0]);
                std::process::exit(1);
            }
        }
        return;
    }
    match argv.first().map(String::as_str) {
        Some("--version" | "version") => {
            println!("{}", cmd::version_line());
            return;
        }
        None | Some("--help" | "-h" | "help") => {
            print!("{}", cmd::HELP);
            return;
        }
        Some("store-version") => match store_version::run(&store::default_root(), &argv[1..]) {
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("layr store-version: {error:#}");
                std::process::exit(128);
            }
        },
        // Neither opens a store: `schema` prints a built-in file, and `store remove` deletes a store folder.
        Some("schema") => match schemas::run(&argv[1..]) {
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("layr schema: {error:#}");
                std::process::exit(128);
            }
        },
        Some("store") => match store_remove::run(&argv[1..]) {
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("layr store: {error:#}");
                std::process::exit(1);
            }
        },
        _ => {}
    }
    if matches!(argv.first().map(String::as_str), Some("init" | "daemon")) {
        // Before the store opens: the override applies to the process that opens the store.
        rootcheck::take_allow_flag(&mut argv);
    }
    if let Some(command) = argv.first() {
        if !cmd::COMMANDS.contains(&command.as_str()) {
            eprintln!("layr: '{command}' is not a layr command. See 'layr help'.");
            std::process::exit(1);
        }
    }
    // The commands that run in the caller's process (retest, person, mount) are counted here; the
    // others where they run (`service::run_as`).
    let started = std::time::Instant::now();
    let early = |argv: &[String], code: i32| {
        let agent = std::env::var_os("LAYR_SESSION").is_some();
        let ms = started.elapsed().as_millis() as u64;
        usage::count(&store::default_root(), argv, rustix::process::getuid().as_raw(), agent, code, ms);
        code
    };
    if argv.first().map(String::as_str) == Some("retest") {
        // Runs in the caller's process: it starts cargo and takes minutes, which the daemon must not do.
        match layr_retest::main(&argv[1..]) {
            Ok(code) => std::process::exit(early(&argv, code)),
            Err(error) => {
                eprintln!("layr retest: {error:#}");
                std::process::exit(early(&argv, 1));
            }
        }
    }
    if argv.first().map(String::as_str) == Some("person") {
        match person::run(&argv[1..]) {
            Ok(code) => std::process::exit(early(&argv, code)),
            Err(error) => {
                eprintln!("layr person: {error:#}");
                std::process::exit(early(&argv, 1));
            }
        }
    }
    if argv.first().map(String::as_str) == Some("fs") {
        match layrfs::run_fs(&argv[1..]) {
            Ok(code) => std::process::exit(early(&argv, code)),
            Err(error) => {
                eprintln!("layr fs: {error:#}");
                std::process::exit(early(&argv, 1));
            }
        }
    }
    if argv.first().map(String::as_str) == Some("mount") {
        match disk::run(&argv[1..]) {
            Ok(code) => std::process::exit(early(&argv, code)),
            Err(error) => {
                eprintln!("layr mount: {error:#}");
                std::process::exit(early(&argv, 1));
            }
        }
    }
    if argv.first().map(|s| s.as_str()) == Some("daemon") {
        match service::daemon(&argv[1..]) {
            Ok(c) => std::process::exit(c),
            Err(e) => {
                eprintln!("fatal: {e:#}");
                std::process::exit(128);
            }
        }
    }
    let direct = (cfg!(target_os = "macos") && std::env::var_os("LAYR_SOCKET").is_none())
        || rustix::process::geteuid().is_root()
        || std::env::var("LAYR_DIRECT").map(|v| v == "1").unwrap_or(false);
    let code = if direct {
        let uid = rustix::process::getuid().as_raw();
        let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
        let env: HashMap<String, String> = std::env::vars().collect();
        service::run_as(argv, ctx::Caller::from_uid(uid), cwd, env, &store::default_root())
    } else {
        match service::forward(&argv) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("fatal: {e:#}");
                128
            }
        }
    };
    std::process::exit(code);
}
