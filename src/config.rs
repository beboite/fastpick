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

#[derive(Debug, Deserialize)]
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
pub struct Harness {
    pub id: String,
    pub name: String,
    pub kind: HarnessKind,
    /// Executable. A bare name is resolved through PATH.
    pub bin: String,
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize)]
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

    #[serde(default)]
    pub note: Option<String>,

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
    pub wire_api: Option<String>,

    #[serde(default)]
    pub env: BTreeMap<String, String>,

    #[serde(default)]
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Catalog {
    /// An OpenAI-style `/v1/models` endpoint: `{"data":[{"id": ...}]}`. Anthropic's own
    /// shape parses identically, it just adds a `display_name`.
    pub url: String,

    /// How the request authenticates: `bearer`, `x-api-key` or `none`.
    #[serde(default = "default_catalog_auth")]
    pub auth: String,

    /// Keep only ids starting with one of these. Empty keeps everything.
    #[serde(default)]
    pub only_prefixes: Vec<String>,

    /// Drop ids containing any of these. Useful for the image and embedding models an
    /// agent cannot drive.
    #[serde(default)]
    pub exclude_contains: Vec<String>,
}

fn default_catalog_auth() -> String {
    "bearer".to_string()
}

#[derive(Debug, Deserialize, Clone)]
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

    #[serde(default)]
    pub note: Option<String>,
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
            note: None,
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
pub struct HostCheck {
    pub host: String,
    /// `warn` prints the message and launches anyway, `abort` refuses.
    #[serde(default = "default_on_down")]
    pub on_down: String,
    #[serde(default)]
    pub message: Option<String>,
}

fn default_on_down() -> String {
    "warn".to_string()
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
        for p in &self.providers {
            if p.harness.is_empty() {
                return Err(anyhow!(
                    "provider `{}` declares no [provider.harness.<id>] binding, so no harness can reach it",
                    p.id
                ));
            }
            for key in p.harness.keys() {
                if !self.harnesses.iter().any(|h| &h.id == key) {
                    return Err(anyhow!(
                        "provider `{}` binds to harness `{key}`, which is not declared",
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
            for m in &p.models {
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

/// `%APPDATA%\fastpick` on Windows, `~/.config/fastpick` elsewhere.
pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("fastpick"))
}

pub fn config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("config.toml"))
}

/// Writes the bundled default config if none exists yet, and reports whether it did.
pub fn ensure_config(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        let prompts = parent.join("system-prompts");
        std::fs::create_dir_all(&prompts)
            .with_context(|| format!("creating {}", prompts.display()))?;
    }
    std::fs::write(path, DEFAULT_CONFIG).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}
