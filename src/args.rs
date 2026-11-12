//! A small git-style argument parser: short and long options, `--opt=value`, `-n5`, grouped
//! short flags, and `--` as the end of the options.
//!
//! After `--`, no argument is an option: `layr comment <change> -- "-1 for this"`. The arguments
//! after `--` are positional, unless the command gives `--` its own meaning (paths, a command to
//! run) with `Spec::paths`. A value option takes the next argument as its value even if it
//! starts with `-`: `layr save -m "-x"`.

use anyhow::{bail, Result};
use std::collections::HashMap;

#[derive(Default)]
pub struct Spec {
    /// Option names that take no value, for example `-s`, `--short`.
    flags: Vec<&'static str>,
    /// Option names that take a value.
    values: Vec<&'static str>,
    /// Long options with an optional value (`--porcelain`, `--porcelain=v1`).
    optional: Vec<&'static str>,
    /// Aliases: alias to canonical name.
    alias: HashMap<&'static str, &'static str>,
    /// The arguments after `--` go to `Parsed::paths`, not to `Parsed::pos`.
    paths: bool,
}

impl Spec {
    pub fn new() -> Spec {
        Spec::default()
    }
    /// A flag with names separated by `|`, for example `"-s|--short"`. The first is canonical.
    pub fn flag(mut self, names: &'static str) -> Spec {
        let v: Vec<&'static str> = names.split('|').collect();
        for n in &v {
            self.flags.push(n);
            self.alias.insert(n, v[0]);
        }
        self
    }
    pub fn value(mut self, names: &'static str) -> Spec {
        let v: Vec<&'static str> = names.split('|').collect();
        for n in &v {
            self.values.push(n);
            self.alias.insert(n, v[0]);
        }
        self
    }
    /// The command reads the arguments after `--` apart from the positional ones (paths, as in
    /// `layr diff <rev> -- <path>`, or a command, as in `layr try <rev> -- <cmd>`).
    pub fn paths(mut self) -> Spec {
        self.paths = true;
        self
    }
    pub fn optional(mut self, names: &'static str) -> Spec {
        let v: Vec<&'static str> = names.split('|').collect();
        for n in &v {
            self.optional.push(n);
            self.alias.insert(n, v[0]);
        }
        self
    }
}

#[derive(Debug, Default)]
pub struct Parsed {
    pub flags: HashMap<String, usize>,
    pub values: HashMap<String, Vec<String>>,
    pub pos: Vec<String>,
    /// Arguments after `--`, for a spec with `Spec::paths` (in `pos` for the others).
    pub paths: Vec<String>,
    pub dashdash: bool,
}

impl Parsed {
    pub fn has(&self, name: &str) -> bool {
        self.flags.contains_key(name) || self.values.contains_key(name)
    }
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).and_then(|v| v.last()).map(|s| s.as_str())
    }
    pub fn all(&self, name: &str) -> Vec<String> {
        self.values.get(name).cloned().unwrap_or_default()
    }
    /// Positional arguments and paths after `--` together.
    pub fn rest(&self) -> Vec<String> {
        let mut v = self.pos.clone();
        v.extend(self.paths.iter().cloned());
        v
    }
}

pub fn parse(spec: &Spec, args: &[String]) -> Result<Parsed> {
    let mut p = Parsed::default();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if p.dashdash {
            if spec.paths {
                p.paths.push(a.clone());
            } else {
                p.pos.push(a.clone());
            }
            continue;
        }
        if a == "--" {
            p.dashdash = true;
            continue;
        }
        if a.starts_with("--") {
            let (name, val) = match a.find('=') {
                Some(k) => (&a[..k], Some(a[k + 1..].to_string())),
                None => (a.as_str(), None),
            };
            if let Some(canon) = spec.alias.get(name) {
                if spec.flags.contains(&name) {
                    if val.is_some() {
                        bail!("option {name} takes no value");
                    }
                    *p.flags.entry(canon.to_string()).or_insert(0) += 1;
                } else if spec.values.contains(&name) {
                    let v = match val {
                        Some(v) => v,
                        None => {
                            if i >= args.len() {
                                bail!("option {name} needs a value");
                            }
                            i += 1;
                            args[i - 1].clone()
                        }
                    };
                    p.values.entry(canon.to_string()).or_default().push(v);
                } else {
                    p.values.entry(canon.to_string()).or_default().push(val.unwrap_or_default());
                }
                continue;
            }
            bail!("{}", unknown(name, a));
        }
        if a.starts_with('-') && a.len() > 1 {
            // `-<digits>`: a count, as in `log -5`.
            if a[1..].chars().all(|c| c.is_ascii_digit()) && spec.alias.contains_key("-n") {
                p.values.entry("-n".into()).or_default().push(a[1..].to_string());
                continue;
            }
            let chars: Vec<char> = a[1..].chars().collect();
            let mut k = 0;
            while k < chars.len() {
                let name = format!("-{}", chars[k]);
                let key: &str = name.as_str();
                let canon = match spec.alias.iter().find(|(n, _)| **n == key) {
                    Some((_, c)) => *c,
                    None => bail!("{}", unknown(key, a)),
                };
                if spec.optional.contains(&key) {
                    let rest: String = chars[k + 1..].iter().collect();
                    p.values.entry(canon.to_string()).or_default().push(rest);
                    break;
                }
                if spec.values.contains(&key) {
                    let rest: String = chars[k + 1..].iter().collect();
                    let v = if !rest.is_empty() {
                        rest
                    } else {
                        if i >= args.len() {
                            bail!("option {key} needs a value");
                        }
                        i += 1;
                        args[i - 1].clone()
                    };
                    p.values.entry(canon.to_string()).or_default().push(v);
                    break;
                }
                *p.flags.entry(canon.to_string()).or_insert(0) += 1;
                k += 1;
            }
            continue;
        }
        p.pos.push(a.clone());
    }
    Ok(p)
}

/// The error for an argument that looks like an option and is not one. Text that starts with `-`
/// (a comment, a message) goes after `--`.
fn unknown(name: &str, arg: &str) -> String {
    if name == arg {
        format!("unknown option: {name} (put -- before an argument that starts with '-' and is not an option)")
    } else {
        format!("unknown option: {name} in '{arg}' (put -- before an argument that starts with '-' and is not an option)")
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, Spec};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn spec() -> Spec {
        Spec::new().flag("-a|--all").flag("-q").value("-m|--message").value("-n|--max-count").optional("--porcelain")
    }

    #[test]
    fn value_options_take_a_next_value_that_starts_with_a_dash() {
        for (list, want) in [
            (&["-m", "-x"][..], "-x"),
            (&["-m", "--"][..], "--"),
            (&["-m", "-- looks good"][..], "-- looks good"),
            (&["--message", "-1 for this"][..], "-1 for this"),
            (&["--message", "--all"][..], "--all"),
            (&["--message=-x"][..], "-x"),
            (&["-m-x"][..], "-x"),
            (&["-am", "-q"][..], "-q"),
        ] {
            let a = parse(&spec(), &args(list)).unwrap();
            assert_eq!(a.get("-m"), Some(want), "{list:?}");
            assert!(a.pos.is_empty() && a.paths.is_empty(), "{list:?}");
        }
        assert!(!parse(&spec(), &args(&["-m", "-q"])).unwrap().has("-q"));
    }

    #[test]
    fn dashdash_ends_the_options() {
        let a = parse(&spec(), &args(&["x", "--", "-1 for this", "--all", "--", "-q"])).unwrap();
        assert_eq!(a.pos, ["x", "-1 for this", "--all", "--", "-q"]);
        assert!(a.paths.is_empty() && a.dashdash);
        assert!(!a.has("--all") && !a.has("-q"));
        assert_eq!(a.rest(), ["x", "-1 for this", "--all", "--", "-q"]);
    }

    #[test]
    fn dashdash_paths_for_a_command_that_reads_them() {
        let a = parse(&spec().paths(), &args(&["-q", "head", "--", "-a", "src"])).unwrap();
        assert_eq!(a.pos, ["head"]);
        assert_eq!(a.paths, ["-a", "src"]);
        assert!(a.has("-q") && !a.has("-a") && a.dashdash);
        assert_eq!(a.rest(), ["head", "-a", "src"]);
    }

    #[test]
    fn flags_and_values_still_parse() {
        let a = parse(&spec(), &args(&["-aq", "-n5", "--porcelain=v2", "--max-count", "3", "pos", "-5"])).unwrap();
        assert!(a.has("-a") && a.has("-q"));
        assert_eq!(a.all("-n"), ["5", "3", "5"]);
        assert_eq!(a.get("--porcelain"), Some("v2"));
        assert_eq!(a.pos, ["pos"]);
        assert!(parse(&spec(), &args(&["--all=x"])).is_err());
        assert!(parse(&spec(), &args(&["-m"])).unwrap_err().to_string().contains("needs a value"));
    }

    #[test]
    fn text_that_looks_like_an_option_names_the_fix() {
        for list in [&["-- looks good"][..], &["-1 for this"][..], &["--nope"][..], &["-z"][..]] {
            let e = parse(&spec(), &args(list)).unwrap_err().to_string();
            assert!(e.starts_with("unknown option: "), "{e}");
            assert!(e.contains("put -- before"), "{e}");
        }
    }
}
