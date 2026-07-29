//! Path expansion shared by the config and the state file.
//!
//! Config files are hand-written, so they are allowed to say `~/.acme/client.key`,
//! `%USERPROFILE%\.acme\client.key` or `$HOME/.acme/client.key` and mean the same thing.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Expands `~`, `%VAR%` and `$VAR` in a config string.
///
/// An unset variable is left as written rather than replaced by an empty string: a path
/// that silently loses a component is far harder to diagnose than one that stays visible
/// in the error message. A `~` with no home directory to expand it against is left alone
/// for the same reason, and surfaces in the "missing key file ~/..." the caller reports.
pub fn expand(raw: &str) -> PathBuf {
    let s = expand_unix_style(&expand_windows_style(raw));

    if s == "~" || s.starts_with("~/") || s.starts_with("~\\") {
        if let Some(home) = dirs::home_dir() {
            // Joined rather than formatted through `Display`, which is lossy and would
            // corrupt a home directory that is not valid UTF-8.
            let rest = s[1..].trim_start_matches(['/', '\\']);
            return if rest.is_empty() {
                home
            } else {
                home.join(rest)
            };
        }
    }
    PathBuf::from(s)
}

/// A resolved program, and whether reaching it needs a `cmd.exe` in front.
///
/// The flag is not a detail for the caller to ignore. Once `cmd.exe` is the process being
/// spawned, it parses the whole command line before the shim does, and Rust's own quoting
/// follows C runtime rules that `cmd.exe` does not share.
pub struct Program {
    pub cmd: Command,
    pub via_shell: bool,
}

/// Builds a `Command` for a program named in the config.
///
/// On Windows this is not `Command::new(name)`. Rust's spawn only ever appends `.exe`, but
/// every agent installed through npm is a `.cmd` shim next to a `.ps1` and an extensionless
/// shell script, so `Command::new("codex")` fails with `program not found` even though the
/// same word works in any shell. The fix is to walk PATH ourselves, honour PATHEXT, and
/// hand a batch shim to `cmd /c`, which is the only thing that can run one.
pub fn program(bin: &str) -> Program {
    let expanded = expand(bin);

    // Anything holding a separator is a path the user chose; only the extension matters.
    let named_path = bin.contains('/') || bin.contains('\\');
    let found = if named_path {
        expanded.is_file().then_some(expanded.clone())
    } else {
        // The expanded name, not the raw one: `bin = "%MY_AGENT%"` is a name to resolve
        // after substitution, and searching PATH for the literal `%MY_AGENT%` never hits.
        which(&expanded.to_string_lossy())
    };

    let path = match found {
        Some(p) => p,
        // Nothing on PATH: hand the bare name over anyway so the failure names what was
        // asked for rather than a path that was invented here.
        None => {
            return Program {
                cmd: Command::new(expanded),
                via_shell: false,
            }
        }
    };

    if cfg!(windows) {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if ext == "cmd" || ext == "bat" {
            // COMSPEC rather than the bare word `cmd`, which resolves through PATH and can
            // be shadowed. `/d` skips the AutoRun value under
            // HKCU\Software\Microsoft\Command Processor: without it, whatever any installer
            // has ever written there runs first, inside every launch.
            let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
            let mut cmd = Command::new(shell);
            cmd.arg("/d").arg("/c").arg(&path);
            return Program {
                cmd,
                via_shell: true,
            };
        }
    }
    Program {
        cmd: Command::new(path),
        via_shell: false,
    }
}

/// The extensions that can actually be launched, as opposed to the ones PATHEXT lists.
///
/// A default PATHEXT also carries `.VBS`, `.JS` and often `.PS1`, and npm drops a `.ps1`
/// beside every shim. Resolving one of those hands `CreateProcess` a file it cannot run,
/// which fails with a message about the image rather than about the lookup.
const RUNNABLE_EXTS: [&str; 4] = [".com", ".exe", ".bat", ".cmd"];

/// The first match for `bin` on PATH, trying PATHEXT extensions before the bare name.
///
/// Extensions come first on purpose: npm leaves an extensionless shell script beside the
/// `.cmd`, and picking that one up would hand a bash script to CreateProcess.
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        let listed: Vec<String> = std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(|e| e.trim().to_lowercase())
            .filter(|e| RUNNABLE_EXTS.contains(&e.as_str()))
            .collect();
        if listed.is_empty() {
            RUNNABLE_EXTS.iter().map(|e| e.to_string()).collect()
        } else {
            listed
        }
    } else {
        Vec::new()
    };

    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let candidate = dir.join(format!("{bin}{ext}"));
            if is_runnable(&candidate) {
                return Some(candidate);
            }
        }
        // Off Windows only. There, the extensionless file beside a shim is npm's bash
        // script, and the whole point of trying extensions first is not to pick it up.
        if !cfg!(windows) {
            let bare = dir.join(bin);
            if is_runnable(&bare) {
                return Some(bare);
            }
        }
    }
    None
}

/// A regular file this user can actually execute.
///
/// The permission bit is what makes this more than `is_file`. A non-executable file named
/// `claude` sitting in an earlier PATH entry, a note or an unpacked tarball, otherwise ends
/// the search and turns into a `Permission denied` at spawn that names nothing useful.
fn is_runnable(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return meta.permissions().mode() & 0o111 != 0;
    }
    #[cfg(not(unix))]
    true
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_program_is_left_as_written() {
        // Not invented into some path: the error the user sees has to name what they asked
        // for, not a guess made here.
        let p = program("fastpick-no-such-binary");
        assert_eq!(
            p.cmd.get_program().to_string_lossy(),
            "fastpick-no-such-binary"
        );
        assert!(!p.via_shell);
    }

    #[cfg(windows)]
    #[test]
    fn a_batch_shim_is_handed_to_cmd() {
        // ping.exe is always there and is a real executable, so it must NOT be wrapped.
        let p = program("ping");
        assert!(!p.via_shell);

        // Every npm-installed agent is a .cmd, which CreateProcess cannot run on its own.
        let Some(shim) = which("npm") else { return };
        if shim
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("cmd"))
        {
            let p = program("npm");
            assert!(p.via_shell);
            assert!(p
                .cmd
                .get_program()
                .to_string_lossy()
                .to_lowercase()
                .ends_with("cmd.exe"));
            let args: Vec<String> = p
                .cmd
                .get_args()
                .map(|a| a.to_string_lossy().to_string())
                .collect();
            // `/d` before `/c`: without it the AutoRun registry value runs first.
            assert_eq!(args[0], "/d");
            assert_eq!(args[1], "/c");
            assert!(args[2].to_lowercase().ends_with("npm.cmd"));
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

    #[cfg(windows)]
    #[test]
    fn a_powershell_shim_is_never_resolved() {
        // `.ps1` is in many users' PATHEXT and npm writes one beside every `.cmd`, but
        // CreateProcess cannot run it and `cmd /c` cannot either.
        for name in ["npm", "npx"] {
            if let Some(found) = which(name) {
                let ext = found
                    .extension()
                    .map(|e| e.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                assert_ne!(ext, "ps1", "{} resolved to a PowerShell shim", name);
            }
        }
    }

    #[test]
    fn an_unset_variable_stays_visible_instead_of_vanishing() {
        let out = expand("$FASTPICK_NOT_SET_ANYWHERE/x").display().to_string();
        assert!(out.contains("FASTPICK_NOT_SET_ANYWHERE"));
        let out = expand("%FASTPICK_NOT_SET_ANYWHERE%/x")
            .display()
            .to_string();
        assert!(out.contains("FASTPICK_NOT_SET_ANYWHERE"));
    }

    #[test]
    fn a_tilde_becomes_the_home_directory() {
        let Some(home) = dirs::home_dir() else { return };
        let out = expand("~/.acme/key");
        assert!(out.starts_with(&home), "{}", out.display());
        assert!(out.display().to_string().ends_with("key"));
    }

    #[test]
    fn a_tilde_after_a_variable_still_expands() {
        // The variable pass runs first, so a value that itself starts with `~` is not
        // left half-expanded.
        let Some(home) = dirs::home_dir() else { return };
        std::env::set_var("FASTPICK_TEST_TILDE", "~");
        let out = expand("$FASTPICK_TEST_TILDE/x");
        std::env::remove_var("FASTPICK_TEST_TILDE");
        assert!(out.starts_with(&home), "{}", out.display());
    }
}
