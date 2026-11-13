//! The help of one command: `layr <command> --help` and `layr help <command> [<subcommand>]`.
//!
//! The text is the usage of the command, as the command prints it on a wrong call: build.rs
//! collects the string literals of `src` that start with "usage: layr " into `USAGES`. A command
//! without one gets the lines of the general help that name it, under their headings.

use super::{COMMANDS, HELP};

include!(concat!(env!("OUT_DIR"), "/usage_texts.rs"));

/// Options whose value is text: a `-h` or `--help` right after one is the value, not a request
/// for help (`layr save -m -h`).
const TEXT_OPTIONS: &[&str] = &["-m", "--message", "--note", "-e", "--grep", "--author", "--title", "--description", "--reason"];

/// The help that `argv` (after the global options) asks for: `None` for none, an empty list for
/// the general help, else the command and its first subcommand word.
pub fn wanted(argv: &[String]) -> Option<Vec<String>> {
    let Some(first) = argv.first() else { return Some(Vec::new()) };
    if matches!(first.as_str(), "help" | "--help" | "-h") {
        return Some(words(&argv[1..]));
    }
    // grep uses -h to omit file names. Its long help flag stays.
    let short = first != "grep";
    let mut value = false;
    for arg in &argv[1..] {
        if arg == "--" {
            break;
        }
        if !value && (arg == "--help" || short && arg == "-h") {
            let mut asked = vec![first.clone()];
            asked.extend(words(&argv[1..]).into_iter().take(1));
            return Some(asked);
        }
        value = TEXT_OPTIONS.contains(&arg.as_str());
    }
    None
}

/// The leading words of `args` that can name a command and a subcommand.
fn words(args: &[String]) -> Vec<String> {
    let word = |s: &String| !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    args.iter().take_while(|s| word(s) && !s.starts_with('-')).take(2).cloned().collect()
}

/// The help text for `asked` (from `wanted`); `None` if the first word is not a layr command.
pub fn text(asked: &[String]) -> Option<String> {
    let Some(command) = asked.first() else { return Some(HELP.to_string()) };
    if !COMMANDS.contains(&command.as_str()) {
        return None;
    }
    // `help`, `--help`, `--version --help`.
    if command == "help" || command.starts_with('-') {
        return Some(HELP.to_string());
    }
    let all = entries();
    let of = |prefix: &str| -> Vec<&String> {
        all.iter().filter(|e| e.strip_prefix(prefix).is_some_and(|r| r.is_empty() || r.starts_with([' ', '\n']))).collect()
    };
    let mut found = of(&format!("layr {command}"));
    // A short list of subcommands that a longer one repeats ("line new/list/...") is left out.
    found.retain(|e| !all.iter().any(|o| o.len() > e.len() && o.starts_with(e.as_str()) && !o[e.len()..].starts_with(' ')));
    if let Some(sub) = asked.get(1) {
        let narrow = of(&format!("layr {command} {sub}"));
        if !narrow.is_empty() {
            found = narrow;
        }
    }
    if !found.is_empty() {
        let mut out = String::new();
        for (i, entry) in found.iter().enumerate() {
            out.push_str(if i == 0 { "usage: " } else { "       " });
            out.push_str(entry);
            out.push('\n');
        }
        return Some(out);
    }
    Some(general_lines(command))
}

/// The usage entries: one per `layr ...` form, with the text lines that follow it.
fn entries() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for text in USAGES {
        let text = text.strip_prefix("usage: ").unwrap_or(text);
        for line in text.lines() {
            let form = line.trim();
            if form.starts_with("layr ") {
                // `layr op log ... | layr op show <id>`: one entry per form.
                for (i, part) in form.split(" | layr ").enumerate() {
                    let entry = if i == 0 { part.to_string() } else { format!("layr {part}") };
                    if !out.contains(&entry) {
                        out.push(entry);
                    }
                }
            } else if let Some(last) = out.last_mut() {
                last.push('\n');
                last.push_str(line.trim_end());
            }
        }
    }
    out
}

/// A usage line, then the lines of the general help that name `command`, under their headings.
fn general_lines(command: &str) -> String {
    let mut out = format!("usage: layr {command} [<args>]\n");
    let mut heading = "";
    // The commands are the indented lines between the first line and the closing text.
    let commands = HELP.rsplit_once("\n\n").map_or(HELP, |(commands, _)| commands);
    for line in commands.lines().skip(1) {
        if line.is_empty() {
            continue;
        }
        if !line.starts_with(' ') {
            heading = line;
            continue;
        }
        // Each item of the list starts with a command name: "log, show", "ref set/list/log/delete".
        let names = line.split([',', ';']).filter_map(|item| item.split_whitespace().next());
        if names.into_iter().any(|n| n == command) {
            if !heading.is_empty() {
                out.push_str(heading);
                out.push('\n');
                heading = "";
            }
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{text, wanted};

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    fn help(s: &str) -> String {
        text(&wanted(&args(s)).expect("a help request")).expect("a layr command")
    }

    /// The help without its "usage: " start. (A test literal must not start with "usage: layr ":
    /// build.rs would take it as a usage text.)
    fn usage(s: &str) -> String {
        let out = help(s);
        out.strip_prefix("usage: ").unwrap_or_else(|| panic!("{out}")).to_string()
    }

    #[test]
    fn help_requests() {
        assert_eq!(wanted(&args("")), Some(vec![]));
        assert_eq!(wanted(&args("help")), Some(vec![]));
        assert_eq!(wanted(&args("--help")), Some(vec![]));
        assert_eq!(wanted(&args("help checks watch")), Some(args("checks watch")));
        assert_eq!(wanted(&args("checks watch --help")), Some(args("checks watch")));
        assert_eq!(wanted(&args("log --oneline -h")), Some(args("log")));
        assert_eq!(wanted(&args("log main --help")), Some(args("log main")));
        // A text value, an argument after `--`, and grep's own -h are no help requests.
        assert_eq!(wanted(&args("save -m -h")), None);
        assert_eq!(wanted(&args("save --message --help")), None);
        assert_eq!(wanted(&args("change edit 1 --title --help")), None);
        assert_eq!(wanted(&args("change edit 1 --description -h")), None);
        assert_eq!(wanted(&args("change close 1 -m --help")), None);
        assert_eq!(wanted(&args("remote mode cloud --reason -h")), None);
        assert_eq!(wanted(&args("store remove -h")), Some(args("store remove")));
        assert_eq!(wanted(&args("comment abc -- --help")), None);
        assert_eq!(wanted(&args("try head -- cargo --help")), None);
        assert_eq!(wanted(&args("grep -h x")), None);
        assert_eq!(wanted(&args("grep --help")), Some(args("grep")));
        assert_eq!(wanted(&args("status")), None);
    }

    #[test]
    fn general_help_stays() {
        assert_eq!(help(""), crate::cmd::HELP);
        assert_eq!(help("help"), crate::cmd::HELP);
        assert_eq!(help("--help"), crate::cmd::HELP);
        assert_eq!(text(&args("nosuch")), None);
    }

    #[test]
    fn command_help_is_its_usage() {
        let checks = usage("checks --help");
        assert!(checks.starts_with("layr checks watch "), "{checks}");
        assert!(checks.contains("\n       layr checks join "), "{checks}");
        assert!(checks.contains("layr checks pending "), "{checks}");
        assert!(!checks.contains("layr line "), "{checks}");

        let comment = usage("help comment");
        assert!(comment.starts_with("layr comment <change> (-m <text> | <text>) "), "{comment}");

        // A subcommand narrows the text; `|`-joined forms are entries of their own.
        assert_eq!(usage("op show --help"), "layr op show <id>\n");
        assert_eq!(usage("help line move"), "layr line move <line> [<path>]\n");

        // The text lines after a form stay with it.
        let try_help = help("try -h");
        assert!(try_help.contains("layr try [--keep] [-q] <rev> --check <name>"), "{try_help}");
        assert!(try_help.contains("Runs the command in a temporary copy"), "{try_help}");
    }

    #[test]
    fn command_without_usage_text_gets_its_general_help_lines() {
        let log = usage("log --help");
        assert!(
            log.starts_with("layr log [<args>]\nRead history:\n   log, show, blame, grep, ls-files, rev-parse, merge-base\n"),
            "{log}"
        );
        assert!(!log.contains("Work in a line:"), "{log}");
        let save = usage("help save");
        assert!(save.starts_with("layr save [<args>]\nWork in a line:\n"), "{save}");
        assert!(save.contains("save -m <message> [-a] [--amend]"), "{save}");
        assert!(!save.contains("Remotes:"), "{save}");
    }

    #[test]
    fn every_command_has_a_short_help() {
        for command in crate::cmd::COMMANDS {
            if command.starts_with('-') || *command == "help" {
                continue;
            }
            let out = text(&[command.to_string()]).unwrap();
            assert_ne!(out, crate::cmd::HELP, "{command}");
            assert!(out.contains(command), "{command}: {out}");
        }
    }
}
