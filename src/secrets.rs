//! Key files: where they are, who else can read them, and how to write one.
//!
//! A credential lives in its own file rather than in the config, so the config stays
//! something you can paste into an issue. That only helps if the file itself is closed:
//! written owner-only, reported when it is not, and never taken from a command line, where
//! it would land in the shell history of every machine it is typed on.

use anyhow::{anyhow, Context, Result};
use std::io::{IsTerminal, Write};
use std::path::Path;

/// Who can read a key file.
// The mode-bit arms are unreachable on Windows, where there are none to read.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub enum Access {
    Missing,
    OwnerOnly,
    /// Readable by the group or by everyone, with the offending mode.
    Shared(u32),
    /// Windows, where there are no mode bits to read: the file inherits the ACL of the
    /// user profile it sits under, which is the same protection the agent's own credentials
    /// get. Claiming more than that would be a lie.
    #[cfg_attr(unix, allow(dead_code))]
    Unchecked,
}

impl Access {
    pub fn label(&self) -> String {
        match self {
            Access::Missing => "missing".to_string(),
            Access::OwnerOnly => "owner only".to_string(),
            Access::Shared(mode) => format!("readable by others ({:o}), run chmod 600", mode),
            Access::Unchecked => "inherits the profile permissions".to_string(),
        }
    }

    pub fn is_problem(&self) -> bool {
        matches!(self, Access::Shared(_))
    }
}

#[cfg(unix)]
pub fn access(path: &Path) -> Access {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return Access::Missing;
    };
    let mode = meta.permissions().mode() & 0o777;
    match mode & 0o077 {
        0 => Access::OwnerOnly,
        _ => Access::Shared(mode),
    }
}

#[cfg(not(unix))]
pub fn access(path: &Path) -> Access {
    match path.is_file() {
        true => Access::Unchecked,
        false => Access::Missing,
    }
}

/// Writes a secret to its own file, readable by nobody else from the moment it exists.
///
/// The mode is set as the file is created rather than after: a `chmod` that follows a write
/// leaves a window where the key is on disk and world-readable.
pub fn write(path: &Path, secret: &str) -> Result<()> {
    let secret = secret.trim();
    if secret.is_empty() {
        return Err(anyhow!("nothing to write: the key is empty"));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        harden_dir(parent);
    }

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    writeln!(f, "{secret}").with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
pub fn harden_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
pub fn harden_dir(_dir: &Path) {}

/// Reads a secret from stdin: without echo when a person is typing it, plainly when it is
/// piped in, so `pass show x | fastpick --set-key x` works too.
pub fn read_secret(prompt: &str) -> Result<String> {
    let mut stderr = std::io::stderr();
    if !std::io::stdin().is_terminal() {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("reading the key from stdin")?;
        return Ok(line.trim().to_string());
    }

    // The prompt goes to stderr: stdout stays a channel a caller can parse.
    write!(stderr, "{prompt}")?;
    stderr.flush()?;
    let typed = read_hidden();
    writeln!(stderr)?;
    typed
}

fn read_hidden() -> Result<String> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

    crossterm::terminal::enable_raw_mode().context("switching the terminal to raw mode")?;
    let mut out = String::new();
    let result = loop {
        match event::read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => match k.code {
                KeyCode::Enter => break Ok(std::mem::take(&mut out)),
                KeyCode::Backspace => {
                    out.pop();
                }
                KeyCode::Esc => break Err(anyhow!("cancelled")),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err(anyhow!("cancelled"))
                }
                KeyCode::Char(c) => out.push(c),
                _ => {}
            },
            Ok(_) => {}
            Err(e) => break Err(e.into()),
        }
    };
    let _ = crossterm::terminal::disable_raw_mode();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompts::tempdir::TempDir;

    #[test]
    fn a_written_key_is_readable_back_and_closed_to_others() {
        let dir = TempDir::new();
        let path = dir.path().join("nested").join("client.key");
        write(&path, "  sk-secret\n").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "sk-secret");
        assert!(!access(&path).is_problem(), "{}", access(&path).label());
        assert_ne!(access(&path), Access::Missing);
    }

    #[test]
    fn an_empty_key_is_refused_rather_than_written() {
        let dir = TempDir::new();
        let path = dir.path().join("client.key");
        assert!(write(&path, "   ").is_err());
        assert!(!path.exists());
        assert_eq!(access(&path), Access::Missing);
    }

    #[cfg(unix)]
    #[test]
    fn a_world_readable_key_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let path = dir.path().join("client.key");
        write(&path, "sk-secret").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(access(&path).is_problem());
        assert!(access(&path).label().contains("chmod"));
    }
}
