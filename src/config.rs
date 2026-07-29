//! The config file: harnesses, providers, how the two are wired together, and where the
//! system prompts live.
//!
//! The shape follows the order of the menu. A harness is a coding agent binary. A provider
//! is an endpoint. The two are not interchangeable: reaching one endpoint from two
//! different agents needs two different sets of settings, so a provider declares one
//! `[provider.harness.<id>]` binding per agent it can serve and appears in the menu only
//! for those.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Written to the config directory on first run.
pub const DEFAULT_CONFIG: &str = include_str!("../config.example.toml");

/// Whether a model id is a plain identifier and nothing else.
///
/// Model ids end up on a command line, and on Windows an agent installed through npm is a
/// `.cmd` shim, which means `cmd.exe` parses that line before the agent does. A catalogue
/// is a remote answer, so an id is a string the *server* chooses: without this check a
/// hostile or compromised endpoint could put shell syntax in one and have it run. Real ids
/// are ASCII names, so refusing everything else costs nothing.
pub fn model_id_is_plain(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '.' | '_' | '-' | ':' | '/' | '@' | '+' | '[' | ']')
        })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Folder holding the `.md` files offered as system prompts.
    #[serde(default)]
    pub system_prompts_dir: Option<String>,

    /// How long a fetched model catalogue stays fresh, in seconds. Six hours by default.
    #[serde(default = "default_catalog_ttl")]
    pub catalog_ttl_secs: u64,

    #[serde(default, rename = "harness")]
    pub harnesses: Vec<Harness>,

    #[serde(default, rename = "provider")]
    pub providers: Vec<Provider>,
}

fn default_catalog_ttl() -> u64 {
    6 * 60 * 60
}

/// Which built-in adapter drives a harness. The launch differs enough between agents that
/// this is code rather than more config: they disagree on how a model is named, how a
/// custom endpoint is declared, and whether extra instructions can be appended at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessKind {
    ClaudeCode,
    Opencode,
    Codex,
}

impl HarnessKind {
    /// Whether the adapter can append instructions to the system prompt at launch.
    pub fn supports_system_prompts(self) -> bool {
        match self {
            HarnessKind::ClaudeCode => true,
            HarnessKind::Opencode => true,
            // Codex has no append-only surface: its instructions override replaces the
            // base prompt, tool rules included. Left unsupported rather than guessed at.
            HarnessKind::Codex => false,
        }
    }

    /// Whether the adapter passes an effort level through.
    pub fn supports_effort(self) -> bool {
        matches!(self, HarnessKind::ClaudeCode)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Harness {
    pub id: String,
    pub name: String,
    pub kind: HarnessKind,
    /// Executable. A bare name is resolved through PATH.
    pub bin: String,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub id: String,
    pub name: String,

    /// Free-form heading the menu puts above this provider, typically the site it belongs
    /// to. Providers sharing one are shown as one block, in config order: two entries that
    /// are the same endpoint with a different key stop looking like two unrelated choices.
    /// Unset means no heading.
    #[serde(default)]
    pub group: Option<String>,

    /// File holding the bearer token. A missing file is a hard error before launch, never
    /// a session that silently fails to authenticate. Unset means the harness uses
    /// whatever credentials it already holds.
    #[serde(default)]
    pub auth_token_file: Option<String>,

    /// Background model for agents that have one (Claude Code calls it small/fast).
    #[serde(default)]
    pub small_fast_model: Option<String>,

    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// A local proxy that has to be listening before launch.
    #[serde(default)]
    pub proxy: Option<Proxy>,

    /// A machine that has to answer before launch.
    #[serde(default)]
    pub host_check: Option<HostCheck>,

    /// Where to ask this provider what it serves.
    #[serde(default)]
    pub catalog: Option<Catalog>,

    /// One entry per harness this provider can serve, keyed by harness id.
    #[serde(default)]
    pub harness: BTreeMap<String, Binding>,

    /// Layered on top of the fetched catalogue: labels, context windows and effort levels
    /// no API reports. Also the fallback list when there is no catalogue at all.
    #[serde(default, rename = "model")]
    pub models: Vec<Model>,
}

impl Provider {
    pub fn binding(&self, harness_id: &str) -> Option<&Binding> {
        self.harness.get(harness_id)
    }
}

/// What one harness needs to reach this provider.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    /// Unset means the harness keeps its own default endpoint and credentials. That is how
    /// a native provider is declared: an empty binding, not a missing one.
    #[serde(default)]
    pub base_url: Option<String>,

    /// OpenCode only: the AI SDK package that speaks this endpoint's dialect, for example
    /// `@ai-sdk/anthropic` or `@ai-sdk/openai-compatible`.
    #[serde(default)]
    pub npm: Option<String>,

    /// Codex only: `chat` for `/v1/chat/completions`, `responses` for `/v1/responses`.
    #[serde(default)]
    pub wire_api: Option<WireApi>,

    #[serde(default)]
    pub env: BTreeMap<String, String>,

    #[serde(default)]
    pub extra_args: Vec<String>,
}

/// The wire format Codex speaks to a provider.
///
/// `chat` is accepted here and refused by Codex itself with "`wire_api = \"chat\"` is no
/// longer supported", so it is kept as a value the config can name and the launch can
/// explain, rather than silently rewritten to `responses` behind the user's back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WireApi {
    Chat,
    Responses,
}

impl WireApi {
    pub fn as_str(self) -> &'static str {
        match self {
            WireApi::Chat => "chat",
            WireApi::Responses => "responses",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    /// An OpenAI-style `/v1/models` endpoint: `{"data":[{"id": ...}]}`. Anthropic's own
    /// shape parses identically, it just adds a `display_name`.
    pub url: String,

    /// How the request authenticates: `bearer`, `x-api-key` or `none`.
    #[serde(default)]
    pub auth: CatalogAuth,

    /// Keep only ids starting with one of these. Empty keeps everything.
    #[serde(default)]
    pub only_prefixes: Vec<String>,

    /// Drop ids containing any of these. Useful for the image and embedding models an
    /// agent cannot drive.
    #[serde(default)]
    pub exclude_contains: Vec<String>,
}

/// How a catalogue request carries the key.
///
/// An enum rather than a free string because the fallback used to be "anything unknown
/// means bearer": a typo, or `auth = "None"` with a capital, then sent `Authorization:
/// Bearer <key>` to an endpoint the user had explicitly told not to receive it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CatalogAuth {
    #[default]
    Bearer,
    XApiKey,
    None,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub id: String,

    #[serde(default)]
    pub label: Option<String>,

    /// Sets the harness's context-window variable. Leave unset for `claude-*` models under
    /// Claude Code: the resolver ignores the variable for those names and reads its own
    /// table instead, so declaring it there is noise.
    #[serde(default)]
    pub context_window: Option<u64>,

    #[serde(default)]
    pub compact_ratio: Option<f64>,

    #[serde(default)]
    pub effort: Vec<String>,

    #[serde(default)]
    pub effort_default: Option<String>,

    #[serde(default)]
    pub small_fast_model: Option<String>,
}

impl Model {
    pub fn new(id: String) -> Model {
        Model {
            id,
            label: None,
            context_window: None,
            compact_ratio: None,
            effort: Vec::new(),
            effort_default: None,
            small_fast_model: None,
        }
    }

    pub fn display(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.id)
    }

    /// The model name with any window suffix removed, e.g. `claude-opus-5[1m]` becomes
    /// `claude-opus-5`. Used to match system prompt files.
    pub fn base_name(&self) -> &str {
        match self.id.find('[') {
            Some(i) => &self.id[..i],
            None => &self.id,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proxy {
    pub port: u16,
    pub exe: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(default = "default_proxy_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub logs_hint: Option<String>,
}

fn default_proxy_timeout() -> u64 {
    15
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCheck {
    pub host: String,
    /// `warn` prints the message and launches anyway, `abort` refuses.
    #[serde(default)]
    pub on_down: OnDown,
    #[serde(default)]
    pub message: Option<String>,
}

/// What a failed host check does. Compared against the string `"abort"` before, so
/// `on_down = "Abort"` silently downgraded a refusal to a warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OnDown {
    #[default]
    Warn,
    Abort,
}

impl std::fmt::Display for OnDown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            OnDown::Warn => "warn",
            OnDown::Abort => "abort",
        })
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.harnesses.is_empty() {
            return Err(anyhow!("no [[harness]] declared"));
        }
        if self.providers.is_empty() {
            return Err(anyhow!("no [[provider]] declared"));
        }

        // Everything downstream resolves an id with `position()`, so a duplicate makes the
        // second entry permanently unreachable and sends the saved cursor to the wrong row.
        // Silent, and the file is written to be copy-pasted from block to block.
        check_unique("harness", self.harnesses.iter().map(|h| h.id.as_str()))?;
        check_unique("provider", self.providers.iter().map(|p| p.id.as_str()))?;

        for h in &self.harnesses {
            if h.bin.trim().is_empty() {
                return Err(anyhow!("harness `{}` declares an empty `bin`", h.id));
            }
        }

        for p in &self.providers {
            if p.harness.is_empty() {
                return Err(anyhow!(
                    "provider `{}` declares no [provider.harness.<id>] binding, so no harness can reach it",
                    p.id
                ));
            }
            for (key, binding) in &p.harness {
                let Some(h) = self.harnesses.iter().find(|h| &h.id == key) else {
                    return Err(anyhow!(
                        "provider `{}` binds to harness `{key}`, which is not declared",
                        p.id
                    ));
                };
                // Caught here rather than at launch: the menu would otherwise offer the
                // pair and only refuse it once the terminal has been handed over.
                if h.kind == HarnessKind::Opencode
                    && binding.base_url.is_some()
                    && binding.npm.is_none()
                {
                    return Err(anyhow!(
                        "provider `{}` gives OpenCode a base_url but no `npm`, so OpenCode has no dialect to speak it with. Use `@ai-sdk/anthropic`, `@ai-sdk/openai-compatible` or `@ai-sdk/openai`.",
                        p.id
                    ));
                }
            }
            if p.catalog.is_none() && p.models.is_empty() {
                return Err(anyhow!(
                    "provider `{}` has neither a [provider.catalog] to list models nor any [[provider.model]] to fall back on",
                    p.id
                ));
            }
            if let Some(cat) = &p.catalog {
                // A key on a cleartext connection is a key on the wire. Refused rather
                // than warned about, because a warning would scroll past inside a menu.
                // Loopback is the exception and not a loose one: the bytes never leave the
                // machine, and a local translator or a llama.cpp server is http by nature.
                if cat.auth != CatalogAuth::None
                    && cat.url.starts_with("http://")
                    && !is_loopback_url(&cat.url)
                {
                    return Err(anyhow!(
                        "provider `{}`: the catalogue url is http://, so the key would travel in cleartext. Use https://, or `auth = \"none\"` if the endpoint needs no key.",
                        p.id
                    ));
                }
            }
            check_unique(
                &format!("model of provider `{}`", p.id),
                p.models.iter().map(|m| m.id.as_str()),
            )?;
            for m in &p.models {
                if !model_id_is_plain(&m.id) {
                    return Err(anyhow!(
                        "provider `{}`: model id `{}` is not a plain name. Letters, digits and . _ - : / @ + [ ] only, because this ends up on a command line.",
                        p.id,
                        m.id
                    ));
                }
                if let Some(r) = m.compact_ratio {
                    // TOML accepts `nan` and `inf`, and a NaN survives `clamp` unchanged,
                    // then turns into a compact window of 0 once cast.
                    if !r.is_finite() || !(0.1..=1.0).contains(&r) {
                        return Err(anyhow!(
                            "model `{}`: compact_ratio must be a number between 0.1 and 1.0, got {r}",
                            m.id
                        ));
                    }
                }
                if let Some(d) = &m.effort_default {
                    if !m.effort.iter().any(|e| e == d) {
                        return Err(anyhow!(
                            "model `{}`: effort_default `{}` is not in its effort list",
                            m.id,
                            d
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// The harnesses whose binary is on this machine, in config order.
    ///
    /// The config is a description of what could be launched anywhere, and the same file is
    /// meant to follow its owner from one machine to the next. Which agents are actually
    /// installed is a property of the machine, so it is answered by looking rather than by
    /// asking the user to keep a list in sync.
    pub fn harnesses_installed(&self) -> Vec<usize> {
        self.harnesses
            .iter()
            .enumerate()
            .filter(|(_, h)| crate::paths::locate(&h.bin).is_some())
            .map(|(i, _)| i)
            .collect()
    }

    /// The providers that can serve a given harness, in config order.
    pub fn providers_for(&self, harness_id: &str) -> Vec<usize> {
        self.providers
            .iter()
            .enumerate()
            .filter(|(_, p)| p.harness.contains_key(harness_id))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn prompts_dir(&self) -> Option<PathBuf> {
        self.system_prompts_dir
            .as_deref()
            .map(crate::paths::expand)
            .or_else(|| config_dir().map(|d| d.join("system-prompts")))
    }
}

/// `%APPDATA%\fastpick` on Windows, `$XDG_CONFIG_HOME/fastpick` or `~/.config/fastpick` on
/// macOS and Linux alike.
///
/// Deliberately not `dirs::config_dir()` on macOS: that answers `~/Library/Application
/// Support`, which is right for an app with a window and wrong for a command-line tool
/// whose config is meant to be opened in an editor and kept in a dotfiles repo next to the
/// agents it launches.
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return dirs::config_dir().map(|d| d.join("fastpick"));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("fastpick"));
    }
    dirs::home_dir().map(|h| h.join(".config").join("fastpick"))
}

pub fn config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("config.toml"))
}

/// Whether a url points at this machine, so cleartext on it never reaches a network.
fn is_loopback_url(url: &str) -> bool {
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest,
        None => url,
    };
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(rest)
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or_else(|| rest.split(['/', '?', '#']).next().unwrap_or(rest));
    let host = match host.strip_prefix('[') {
        // An IPv6 literal, `[::1]:8080`.
        Some(v6) => v6.split(']').next().unwrap_or(v6),
        None => host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host),
    };
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Rejects a duplicate or empty id, naming the one that repeats.
fn check_unique<'a>(what: &str, ids: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for id in ids {
        if id.trim().is_empty() {
            return Err(anyhow!("a {what} has an empty id"));
        }
        if !seen.insert(id) {
            return Err(anyhow!(
                "two {what} entries share the id `{id}`, so only the first can ever be reached"
            ));
        }
    }
    Ok(())
}

/// Creates a directory only this user can enter.
///
/// The config directory holds the paths of the key files and, whatever the comment at the
/// top of the file says, the place a user will eventually inline a key.
fn create_dir_private(dir: &Path) -> Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        return std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()));
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))
}

/// Writes the bundled default config if none exists yet, and reports whether it did.
pub fn ensure_config(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        create_dir_private(parent)?;
        // Also tightens a directory that already existed, which the create above returns
        // early on: the config folder is where key files end up by default.
        crate::secrets::harden_dir(parent);
        create_dir_private(&parent.join("system-prompts"))?;
    }

    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    // `create_new` rather than a plain write after the `exists` above: two runs starting at
    // the same moment would both find nothing and the second would overwrite the first.
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(path) {
        Ok(mut f) => {
            f.write_all(DEFAULT_CONFIG.as_bytes())
                .with_context(|| format!("writing {}", path.display()))?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e).with_context(|| format!("writing {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config with one harness and one provider, so a test only has to write the part
    /// it is about.
    fn parse(extra_provider: &str) -> Result<Config> {
        let toml = format!(
            r#"
            [[harness]]
            id = "claude-code"
            name = "Claude Code"
            kind = "claude-code"
            bin = "claude"
            {extra_provider}
            "#
        );
        let cfg: Config = toml::from_str(&toml)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn err(extra: &str) -> String {
        parse(extra).unwrap_err().to_string()
    }

    #[test]
    fn the_shipped_starter_config_parses_and_validates() {
        // It is written to every new install by `ensure_config`, so a typo in it is a
        // broken first run for everyone.
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).expect("the example config must parse");
        cfg.validate().expect("the example config must validate");
    }

    #[test]
    fn the_starter_config_leaves_the_prompts_folder_to_the_platform() {
        // Written out, the path would have to pick a shape, and `~/.config/...` is wrong
        // on Windows where the config lives under %APPDATA%.
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert!(
            cfg.system_prompts_dir.is_none(),
            "the example must not pin system_prompts_dir"
        );
    }

    #[test]
    fn a_misspelt_field_is_refused_instead_of_dropped() {
        let e = err(r#"
            [[provider]]
            id = "p"
            name = "P"
            auth_token_files = "~/k"
            [provider.harness.claude-code]
            [[provider.model]]
            id = "m"
            "#);
        assert!(e.contains("auth_token_files"), "{e}");
    }

    #[test]
    fn two_providers_cannot_share_an_id() {
        let e = err(r#"
            [[provider]]
            id = "twin"
            name = "First"
            [provider.harness.claude-code]
            [[provider.model]]
            id = "m"

            [[provider]]
            id = "twin"
            name = "Second"
            [provider.harness.claude-code]
            [[provider.model]]
            id = "m"
            "#);
        assert!(e.contains("twin"), "{e}");
    }

    #[test]
    fn a_model_id_that_is_not_a_plain_name_is_refused() {
        // It reaches a command line, and on Windows `cmd.exe` parses that line first.
        let e = err(r#"
            [[provider]]
            id = "p"
            name = "P"
            [provider.harness.claude-code]
            [[provider.model]]
            id = "m1 & calc.exe"
            "#);
        assert!(e.contains("plain name"), "{e}");
    }

    #[test]
    fn real_model_ids_are_accepted() {
        for id in [
            "claude-opus-5[1m]",
            "gpt-5.2",
            "meta/llama-4-70b",
            "qwen3:32b",
            "model@2026-01-01",
        ] {
            assert!(model_id_is_plain(id), "{id} should be accepted");
        }
        for id in ["", "a b", "a&b", "a|b", "a\"b", "a%PATH%b", "a\nb"] {
            assert!(!model_id_is_plain(id), "{id:?} should be refused");
        }
    }

    #[test]
    fn a_key_is_never_sent_over_cleartext_to_a_remote_host() {
        let e = err(r#"
            [[provider]]
            id = "p"
            name = "P"
            [provider.catalog]
            url = "http://models.invalid/v1/models"
            [provider.harness.claude-code]
            [[provider.model]]
            id = "m"
            "#);
        assert!(e.contains("cleartext"), "{e}");
    }

    #[test]
    fn loopback_over_http_is_fine_because_it_never_reaches_a_network() {
        for url in [
            "http://127.0.0.1:4000/v1/models",
            "http://localhost:8080/v1/models",
            "http://[::1]:8080/v1/models",
        ] {
            let cfg = parse(&format!(
                r#"
                [[provider]]
                id = "p"
                name = "P"
                [provider.catalog]
                url = "{url}"
                [provider.harness.claude-code]
                [[provider.model]]
                id = "m"
                "#
            ));
            assert!(cfg.is_ok(), "{url}: {:?}", cfg.err());
        }
    }

    #[test]
    fn an_unknown_auth_value_is_refused_rather_than_read_as_bearer() {
        // It used to fall through to bearer, so `auth = "None"` sent the key to an endpoint
        // the user had told it not to.
        let e = err(r#"
            [[provider]]
            id = "p"
            name = "P"
            [provider.catalog]
            url = "https://models.invalid/v1/models"
            auth = "None"
            [provider.harness.claude-code]
            [[provider.model]]
            id = "m"
            "#);
        assert!(e.contains("auth"), "{e}");
    }

    #[test]
    fn a_non_finite_compact_ratio_is_refused() {
        // TOML accepts `nan`, and NaN survives `clamp`, then casts to a window of 0.
        let e = err(r#"
            [[provider]]
            id = "p"
            name = "P"
            [provider.harness.claude-code]
            [[provider.model]]
            id = "m"
            context_window = 200000
            compact_ratio = nan
            "#);
        assert!(e.contains("compact_ratio"), "{e}");
    }

    #[test]
    fn loopback_detection_does_not_take_a_lookalike_host() {
        assert!(is_loopback_url("http://127.0.0.1:1/x"));
        assert!(is_loopback_url("http://127.5.5.5/x"));
        assert!(is_loopback_url("http://localhost/x"));
        assert!(is_loopback_url("http://[::1]:80/x"));
        assert!(!is_loopback_url("http://127.0.0.1.evil.invalid/x"));
        assert!(!is_loopback_url("http://localhost.evil.invalid/x"));
        assert!(!is_loopback_url("http://10.0.0.1/x"));
    }
}
