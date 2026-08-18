//! Filesystem `SourceProvider`: resolves `use name` the way the old expander
//! did — relative to the *including file's* directory, with the includer
//! excluded as a candidate (so an example shadowing a lib name still reaches
//! the lib through its own name), deduped by canonical path. The expansion
//! logic itself lives in `lux_frontend::driver::uses`.

use std::path::{Path, PathBuf};

use lux_frontend::driver::uses::{self, SourceProvider};

pub struct FsProvider {
    /// The root program file; the identity for `from = None` resolutions.
    root: PathBuf,
}

impl FsProvider {
    pub fn for_program(source_path: &Path) -> FsProvider {
        FsProvider {
            root: source_path.to_path_buf(),
        }
    }
}

impl SourceProvider for FsProvider {
    fn source(&self, name: &str, from: Option<&str>) -> Option<(std::borrow::Cow<'_, str>, String)> {
        let file = format!("{name}.lux");
        let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let includer: PathBuf = match from {
            Some(path) => PathBuf::from(path),
            None => self.root.clone(),
        };
        let parent = includer.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
        let candidates = [
            parent.join(&file),
            parent.join("lib").join(&file),
            parent.join("..").join("lib").join(&file),
            crate_root.join("lib").join(&file),
            crate_root.join("examples").join(&file),
            crate_root.join("stdlib").join(&file),
        ];
        let includer_canon = includer.canonicalize().ok();
        let path = candidates.into_iter().find_map(|path| {
            if !path.is_file() {
                return None;
            }
            let canon = path.canonicalize().ok()?;
            if includer_canon.as_ref() == Some(&canon) {
                return None;
            }
            Some(canon)
        })?;
        let text = std::fs::read_to_string(&path).ok()?;
        let id = path.to_string_lossy().into_owned();
        Some((std::borrow::Cow::Owned(text), id))
    }
}

pub fn expand_uses(source: &str, source_path: &Path) -> Result<String, String> {
    uses::expand_uses_with(source, &FsProvider::for_program(source_path))
}
