//! Reusable prompt files ("custom slash commands"): `review-pr.md` becomes `/prompts:review-pr`.
//! `$ARGUMENTS` is replaced by everything typed after the command, `$1`..`$9` by single words and
//! `$$` by a literal dollar sign.

use crate::config::config_dir;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq)]
pub struct PromptFile {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub scope: &'static str,
    pub body: String,
}

const MAX_PROMPT_BYTES: usize = 64 * 1024;

pub fn prompt_dirs(cwd: &Path) -> Vec<(PathBuf, &'static str)> {
    let mut dirs = vec![(cwd.join(".goon").join("prompts"), "workspace"), (cwd.join(".agents").join("prompts"), "workspace"), (config_dir().join("prompts"), "user")];
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty()).or_else(|| std::env::var("USERPROFILE").ok().filter(|h| !h.is_empty()));
    if let Some(home) = home {
        dirs.push((PathBuf::from(home).join(".codex").join("prompts"), "user"));
    }
    dirs
}

fn parse_front_matter(text: &str) -> (Option<String>, String) {
    static FM: OnceLock<Regex> = OnceLock::new();
    static DESC: OnceLock<Regex> = OnceLock::new();
    let fm = FM.get_or_init(|| Regex::new(r"^---\r?\n((?s).*?)\r?\n---\r?\n?").unwrap());
    let desc = DESC.get_or_init(|| Regex::new(r"(?m)^description\s*:\s*(.+)$").unwrap());
    let Some(m) = fm.captures(text) else { return (None, text.to_string()) };
    let description = desc.captures(&m[1]).map(|c| {
        let mut v = c[1].trim().to_string();
        if v.starts_with('"') || v.starts_with('\'') {
            v.remove(0);
        }
        if v.ends_with('"') || v.ends_with('\'') {
            v.pop();
        }
        v
    });
    (description.filter(|d| !d.is_empty()), text[m.get(0).unwrap().end()..].to_string())
}

pub fn discover_prompts(cwd: &Path) -> Vec<PromptFile> {
    static NAME: OnceLock<Regex> = OnceLock::new();
    let name_re = NAME.get_or_init(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$").unwrap());
    let mut seen = std::collections::HashSet::new();
    let mut prompts = vec![];
    for (dir, scope) in prompt_dirs(cwd) {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by(|a, b| crate::jsutil::locale_cmp(&a.file_name().to_string_lossy(), &b.file_name().to_string_lossy()));
        for entry in entries {
            let file_name = entry.file_name().to_string_lossy().to_string();
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) || !file_name.to_lowercase().ends_with(".md") {
                continue;
            }
            let name = file_name[..file_name.rfind('.').unwrap()].to_string();
            if !name_re.is_match(&name) || seen.contains(&name.to_lowercase()) {
                continue;
            }
            let path = dir.join(&file_name);
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            if text.len() > MAX_PROMPT_BYTES {
                continue;
            }
            let (description, body) = parse_front_matter(&text);
            let first = body.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).find(|l| !l.trim().is_empty()).map(|l| l.trim().to_string()).unwrap_or_default();
            seen.insert(name.to_lowercase());
            prompts.push(PromptFile { name, description: description.unwrap_or_else(|| crate::jsutil::take_chars(&first, 100).to_string()), path, scope, body: body.trim().to_string() });
        }
    }
    prompts
}

pub fn expand_prompt_template(body: &str, args: &str) -> String {
    static TOKEN: OnceLock<Regex> = OnceLock::new();
    static PLACEHOLDER: OnceLock<Regex> = OnceLock::new();
    let token = TOKEN.get_or_init(|| Regex::new(r"\$(\$|ARGUMENTS|[1-9])").unwrap());
    let placeholder = PLACEHOLDER.get_or_init(|| Regex::new(r"\$(?:ARGUMENTS|[1-9])").unwrap());
    let trimmed = args.trim();
    let words: Vec<&str> = if trimmed.is_empty() { vec![] } else { trimmed.split_whitespace().collect() };
    let expanded = token
        .replace_all(body, |c: &regex::Captures| match &c[1] {
            "$" => "$".to_string(),
            "ARGUMENTS" => trimmed.to_string(),
            n => words.get(n.parse::<usize>().unwrap() - 1).map(|w| w.to_string()).unwrap_or_default(),
        })
        .to_string();
    if !placeholder.is_match(body) && !trimmed.is_empty() {
        format!("{expanded}\n\n{trimmed}")
    } else {
        expanded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates() {
        assert_eq!(expand_prompt_template("Review $1 then $ARGUMENTS ($$5)", "a b"), "Review a then a b ($5)");
        assert_eq!(expand_prompt_template("Plain", "extra"), "Plain\n\nextra");
        assert_eq!(parse_front_matter("---\ndescription: \"Hi\"\n---\nbody"), (Some("Hi".into()), "body".into()));
    }
}
