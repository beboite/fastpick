//! Path expansion shared by the config and the state file.
//!
//! Config files are hand-written, so they are allowed to say `~/.acme/client.key`,
//! `%USERPROFILE%\.acme\client.key` or `$HOME/.acme/client.key` and mean the same thing.

use std::path::PathBuf;
use std::process::Command;

/// Expands `~`, `%VAR%` and `$VAR` in a config string.
///
/// An unset variable is left as written rather than replaced by an empty string: a path
/// that silently loses a component is far harder to diagnose than one that stays visible
/// in the error message.
pub fn expand(raw: &str) -> PathBuf {
    let mut s = raw.to_string();

    if s == "~" || s.starts_with("~/") || s.starts_with("~\\") {
        if let Some(home) = dirs::home_dir() {
            s = format!("{}{}", home.display(), &s[1..]);
        }
    }

    s = expand_windows_style(&s);
    s = expand_unix_style(&s);
    PathBuf::from(s)
}

/// Builds a `Command` for a program named in the config.
///
/// On Windows this is not `Command::new(name)`. Rust's spawn only ever appends `.exe`, but
/// every agent installed through npm is a `.cmd` shim next to a `.ps1` and an extensionless
/// shell script, so `Command::new("codex")` fails with `program not found` even though the
/// same word works in any shell. The fix is to walk PATH ourselves, honour PATHEXT, and
/// hand a batch shim to `cmd /c`, which is the only thing that can run one.
pub fn program(bin: &str) -> Command {
    let expanded = expand(bin);

    // Anything holding a separator is a path the user chose; only the extension matters.
    let named_path = bin.contains('/') || bin.contains('\\');
    let found = if named_path {
        expanded.exists().then_some(expanded.clone())
    } else {
        which(bin)
    };

    let path = match found {
        Some(p) => p,
        // Nothing on PATH: hand the bare name over anyway so the failure names what was
        // asked for rather than a path that was invented here.
        None => return Command::new(expanded),
    };

    if cfg!(windows) {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if ext == "cmd" || ext == "bat" {
            let mut cmd = Command::new("cmd");
            cmd.arg("/c").arg(&path);
            return cmd;
        }
    }
    Command::new(path)
}

/// The first match for `bin` on PATH, trying PATHEXT extensions before the bare name.
///
/// Extensions come first on purpose: npm leaves an extensionless shell script beside the
/// `.cmd`, and picking that one up would hand a bash script to CreateProcess.
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| e.to_lowercase())
            .collect()
    } else {
        Vec::new()
    };

    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let candidate = dir.join(format!("{bin}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        let bare = dir.join(bin);
        if bare.is_file() {
            return Some(bare);
        }
    }
    None
}

fn expand_windows_style(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                out.push_str(&rest[..start]);
                match std::env::var(name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_program_is_left_as_written() {
        // Not invented into some path: the error the user sees has to name what they asked
        // for, not a guess made here.
        let cmd = program("fastpick-no-such-binary");
        assert_eq!(
            cmd.get_program().to_string_lossy(),
            "fastpick-no-such-binary"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_batch_shim_is_handed_to_cmd() {
        // ping.exe is always there and is a real executable, so it must NOT be wrapped.
        let cmd = program("ping");
        assert_ne!(cmd.get_program().to_string_lossy(), "cmd");

        // Every npm-installed agent is a .cmd, which CreateProcess cannot run on its own.
        let Some(shim) = which("npm") else { return };
        if shim.extension().is_some_and(|e| e.eq_ignore_ascii_case("cmd")) {
            let cmd = program("npm");
            assert_eq!(cmd.get_program().to_string_lossy(), "cmd");
            let args: Vec<String> = cmd
                .get_args()
                .map(|a| a.to_string_lossy().to_string())
                .collect();
            assert_eq!(args[0], "/c");
            assert!(args[1].to_lowercase().ends_with("npm.cmd"));
        }
    }

    #[cfg(windows)]
    #[test]
    fn an_extension_beats_the_extensionless_shell_script_beside_it() {
        // npm leaves three files per binary: `npm`, `npm.cmd` and `npm.ps1`. The first is a
        // bash script, and picking it would hand shell source to CreateProcess.
        let Some(found) = which("npm") else { return };
        assert!(
            found.extension().is_some(),
            "resolved the extensionless shim: {}",
            found.display()
        );
    }

    #[test]
    fn an_unset_variable_stays_visible_instead_of_vanishing() {
        let out = expand("$FASTPICK_NOT_SET_ANYWHERE/x").display().to_string();
        assert!(out.contains("FASTPICK_NOT_SET_ANYWHERE"));
        let out = expand("%FASTPICK_NOT_SET_ANYWHERE%/x").display().to_string();
        assert!(out.contains("FASTPICK_NOT_SET_ANYWHERE"));
    }

    #[test]
    fn a_tilde_becomes_the_home_directory() {
        let Some(home) = dirs::home_dir() else { return };
        let out = expand("~/.acme/key");
        assert!(out.starts_with(&home), "{}", out.display());
        assert!(out.display().to_string().ends_with("key"));
    }
}

fn expand_unix_style(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let start = i + 1;
        let mut end = start;
        for (j, c2) in s[start..].char_indices() {
            if c2.is_alphanumeric() || c2 == '_' {
                end = start + j + c2.len_utf8();
            } else {
                break;
            }
        }
        if end == start {
            out.push('$');
            continue;
        }
        let name = &s[start..end];
        match std::env::var(name) {
            Ok(v) => out.push_str(&v),
            Err(_) => {
                out.push('$');
                out.push_str(name);
            }
        }
        while let Some(&(j, _)) = chars.peek() {
            if j < end {
                chars.next();
            } else {
                break;
            }
        }
    }
    out
}
