//! The one flag parser every subcommand shares.
//!
//! Each command declares its flags as a table of [`Flag`]s; [`parse`] turns
//! argv into an ordered list of [`Arg`]s (order matters for repeatable flags
//! and for "last positional wins"). Rules:
//!   - `--flag value` and `--flag=value` are equivalent for value flags;
//!   - `--flag=` is an explicit empty value, distinct from a missing one
//!     (`--flag` as the last argument), which is an error;
//!   - a switch given a value (`--json=1`) is an error;
//!   - an unknown flag is an error naming it;
//!   - `-` is a positional (the stdin marker), and `--` ends flag parsing.

/// One accepted flag. `names[0]` is canonical; the rest are aliases.
#[derive(Clone, Copy)]
pub struct Flag {
    pub names: &'static [&'static str],
    pub takes_value: bool,
}

impl Flag {
    pub const fn switch(names: &'static [&'static str]) -> Self {
        Flag {
            names,
            takes_value: false,
        }
    }

    pub const fn value(names: &'static [&'static str]) -> Self {
        Flag {
            names,
            takes_value: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    /// `name` is the canonical spelling; `value` is `None` for switches.
    Flag {
        name: &'static str,
        value: Option<String>,
    },
    Pos(String),
}

/// Parsed argv, in order.
#[derive(Debug, Default)]
pub struct Parsed {
    pub args: Vec<Arg>,
}

impl Parsed {
    /// The last value given for `name`.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.args.iter().rev().find_map(|a| match a {
            Arg::Flag {
                name: n,
                value: Some(v),
            } if *n == name => Some(v.as_str()),
            _ => None,
        })
    }

    pub fn positionals(&self) -> Vec<&str> {
        self.args
            .iter()
            .filter_map(|a| match a {
                Arg::Pos(p) => Some(p.as_str()),
                Arg::Flag { .. } => None,
            })
            .collect()
    }
}

pub fn parse(argv: &[String], flags: &[Flag]) -> Result<Parsed, String> {
    let lookup = |name: &str| flags.iter().find(|f| f.names.contains(&name));
    let mut out = Vec::new();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            out.extend(it.by_ref().map(|p| Arg::Pos(p.clone())));
            break;
        }
        if a == "-" || !a.starts_with('-') {
            out.push(Arg::Pos(a.clone()));
            continue;
        }
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if a.starts_with("--") => (n, Some(v.to_string())),
            _ => (a.as_str(), None),
        };
        let flag = lookup(name).ok_or_else(|| format!("unknown flag '{name}'"))?;
        let value = match (flag.takes_value, inline) {
            (true, Some(v)) => Some(v),
            (true, None) => Some(
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("flag '{name}' needs a value"))?,
            ),
            (false, Some(_)) => return Err(format!("flag '{name}' takes no value (got '{a}')")),
            (false, None) => None,
        };
        out.push(Arg::Flag {
            name: flag.names[0],
            value,
        });
    }
    Ok(Parsed { args: out })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLAGS: &[Flag] = &[
        Flag::switch(&["--json"]),
        Flag::switch(&["--continue", "-c"]),
        Flag::value(&["--model"]),
        Flag::value(&["--dir", "-d"]),
    ];

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn switch(name: &'static str) -> Arg {
        Arg::Flag { name, value: None }
    }

    fn p(v: &[&str]) -> Result<Parsed, String> {
        parse(&argv(v), FLAGS)
    }

    #[test]
    fn both_value_syntaxes_are_equivalent() {
        let a = p(&["--model", "m1"]).unwrap();
        let b = p(&["--model=m1"]).unwrap();
        assert_eq!(a.args, b.args);
        assert_eq!(a.value("--model"), Some("m1"));
        // Only the first '=' splits: the value keeps the rest.
        assert_eq!(p(&["--model=a=b"]).unwrap().value("--model"), Some("a=b"));
        // A separate value is taken verbatim, even when it looks like a flag.
        assert_eq!(
            p(&["--model", "--json"]).unwrap().value("--model"),
            Some("--json")
        );
    }

    #[test]
    fn missing_value_is_an_error_but_empty_is_a_value() {
        let e = p(&["--model"]).unwrap_err();
        assert_eq!(e, "flag '--model' needs a value");
        let empty = p(&["--model="]).unwrap();
        assert_eq!(empty.value("--model"), Some(""));
        assert_eq!(p(&["--model", ""]).unwrap().value("--model"), Some(""));
    }

    #[test]
    fn unknown_flags_and_valued_switches_are_errors() {
        assert_eq!(p(&["--bogus"]).unwrap_err(), "unknown flag '--bogus'");
        assert_eq!(p(&["--bogus=1"]).unwrap_err(), "unknown flag '--bogus'");
        assert_eq!(p(&["-x"]).unwrap_err(), "unknown flag '-x'");
        let e = p(&["--json=1"]).unwrap_err();
        assert!(e.contains("takes no value"), "{e}");
    }

    #[test]
    fn aliases_map_to_the_canonical_name() {
        let a = p(&["-c", "-d", "x"]).unwrap();
        assert!(a.args.contains(&switch("--continue")));
        assert_eq!(a.value("--dir"), Some("x"));
    }

    #[test]
    fn positionals_keep_order_and_the_stdin_marker() {
        let a = p(&["status", "--dir", "/tmp/x", "-", "b"]).unwrap();
        assert_eq!(a.positionals(), vec!["status", "-", "b"]);
    }

    #[test]
    fn double_dash_ends_flag_parsing() {
        let a = p(&["--json", "--", "--model", "-c"]).unwrap();
        assert!(a.args.contains(&switch("--json")));
        assert_eq!(a.value("--model"), None);
        assert_eq!(a.positionals(), vec!["--model", "-c"]);
    }

    #[test]
    fn last_value_wins_and_repeats_are_kept_in_order() {
        let a = p(&["--model", "a", "--model=b"]).unwrap();
        assert_eq!(a.value("--model"), Some("b"));
        assert_eq!(a.args.len(), 2);
    }
}
