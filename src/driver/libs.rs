//! Source-level library inclusion for reusable Lux modules.
//!
//! `use name` loads `lib/name.lux` (or a sibling/examples copy), recursively
//! expands that file's own `use` lines, and splices the library body — minus
//! `mod` and `fn main` — above the consumer. Content-addressed functions stay
//! identical whether the library is compiled alone or included.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub fn expand_uses(source: &str, source_path: &Path) -> Result<String, String> {
    expand_uses_inner(source, source_path, &mut BTreeSet::new())
}

fn expand_uses_inner(
    source: &str,
    source_path: &Path,
    seen: &mut BTreeSet<PathBuf>,
) -> Result<String, String> {
    let canonical = source_path
        .canonicalize()
        .unwrap_or_else(|_| source_path.to_path_buf());
    if !seen.insert(canonical.clone()) {
        return Ok(String::new());
    }

    let mut out = String::new();
    for line in source.lines() {
        if let Some(name) = parse_use_line(line) {
            let path = resolve_library(&name, source_path).ok_or_else(|| {
                format!(
                    "cannot resolve `use {name}` from {}",
                    source_path.display()
                )
            })?;
            let lib_source = std::fs::read_to_string(&path)
                .map_err(|e| format!("reading {}: {e}", path.display()))?;
            let expanded = expand_uses_inner(&lib_source, &path, seen)?;
            out.push_str(&format!("// ---- begin {} ----\n", path.display()));
            out.push_str(&strip_mod_and_main(&expanded));
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("// ---- end {} ----\n", path.display()));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    Ok(out)
}

fn parse_use_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("use ")?;
    if rest.contains('{') || rest.contains("::") {
        return None;
    }
    let name = rest.split_whitespace().next()?.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(name.to_string())
}

#[allow(dead_code)]
fn strip_use_lines(source: &str) -> String {
    source
        .lines()
        .filter(|line| parse_use_line(line).is_none())
        .fold(String::new(), |mut acc, line| {
            acc.push_str(line);
            acc.push('\n');
            acc
        })
}

fn resolve_library(name: &str, source_path: &Path) -> Option<PathBuf> {
    let file = format!("{name}.lux");
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let parent = source_path.parent().unwrap_or_else(|| Path::new("."));
    let candidates = [
        parent.join(&file),
        parent.join("lib").join(&file),
        parent.join("..").join("lib").join(&file),
        crate_root.join("lib").join(&file),
        crate_root.join("examples").join(&file),
        crate_root.join("stdlib").join(&file),
    ];
    let self_canon = source_path.canonicalize().ok();
    candidates.into_iter().find(|path| {
        path.is_file()
            && path
                .canonicalize()
                .ok()
                .is_some_and(|canon| self_canon.as_ref() != Some(&canon))
    })
}

fn strip_mod_and_main(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        if at_line_start(&chars, i) && starts_with(&chars, i, "mod ") {
            i = skip_line(&chars, i);
            continue;
        }
        if at_line_start(&chars, i) && starts_with(&chars, i, "fn main") {
            i = skip_function(&chars, i);
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn at_line_start(chars: &[char], i: usize) -> bool {
    i == 0 || chars[i - 1] == '\n'
}

fn starts_with(chars: &[char], i: usize, prefix: &str) -> bool {
    chars[i..].iter().copied().take(prefix.len()).eq(prefix.chars())
}

fn skip_line(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && chars[i] != '\n' {
        i += 1;
    }
    if i < chars.len() {
        i + 1
    } else {
        i
    }
}

fn skip_function(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && chars[i] != '{' {
        i += 1;
    }
    if i >= chars.len() {
        return i;
    }
    let mut depth = 0i32;
    while i < chars.len() {
        match chars[i] {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    if i < chars.len() && chars[i] == '\n' {
                        i += 1;
                    }
                    return i;
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_use() {
        assert_eq!(parse_use_line("use font").as_deref(), Some("font"));
        assert_eq!(parse_use_line("use font_atlas").as_deref(), Some("font_atlas"));
        assert_eq!(parse_use_line("use prelude::{map}").as_deref(), None);
    }
}
