//! `node:util` `parseArgs` semantics (strict and loose) for the goon command line.

use anyhow::{bail, Result};
use indexmap::IndexMap;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bool,
    Str,
}

pub struct OptSpec {
    pub name: &'static str,
    pub kind: Kind,
    pub short: Option<char>,
    pub multiple: bool,
}

const fn b(name: &'static str, short: Option<char>) -> OptSpec {
    OptSpec { name, kind: Kind::Bool, short, multiple: false }
}

const fn s(name: &'static str, short: Option<char>) -> OptSpec {
    OptSpec { name, kind: Kind::Str, short, multiple: false }
}

const fn m(name: &'static str) -> OptSpec {
    OptSpec { name, kind: Kind::Str, short: None, multiple: true }
}

pub const MAIN_OPTIONS: &[OptSpec] = &[
    b("help", Some('h')),
    b("version", Some('v')),
    s("prompt", Some('p')),
    s("cwd", Some('C')),
    s("provider", None),
    s("base-url", None),
    s("model", None),
    s("effort", None),
    s("key-env", None),
    s("resume", None),
    b("pair", None),
    b("yes", None),
    b("plan", None),
    b("read-only", None),
    b("json", None),
    b("allow-net", None),
    m("allow-net-hosts"),
    b("no-tui", None),
    b("no-mcp", None),
    b("no-subagents", None),
    s("inline-reasoning", None),
    b("show-reasoning", None),
    s("context-window", None),
    s("max-output-tokens", None),
    s("run-token-budget", None),
    s("timeout-ms", None),
    s("max-cost-usd", None),
    s("max-time-ms", None),
    s("output-last-message", Some('o')),
    b("no-verify", None),
    m("verify-cmd"),
    b("event-log", None),
    b("no-event-log", None),
    b("stub", None),
    s("stub-fixture", None),
    b("envelopes", None),
    b("rebuild-session", None),
];

pub const MCP_ADD_OPTIONS: &[OptSpec] = &[s("url", None), s("bearer-env", None), s("bearer-token-env-var", None), m("env"), b("trust", None), s("timeout-ms", None)];

#[derive(Clone, Debug, PartialEq)]
pub enum OptValue {
    Bool(bool),
    Str(String),
    List(Vec<String>),
}

#[derive(Default, Debug)]
pub struct Parsed {
    pub values: IndexMap<String, OptValue>,
    pub positionals: Vec<String>,
}

impl Parsed {
    pub fn flag(&self, name: &str) -> bool {
        matches!(self.values.get(name), Some(OptValue::Bool(true)))
    }

    pub fn has(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }

    pub fn str(&self, name: &str) -> Option<String> {
        match self.values.get(name) {
            Some(OptValue::Str(s)) => Some(s.clone()),
            _ => None,
        }
    }

    pub fn list(&self, name: &str) -> Vec<String> {
        match self.values.get(name) {
            Some(OptValue::List(l)) => l.clone(),
            Some(OptValue::Str(s)) => vec![s.clone()],
            _ => vec![],
        }
    }
}

fn spec_by_name<'a>(specs: &'a [OptSpec], name: &str) -> Option<&'a OptSpec> {
    specs.iter().find(|s| s.name == name)
}

fn spec_by_short(specs: &[OptSpec], short: char) -> Option<&OptSpec> {
    specs.iter().find(|s| s.short == Some(short))
}

fn unknown(token: &str) -> anyhow::Error {
    anyhow::anyhow!("Unknown option '{token}'. To specify a positional argument starting with a '-', place it at the end of the command after '--', as in '-- \"{token}\"")
}

fn option_label(spec: &OptSpec) -> String {
    match spec.short {
        Some(c) => format!("-{c}, --{}", spec.name),
        None => format!("--{}", spec.name),
    }
}

fn store(parsed: &mut Parsed, spec: &OptSpec, value: OptValue) {
    if spec.multiple {
        let OptValue::Str(v) = value else { return };
        match parsed.values.get_mut(spec.name) {
            Some(OptValue::List(list)) => list.push(v),
            _ => {
                parsed.values.insert(spec.name.to_string(), OptValue::List(vec![v]));
            }
        }
    } else {
        parsed.values.insert(spec.name.to_string(), value);
    }
}

fn option_like(value: &str) -> bool {
    value.len() > 1 && value.starts_with('-')
}

/// Strict parsing with positionals allowed (the main command line).
pub fn parse_args(args: &[String], specs: &[OptSpec], strict: bool) -> Result<Parsed> {
    let mut parsed = Parsed::default();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            parsed.positionals.extend(args[i + 1..].iter().cloned());
            break;
        }
        if let Some(body) = arg.strip_prefix("--") {
            let (name, inline) = match body.find('=') {
                Some(eq) => (&body[..eq], Some(body[eq + 1..].to_string())),
                None => (body, None),
            };
            let Some(spec) = spec_by_name(specs, name) else {
                if strict {
                    return Err(unknown(&format!("--{name}")));
                }
                parsed.values.insert(name.to_string(), inline.map(OptValue::Str).unwrap_or(OptValue::Bool(true)));
                i += 1;
                continue;
            };
            match spec.kind {
                Kind::Bool => {
                    if inline.is_some() && strict {
                        bail!("Option '--{name}' does not take an argument");
                    }
                    store(&mut parsed, spec, OptValue::Bool(true));
                    i += 1;
                }
                Kind::Str => {
                    if let Some(v) = inline {
                        store(&mut parsed, spec, OptValue::Str(v));
                        i += 1;
                        continue;
                    }
                    let Some(next) = args.get(i + 1) else {
                        if strict {
                            bail!("Option '{} <value>' argument missing", option_label(spec));
                        }
                        i += 1;
                        continue;
                    };
                    if strict && option_like(next) {
                        bail!("Option '--{name}' argument is ambiguous.\nDid you forget to specify the option argument for '--{name}'?\nTo specify an option argument starting with a dash use '--{name}=-XYZ'.");
                    }
                    store(&mut parsed, spec, OptValue::Str(next.clone()));
                    i += 2;
                }
            }
            continue;
        }
        if arg.len() > 1 && arg.starts_with('-') {
            let chars: Vec<char> = arg[1..].chars().collect();
            let mut j = 0;
            let mut consumed_next = false;
            while j < chars.len() {
                let c = chars[j];
                let Some(spec) = spec_by_short(specs, c) else {
                    if strict {
                        return Err(unknown(&format!("-{c}")));
                    }
                    parsed.values.insert(c.to_string(), OptValue::Bool(true));
                    j += 1;
                    continue;
                };
                match spec.kind {
                    Kind::Bool => {
                        store(&mut parsed, spec, OptValue::Bool(true));
                        j += 1;
                    }
                    Kind::Str => {
                        let rest: String = chars[j + 1..].iter().collect();
                        if !rest.is_empty() {
                            store(&mut parsed, spec, OptValue::Str(rest));
                        } else {
                            let Some(next) = args.get(i + 1) else {
                                if strict {
                                    bail!("Option '{} <value>' argument missing", option_label(spec));
                                }
                                break;
                            };
                            if strict && option_like(next) {
                                bail!("Option '-{c}' argument is ambiguous.\nDid you forget to specify the option argument for '-{c}'?\nTo specify an option argument starting with a dash use '--{}=-XYZ' or '-{c}-XYZ'.", spec.name);
                            }
                            store(&mut parsed, spec, OptValue::Str(next.clone()));
                            consumed_next = true;
                        }
                        break;
                    }
                }
            }
            i += if consumed_next { 2 } else { 1 };
            continue;
        }
        parsed.positionals.push(arg.clone());
        i += 1;
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Parsed> {
        parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>(), MAIN_OPTIONS, true)
    }

    #[test]
    fn mirrors_node_parse_args() {
        assert!(p(&["--nope"]).unwrap_err().to_string().starts_with("Unknown option '--nope'."));
        assert_eq!(p(&["-p"]).unwrap_err().to_string(), "Option '-p, --prompt <value>' argument missing");
        assert!(p(&["-p", "-h"]).unwrap_err().to_string().starts_with("Option '-p' argument is ambiguous."));
        assert_eq!(p(&["--yes=1"]).unwrap_err().to_string(), "Option '--yes' does not take an argument");
        let r = p(&["-hp", "x"]).unwrap();
        assert!(r.flag("help"));
        assert_eq!(r.str("prompt").unwrap(), "x");
        assert_eq!(p(&["-pabc"]).unwrap().str("prompt").unwrap(), "abc");
        assert_eq!(p(&["--prompt=-x"]).unwrap().str("prompt").unwrap(), "-x");
        assert_eq!(p(&["a", "--", "--yes"]).unwrap().positionals, vec!["a", "--yes"]);
        assert_eq!(p(&["--allow-net-hosts", "a", "--allow-net-hosts=b"]).unwrap().list("allow-net-hosts"), vec!["a", "b"]);
        assert_eq!(p(&["-"]).unwrap().positionals, vec!["-"]);
    }
}
