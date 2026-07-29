//! Matching `.md` files in the system prompts folder to the selected model.
//!
//! The whole point of naming the files after models is that no flag has to be typed:
//! pick `orca-v4-pro` and `orca-v4.md` is already proposed. Matching is on the
//! file stem, case-insensitively, and is deliberately loose in one direction only:
//! a stem may be a prefix of the model name (`orca-v4` covers `orca-v4-pro`),
//! so one file can serve a whole family without being copied per variant.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PromptFile {
    pub path: PathBuf,
    pub stem: String,
    /// Length of the matched prefix. Longer means more specific, so it sorts first.
    pub score: usize,
}

/// Every `.md` in the folder, sorted by name. Used for the "show all" view, where a file
/// that matches nothing is still selectable by hand.
pub fn all_in(dir: &Path) -> Vec<PromptFile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PromptFile> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("md")))
        .filter_map(|e| {
            let path = e.path();
            let stem = path.file_stem()?.to_string_lossy().to_string();
            Some(PromptFile { path, stem, score: 0 })
        })
        .collect();
    out.sort_by(|a, b| a.stem.to_lowercase().cmp(&b.stem.to_lowercase()));
    out
}

/// The files that match `model`, most specific first.
pub fn matches_for(dir: &Path, model: &str) -> Vec<PromptFile> {
    let model = model.to_lowercase();
    let mut out: Vec<PromptFile> = all_in(dir)
        .into_iter()
        .filter_map(|mut f| {
            let stem = f.stem.to_lowercase();
            if stem == model {
                f.score = usize::MAX;
                return Some(f);
            }
            // A stem covering a family: `orca-v4` for `orca-v4-pro`. The dash is
            // required so `zeta-5` does not claim `zeta-52` if such a name ever appears.
            if model.starts_with(&format!("{stem}-")) {
                f.score = stem.len();
                return Some(f);
            }
            None
        })
        .collect();
    out.sort_by(|a, b| b.score.cmp(&a.score).then(a.stem.cmp(&b.stem)));
    out
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
    fn exact_name_wins_over_family_prefix() {
        let dir = fixture(&["orca-v4.md", "orca-v4-pro.md", "nova-4.5.md"]);
        let hits = matches_for(dir.path(), "orca-v4-pro");
        assert_eq!(hits[0].stem, "orca-v4-pro");
        assert_eq!(hits[1].stem, "orca-v4");
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn a_family_file_covers_its_variants() {
        let dir = fixture(&["orca-v4.md"]);
        assert_eq!(matches_for(dir.path(), "orca-v4-pro").len(), 1);
        assert_eq!(matches_for(dir.path(), "orca-v4-flash").len(), 1);
        // A different family must not be caught by it.
        assert!(matches_for(dir.path(), "orca-v3.2").is_empty());
    }

    #[test]
    fn the_dash_is_required_so_neighbours_do_not_collide() {
        let dir = fixture(&["zeta-5.md"]);
        assert_eq!(matches_for(dir.path(), "zeta-5-air").len(), 1);
        // `zeta-5.2` is a different model, not a variant of `zeta-5`.
        assert!(matches_for(dir.path(), "zeta-5.2").is_empty());
    }

    #[test]
    fn non_md_files_are_ignored() {
        let dir = fixture(&["nova-4.5.md", "nova-4.5.txt", "notes.org"]);
        assert_eq!(all_in(dir.path()).len(), 1);
    }

    #[test]
    fn a_missing_folder_is_empty_not_an_error() {
        assert!(all_in(std::path::Path::new("no/such/folder")).is_empty());
        assert!(matches_for(std::path::Path::new("no/such/folder"), "x").is_empty());
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
