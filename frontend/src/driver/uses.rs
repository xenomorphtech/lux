//! Source-level `use` expansion over an abstract source store.
//!
//! `use name` splices the named library's body — minus its `mod` line and
//! `fn main` — above the consumer, recursively expanding the library's own
//! `use` lines. Content-addressed functions stay identical whether a library
//! is compiled alone or included, and the splice markers are comments (the
//! lexer strips them), so they never reach a fingerprint.
//!
//! Where the source text comes from is the host's business: the `lux` crate
//! provides a filesystem `SourceProvider`; the Yggdrasil kernel provides one
//! over a boot-shipped source pack.

#[allow(unused_imports)]
use crate::prelude::*;
use alloc::borrow::Cow;
use alloc::collections::BTreeSet;

/// Resolves a bare `use <name>` to that library's source text plus a
/// provider-defined identity for the resolved unit. `from` is the identity of
/// the file whose `use` line is being resolved (None for the root program):
/// filesystem providers use canonical paths as identities and resolve
/// relative to the includer's directory — so an example that shadows a lib
/// name can still reach the lib through its own name — while flat stores
/// (the kernel's source pack) just use the name itself.
pub trait SourceProvider {
    fn source(&self, name: &str, from: Option<&str>) -> Option<(Cow<'_, str>, String)>;
}

/// Expand every `use` line of `source`, fetching library text from
/// `provider`. Each resolved unit (by provider identity) is included at most
/// once; self- and cyclic references are silently skipped, matching the old
/// path-canonicalization guard.
pub fn expand_uses_with(source: &str, provider: &dyn SourceProvider) -> Result<String, String> {
    let mut seen = BTreeSet::new();
    expand_inner(source, None, provider, &mut seen)
}

fn expand_inner(
    source: &str,
    from: Option<&str>,
    provider: &dyn SourceProvider,
    seen: &mut BTreeSet<String>,
) -> Result<String, String> {
    let mut out = String::new();
    for line in source.lines() {
        if let Some(name) = parse_use_line(line) {
            let (lib_source, id) = provider
                .source(&name, from)
                .ok_or_else(|| format!("cannot resolve `use {name}`"))?;
            if !seen.insert(id.clone()) {
                continue;
            }
            let expanded = expand_inner(&lib_source, Some(&id), provider, seen)?;
            out.push_str(&format!("// ---- begin {name} ----\n"));
            out.push_str(&strip_mod_and_main(&expanded));
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("// ---- end {name} ----\n"));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    Ok(out)
}

/// Accepts only bare `use <ident>`; `use a::{b}` forms are left for the
/// parser as `Item::Use`.
pub fn parse_use_line(line: &str) -> Option<String> {
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

/// Drop a library's `mod` line and its `fn main` (with body) so the splice
/// contributes only definitions.
pub fn strip_mod_and_main(source: &str) -> String {
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
    if i < chars.len() { i + 1 } else { i }
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

    struct MapProvider(&'static [(&'static str, &'static str)]);
    impl SourceProvider for MapProvider {
        fn source(&self, name: &str, _from: Option<&str>) -> Option<(Cow<'_, str>, String)> {
            self.0
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, s)| (Cow::Borrowed(*s), String::from(name)))
        }
    }

    #[test]
    fn parse_simple_use() {
        assert_eq!(parse_use_line("use font").as_deref(), Some("font"));
        assert_eq!(parse_use_line("use font_atlas").as_deref(), Some("font_atlas"));
        assert_eq!(parse_use_line("use prelude::{map}").as_deref(), None);
    }

    #[test]
    fn expands_recursively_and_once() {
        let p = MapProvider(&[
            ("a", "mod a\nuse b\nfn fa() -> Int { fb() }\nfn main() -> Int { 0 }\n"),
            ("b", "mod b\nfn fb() -> Int { 2 }\n"),
        ]);
        let out = expand_uses_with("use a\nuse b\nfn main() -> Int { fa() }\n", &p).unwrap();
        assert_eq!(out.matches("fn fb").count(), 1, "b included once: {out}");
        assert!(out.contains("fn fa"));
        assert_eq!(out.matches("fn main").count(), 1, "lib mains stripped");
    }

    #[test]
    fn cycles_are_skipped() {
        let p = MapProvider(&[("x", "mod x\nuse x\nfn fx() -> Int { 1 }\n")]);
        let out = expand_uses_with("use x\n", &p).unwrap();
        assert!(out.contains("fn fx"));
    }
}
