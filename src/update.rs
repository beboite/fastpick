//! Noticing that a newer release exists, and replacing this binary with it on request.
//!
//! Split in two on purpose. The check is passive: it runs at most once a day, on a
//! background thread, and its only effect is one line at the bottom of the menu. The
//! install happens when the user types `fastpick --update` and never before. A tool whose
//! whole job is to launch someone's coding session has no business rewriting itself in the
//! middle of one.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const REPO: &str = "beboite/fastpick";

/// How long a check stays good. A picker is opened many times a day and the answer moves
/// at the speed of releases, so anything shorter is only noise on someone's network.
const CHECK_EVERY_SECS: u64 = 24 * 60 * 60;

/// The minisign public key every release asset is signed with.
///
/// Empty until the key pair exists. While it is empty `--update` refuses to install rather
/// than falling back to "https was probably fine": an unverified binary that replaces this
/// one is the whole attack, and a soft failure here would be the wrong default forever.
///
/// Generate with `minisign -G -p fastpick.pub -s fastpick.key`, paste the second line of
/// `fastpick.pub` here, and put the contents of `fastpick.key` in the repository secret
/// `MINISIGN_SECRET_KEY` (with its password in `MINISIGN_PASSWORD`).
const PUBLIC_KEY: &str = "";

/// The asset this build would install, named after the target it was compiled for.
///
/// A bare binary rather than an archive: extracting one would mean a zip and a tar
/// implementation for a file that is a single executable anyway.
pub fn asset_name() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "fastpick-x86_64-pc-windows-msvc.exe",
        ("linux", "x86_64") => "fastpick-x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "fastpick-aarch64-unknown-linux-gnu",
        ("macos", "x86_64") => "fastpick-x86_64-apple-darwin",
        ("macos", "aarch64") => "fastpick-aarch64-apple-darwin",
        _ => return None,
    })
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A version as three numbers, so `0.10.0` sorts after `0.9.0` where a string compare would
/// not. Anything after the patch number (`-rc1`, `+build`) is dropped: a prerelease is never
/// offered as an update.
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim_start_matches('v');
    if core.contains('-') || core.contains('+') {
        return None;
    }
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next().unwrap_or("0").parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn is_newer(candidate: &str, than: &str) -> bool {
    match (parse_version(candidate), parse_version(than)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// What the last check found, kept on disk so opening the menu does not mean a request.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Checked {
    #[serde(default)]
    checked_at: u64,
    #[serde(default)]
    latest: Option<String>,
}

fn state_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|d| d.join("update.json"))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_state() -> Checked {
    let Some(p) = state_path() else {
        return Checked::default();
    };
    std::fs::read_to_string(p)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_state(s: &Checked) {
    let Some(p) = state_path() else { return };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(raw) = serde_json::to_string(s) else {
        return;
    };
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, raw).is_ok() && std::fs::rename(&tmp, &p).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The newer version the last check found, if it is still newer than what is running.
///
/// Reads a file and nothing else, so the menu can call it while drawing.
pub fn pending() -> Option<String> {
    let s = read_state();
    let latest = s.latest?;
    is_newer(&latest, current_version()).then_some(latest)
}

/// Refreshes the cached answer in the background, at most once a day.
///
/// Every failure is silent. This is a nicety at the bottom of a menu, and a captive portal
/// or an offline laptop must not turn it into an error the user has to dismiss.
pub fn check_in_background() {
    let s = read_state();
    if now().saturating_sub(s.checked_at) < CHECK_EVERY_SECS {
        return;
    }
    // Recorded before the request, not after, so a provider that hangs cannot make every
    // single launch try again.
    write_state(&Checked {
        checked_at: now(),
        latest: s.latest.clone(),
    });

    std::thread::spawn(move || {
        if let Ok(v) = latest_version() {
            write_state(&Checked {
                checked_at: now(),
                latest: Some(v),
            });
        }
    });
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .user_agent(&format!("fastpick/{}", current_version()))
        .build()
}

/// A token for a repository that is not public. Read from the environment rather than
/// stored: fastpick has no business keeping a GitHub credential of its own.
fn gh_token() -> Option<String> {
    ["GH_TOKEN", "GITHUB_TOKEN"]
        .iter()
        .find_map(|k| std::env::var(k).ok())
        .filter(|t| !t.trim().is_empty())
}

fn with_auth(req: ureq::Request) -> ureq::Request {
    let req = req.set("X-GitHub-Api-Version", "2022-11-28");
    match gh_token() {
        Some(t) => req.set("Authorization", &format!("Bearer {t}")),
        None => req,
    }
}

fn latest_release() -> Result<serde_json::Value> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = with_auth(
        agent()
            .get(&url)
            .set("Accept", "application/vnd.github+json"),
    )
    .call()
    .map_err(describe)?;
    resp.into_json()
        .context("GitHub answered something that is not JSON")
}

fn latest_version() -> Result<String> {
    let release = latest_release()?;
    let tag = release
        .get("tag_name")
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow!("the latest release has no tag"))?;
    Ok(tag.trim_start_matches('v').to_string())
}

/// A one-line reason, with the private-repo case named rather than left as "HTTP 404".
fn describe(e: ureq::Error) -> anyhow::Error {
    match e {
        // The same 404 covers "no published release", "private repository" and "your token
        // cannot see it", and GitHub's own body says none of the three.
        ureq::Error::Status(404, _) if gh_token().is_none() => anyhow!(
            "no published release found for {REPO}. If the repository is private, set GH_TOKEN to a token that can read it."
        ),
        ureq::Error::Status(404, _) => anyhow!(
            "no published release found for {REPO}. A draft release does not count until someone presses publish, and GH_TOKEN has to be able to read the repository."
        ),
        ureq::Error::Status(403, r) | ureq::Error::Status(429, r)
            if r.header("x-ratelimit-remaining") == Some("0") =>
        {
            anyhow!("GitHub's rate limit is exhausted for this IP. Set GH_TOKEN to raise it.")
        }
        ureq::Error::Status(code, r) => match r.into_string() {
            Ok(body) if !body.trim().is_empty() => {
                let line = body.lines().next().unwrap_or_default();
                anyhow!("HTTP {code}: {}", &line[..line.len().min(200)])
            }
            _ => anyhow!("HTTP {code}"),
        },
        ureq::Error::Transport(t) => {
            let s = t.to_string();
            anyhow!("{}", s.lines().next().unwrap_or("transport error"))
        }
    }
}

/// The download url for one asset of a release.
///
/// The API url rather than `browser_download_url`: the browser one is unauthenticated and
/// answers 404 for a private repository even with a valid token on the request.
fn asset_url(release: &serde_json::Value, name: &str) -> Option<String> {
    release
        .get("assets")?
        .as_array()?
        .iter()
        .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(name))
        .and_then(|a| a.get("url"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
}

fn download(url: &str, limit: usize) -> Result<Vec<u8>> {
    let resp = with_auth(agent().get(url).set("Accept", "application/octet-stream"))
        .call()
        .map_err(describe)?;
    let mut buf = Vec::new();
    // Bounded: `into_reader` has no cap of its own, and a wrong url pointing at something
    // enormous would otherwise be read until the machine runs out of memory.
    std::io::Read::take(resp.into_reader(), limit as u64 + 1)
        .read_to_end(&mut buf)
        .context("reading the download")?;
    if buf.len() > limit {
        return Err(anyhow!(
            "the download is larger than {limit} bytes, refusing it"
        ));
    }
    Ok(buf)
}

/// Checks the release signature over the downloaded bytes.
///
/// Refuses rather than warns when no key is compiled in. An update path that installs
/// unverified bytes is worth less than no update path at all.
fn verify(bytes: &[u8], signature: &str) -> Result<()> {
    // Clippy is right that the test below is constant, and it is constant the wrong way
    // round today: the key is still empty. Filling `PUBLIC_KEY` in turns this into the
    // dead branch it is meant to be, and the allow can go with it.
    #[allow(clippy::const_is_empty)]
    if PUBLIC_KEY.is_empty() {
        return Err(anyhow!(
            "this build has no release signing key compiled in, so a downloaded binary cannot be verified. Install the update by hand from https://github.com/{REPO}/releases"
        ));
    }
    let key = minisign_verify::PublicKey::from_base64(PUBLIC_KEY)
        .map_err(|e| anyhow!("the compiled-in public key is not valid minisign: {e}"))?;
    let sig = minisign_verify::Signature::decode(signature)
        .map_err(|e| anyhow!("the release signature could not be read: {e}"))?;
    key.verify(bytes, &sig, false)
        .map_err(|_| anyhow!("the downloaded binary is not signed by the fastpick release key"))
}

/// Downloads the newest release and puts it where this binary is running from.
pub fn run() -> Result<i32> {
    let asset = asset_name().ok_or_else(|| {
        anyhow!(
            "no release is built for {}-{}. Build from source instead.",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;

    println!("Checking {REPO} ...");
    let release = latest_release()?;
    let tag = release
        .get("tag_name")
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow!("the latest release has no tag"))?;
    let latest = tag.trim_start_matches('v');

    // Recorded whatever happens next, so the menu stops offering a version the user has
    // just dealt with one way or the other.
    write_state(&Checked {
        checked_at: now(),
        latest: Some(latest.to_string()),
    });

    if !is_newer(latest, current_version()) {
        println!("{} is the newest release.", current_version());
        return Ok(0);
    }

    let bin_url = asset_url(&release, asset)
        .ok_or_else(|| anyhow!("release {tag} has no `{asset}`, so there is nothing to install"))?;
    let sig_url = asset_url(&release, &format!("{asset}.minisig"))
        .ok_or_else(|| anyhow!("release {tag} ships `{asset}` unsigned, refusing to install it"))?;

    println!("Downloading {latest} ...");
    let bytes = download(&bin_url, 64 * 1024 * 1024)?;
    let signature = String::from_utf8(download(&sig_url, 64 * 1024)?)
        .context("the signature file is not text")?;
    verify(&bytes, &signature)?;

    let target = std::env::current_exe().context("finding the running binary")?;
    replace_self(&target, &bytes)?;
    println!("fastpick is now {latest}.");
    Ok(0)
}

/// Puts `bytes` where the running binary is, without ever leaving that path missing.
///
/// The new file is written beside the target and renamed onto it, so a crash or a full disk
/// halfway through leaves the old binary in place rather than a truncated one.
fn replace_self(target: &Path, bytes: &[u8]) -> Result<()> {
    let dir = target
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", target.display()))?;
    let staged = dir.join("fastpick.new");
    std::fs::write(&staged, bytes).with_context(|| {
        format!(
            "writing {}. Is {} writable?",
            staged.display(),
            dir.display()
        )
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .context("making the new binary executable")?;
    }

    // Windows will not overwrite a running executable, but it will rename one. The old file
    // is moved aside so the rename below has a free path, and cleaned up on the next run
    // because it is still locked while this process lives.
    let retired = dir.join("fastpick.old");
    #[cfg(windows)]
    {
        let _ = std::fs::remove_file(&retired);
        std::fs::rename(target, &retired).with_context(|| {
            format!(
                "moving the running binary aside. Is {} writable?",
                dir.display()
            )
        })?;
    }

    if let Err(e) = std::fs::rename(&staged, target) {
        // Put back what was moved, so a failure here does not leave the user with no
        // fastpick at all.
        #[cfg(windows)]
        let _ = std::fs::rename(&retired, target);
        let _ = std::fs::remove_file(&staged);
        return Err(e).with_context(|| format!("installing over {}", target.display()));
    }
    let _ = std::fs::remove_file(&retired);
    Ok(())
}

/// Deletes what the previous `--update` had to leave behind. No-op everywhere else.
pub fn sweep_leftovers() {
    #[cfg(windows)]
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let _ = std::fs::remove_file(dir.join("fastpick.old"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_numbers_not_as_text() {
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("0.2.2", "0.2.1"));
        assert!(!is_newer("0.2.1", "0.2.1"));
        assert!(!is_newer("0.2.0", "0.2.1"));
    }

    #[test]
    fn a_leading_v_is_the_tag_not_the_version() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
    }

    #[test]
    fn a_prerelease_is_never_offered_as_an_update() {
        assert_eq!(parse_version("1.2.3-rc1"), None);
        assert_eq!(parse_version("1.2.3+build7"), None);
        assert!(!is_newer("1.0.0-rc1", "0.2.1"));
    }

    #[test]
    fn garbage_never_reads_as_newer() {
        for v in ["", "latest", "1.2.3.4", "x.y.z", "v"] {
            assert!(!is_newer(v, "0.2.1"), "{v:?} should not be an update");
            assert!(!is_newer("0.2.1", v), "{v:?} should not be a baseline");
        }
    }

    #[test]
    fn an_unsigned_build_refuses_to_install_rather_than_trusting_https() {
        // Guards the constant above: filling it in is a deliberate act, and until then the
        // failure has to be loud rather than a silent fallback.
        #[allow(clippy::const_is_empty)]
        if PUBLIC_KEY.is_empty() {
            let e = verify(b"whatever", "untrusted comment\nnot-a-signature\n").unwrap_err();
            assert!(e.to_string().contains("signing key"), "{e}");
        }
    }

    #[test]
    fn every_asset_the_workflow_builds_can_be_named_here() {
        // The release matrix and this table are the same list written twice, and a target
        // missing here tells its users to build from source while a binary sits in the
        // release. Read from the workflow so the two cannot drift apart quietly.
        let workflow = include_str!("../.github/workflows/release.yml");
        let built: Vec<&str> = workflow
            .lines()
            .filter_map(|l| l.trim().strip_prefix("asset: "))
            .collect();
        assert_eq!(
            built.len(),
            5,
            "the release matrix changed shape: {built:?}"
        );

        let named = [
            "fastpick-x86_64-pc-windows-msvc.exe",
            "fastpick-x86_64-unknown-linux-gnu",
            "fastpick-aarch64-unknown-linux-gnu",
            "fastpick-x86_64-apple-darwin",
            "fastpick-aarch64-apple-darwin",
        ];
        for asset in &built {
            assert!(named.contains(asset), "{asset} is built but never named");
        }

        // And this platform is one of them, so the test is not vacuous on any runner.
        let mine = asset_name().expect("no asset name for this platform");
        assert!(built.contains(&mine), "{mine} is named but never built");
    }
}
