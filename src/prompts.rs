//! The `.md` files in the system prompts folder.
//!
//! Nothing is guessed from the model's name. Every file in the folder is offered for every
//! model, and a model gets a file checked for it only by naming one in `prompt`. Guessing
//! used to mean a file was tied to the id it was named after: one prompt could not serve two
//! unrelated models, a file named after nothing was invisible until a key was pressed, and
//! an endpoint routing by its own scheme silently matched nothing at all. A name written in
//! the config says what was meant, and says it once.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PromptFile {
    pub path: PathBuf,
    pub stem: String,
}

/// Every `.md` in the folder, sorted by name. This is the whole list a model is offered.
pub fn all_in(dir: &Path) -> Vec<PromptFile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PromptFile> = entries
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("md"))
        })
        // A directory named `something.md` would otherwise be offered as a prompt and then
        // fail at launch, inside the agent, with an error about a file it cannot read.
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let path = e.path();
            let stem = path.file_stem()?.to_string_lossy().to_string();
            Some(PromptFile { path, stem })
        })
        .collect();
    out.sort_by_key(|p| p.stem.to_lowercase());
    out
}

/// The file a name refers to, or nothing when the folder holds no such file.
///
/// The name comes from a model's `prompt` or from `--md`, both hand-written, so the
/// extension is optional and the case does not have to be reproduced: `Orca-V4`, `orca-v4`
/// and `orca-v4.md` are the same file. Nothing else is tried, and a name matching no file is
/// the caller's to report: guessing here is what this module stopped doing.
pub fn find(dir: &Path, name: &str) -> Option<PromptFile> {
    // Split on the last dot rather than trimming three bytes, so `nova-4.5` keeps its own
    // dot and a name ending in a multi-byte character cannot be cut mid-character.
    let name = match name.rsplit_once('.') {
        Some((stem, ext)) if ext.eq_ignore_ascii_case("md") => stem,
        _ => name,
    };
    all_in(dir)
        .into_iter()
        .find(|f| f.stem.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(names: &[&str]) -> tempdir::TempDir {
        let dir = tempdir::TempDir::new();
        for n in names {
            std::fs::write(dir.path().join(n), "x").unwrap();
        }
        dir
    }

    #[test]
    fn every_file_in_the_folder_is_listed_whatever_it_is_called() {
        let dir = fixture(&["orca-v4.md", "house-style.md", "nova-4.5.md"]);
        let names: Vec<String> = all_in(dir.path()).into_iter().map(|f| f.stem).collect();
        assert_eq!(names, ["house-style", "nova-4.5", "orca-v4"]);
    }

    #[test]
    fn a_name_finds_its_file_with_or_without_the_extension() {
        let dir = fixture(&["orca-v4.md", "nova-4.5.md"]);
        assert_eq!(find(dir.path(), "orca-v4").unwrap().stem, "orca-v4");
        assert_eq!(find(dir.path(), "orca-v4.md").unwrap().stem, "orca-v4");
        assert_eq!(find(dir.path(), "Orca-V4").unwrap().stem, "orca-v4");
    }

    #[test]
    fn a_name_no_file_carries_is_not_guessed_at() {
        let dir = fixture(&["orca-v4.md"]);
        // The prefix rule this replaced would have answered `orca-v4.md` here.
        assert!(find(dir.path(), "orca-v4-pro").is_none());
        assert!(find(dir.path(), "orca").is_none());
    }

    #[test]
    fn non_md_files_are_ignored() {
        let dir = fixture(&["nova-4.5.md", "nova-4.5.txt", "notes.org"]);
        assert_eq!(all_in(dir.path()).len(), 1);
    }

    #[test]
    fn a_missing_folder_is_empty_not_an_error() {
        assert!(all_in(std::path::Path::new("no/such/folder")).is_empty());
        assert!(find(std::path::Path::new("no/such/folder"), "x").is_none());
    }
}

/// A dependency-free temporary directory, shared by the tests in every module.
#[cfg(test)]
pub mod tempdir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new() -> TempDir {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("fastpick-test-{pid}-{n}"));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
