//! The config file: harnesses, providers, how the two are wired together, and where the
//! system prompts live.
//!
//! The shape follows the order of the menu. A harness is a coding agent binary. A provider
//! is an endpoint. The two are not interchangeable: reaching one endpoint from two
//! different agents needs two different sets of settings, so a provider declares one
//! `[provider.harness.<id>]` binding per agent it can serve and appears in the menu only
//! for those.
//!
//! A provider may hold several credentials, one `[[provider.key]]` block each, because a
//! site can issue one key per upstream group and those groups differ in which models and
//! which API surfaces they allow. Everything that describes a route therefore lives on the
//! key rather than on the provider: its token file, its catalogue, its bindings, its models,
//! its proxy and its host check. The provider keeps what the site shares, which is its name,
//! its menu group and a base `env`.
//!
//! Declaring no key at all is the common case and stays written the short way, route fields
//! straight on the provider. `normalise` folds that into one synthetic key, so nothing
//! downstream of this module ever sees two shapes.

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

    /// The site's own variables, shared by every key. A key's own `env` is layered on top
    /// and wins, so this is for what the provider says about itself rather than about one
    /// subscription.
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// One credential, with the whole route it can reach. Filled by `normalise` even when
    /// the config declares none, so it is never empty by the time anything reads it.
    #[serde(default, rename = "key")]
    pub keys: Vec<ProviderKey>,

    // The short form: one unnamed key written straight on the provider. Private, and read
    // once by `normalise`, which moves it into `keys`. Nothing outside this module sees
    // these, so no caller can read a route field that a multi-key provider left empty.
    #[serde(default)]
    auth_token_file: Option<String>,
    #[serde(default)]
    small_fast_model: Option<String>,
    #[serde(default)]
    proxy: Option<Proxy>,
    #[serde(default)]
    host_check: Option<HostCheck>,
    #[serde(default)]
    catalog: Option<Catalog>,
    #[serde(default)]
    harness: BTreeMap<String, Binding>,
    #[serde(default, rename = "model")]
    models: Vec<Model>,
}

/// One credential and everything it can reach.
///
/// A provider that issues a single key never writes this block: `normalise` builds one from
/// the provider's own fields, gives it the provider's id and no label, and the rest of the
/// program stops caring which of the two shapes the file used.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderKey {
    /// Unique inside its provider and free of `.`, since `<provider>.<key>` is how a key is
    /// addressed on the command line.
    pub id: String,

    /// Written dim beside a model in the menu, to say which subscription serves it. Unset on
    /// a provider with one key, where there is nothing to tell apart.
    #[serde(default)]
    pub label: Option<String>,

    /// File holding the bearer token. A missing file is a hard error before launch, never
    /// a session that silently fails to authenticate. Unset means the harness uses
    /// whatever credentials it already holds.
    #[serde(default)]
    pub auth_token_file: Option<String>,

    /// Background model for agents that have one (Claude Code calls it small/fast). Per key
    /// because a key can only serve the models its own group holds.
    #[serde(default)]
    pub small_fast_model: Option<String>,

    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// A local proxy that has to be listening before launch. On the key, so it starts only
    /// when the model picked is one this key serves.
    #[serde(default)]
    pub proxy: Option<Proxy>,

    /// A machine that has to answer before launch.
    #[serde(default)]
    pub host_check: Option<HostCheck>,

    /// Where to ask this key what it serves. Two keys on one site still get one lookup each:
    /// the answer is what the token may use, not what the site sells.
    #[serde(default)]
    pub catalog: Option<Catalog>,

    /// One entry per harness this key can serve, keyed by harness id.
    #[serde(default)]
    pub harness: BTreeMap<String, Binding>,

    /// Layered on top of the fetched catalogue: labels, context windows and effort levels
    /// no API reports. Also the fallback list when there is no catalogue at all.
    #[serde(default, rename = "model")]
    pub models: Vec<Model>,
}

impl ProviderKey {
    pub fn binding(&self, harness_id: &str) -> Option<&Binding> {
        self.harness.get(harness_id)
    }
}

impl Provider {
    /// The keys that can serve a given harness, in config order.
    pub fn keys_for(&self, harness_id: &str) -> Vec<usize> {
        self.keys
            .iter()
            .enumerate()
            .filter(|(_, k)| k.harness.contains_key(harness_id))
            .map(|(i, _)| i)
            .collect()
    }

    /// Whether any key reaches this harness. One is enough for the provider to be offered,
    /// the model list then narrows to the keys that do.
    pub fn binds(&self, harness_id: &str) -> bool {
        self.keys.iter().any(|k| k.harness.contains_key(harness_id))
    }

    /// How a key is named on the command line. A provider holding one key answers to its own
    /// id, so the short form never has to be written as `x.x`.
    pub fn route_id(&self, key: usize) -> String {
        if self.keys.len() < 2 {
            return self.id.clone();
        }
        format!("{}.{}", self.id, self.keys[key].id)
    }

    /// The site's variables with the key's own layered over them, the key winning on a
    /// name both declare.
    pub fn env_for(&self, key: usize) -> BTreeMap<String, String> {
        let mut env = self.env.clone();
        env.extend(
            self.keys[key]
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        env
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
        Config::parse(&raw).with_context(|| format!("reading {}", path.display()))
    }

    /// Parse, fold the short form into keys, check. The three always run together: a `Config`
    /// that skipped `normalise` would have an empty `keys` on every provider, so this is the
    /// only way one is built, tests included.
    pub fn parse(raw: &str) -> Result<Config> {
        let cfg = Config::parse_unvalidated(raw)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Parsed and folded into keys, but not checked.
    ///
    /// Only for the tests that exercise a guard living further down, on a config `validate`
    /// would refuse outright. Every other caller goes through `parse`.
    pub fn parse_unvalidated(raw: &str) -> Result<Config> {
        // No context wrapped around the parse error: serde already names the field and the
        // line, and `load` adds the file path. A layer saying "parsing the config" would
        // only push that detail out of the first line people read.
        let mut cfg: Config = toml::from_str(raw)?;
        cfg.normalise()?;
        Ok(cfg)
    }

    /// Turns the short form into the general one, so `keys` is the only shape after load.
    ///
    /// Mixing the two is refused rather than merged. A provider that declares keys has said
    /// where its routes live, and a stray `auth_token_file` left on the provider would then
    /// be a credential belonging to none of them: silently ignoring it is how the wrong key
    /// reaches an endpoint, so the field is named and the load stops.
    fn normalise(&mut self) -> Result<()> {
        for p in &mut self.providers {
            let stray = [
                p.auth_token_file.is_some().then_some("auth_token_file"),
                p.small_fast_model.is_some().then_some("small_fast_model"),
                p.proxy.is_some().then_some("[provider.proxy]"),
                p.host_check.is_some().then_some("[provider.host_check]"),
                p.catalog.is_some().then_some("[provider.catalog]"),
                (!p.harness.is_empty()).then_some("[provider.harness.<id>]"),
                (!p.models.is_empty()).then_some("[[provider.model]]"),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();

            if p.keys.is_empty() {
                p.keys.push(ProviderKey {
                    id: p.id.clone(),
                    label: None,
                    auth_token_file: p.auth_token_file.take(),
                    small_fast_model: p.small_fast_model.take(),
                    env: BTreeMap::new(),
                    proxy: p.proxy.take(),
                    host_check: p.host_check.take(),
                    catalog: p.catalog.take(),
                    harness: std::mem::take(&mut p.harness),
                    models: std::mem::take(&mut p.models),
                });
                continue;
            }
            if let Some(field) = stray.first() {
                return Err(anyhow!(
                    "provider `{}` declares [[provider.key]] blocks, so `{field}` cannot stay on the provider: move it into the key it belongs to",
                    p.id
                ));
            }
        }
        Ok(())
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
            // A provider written the short way holds one key named after it, and naming it
            // in an error would only say the provider twice. Every message below is worded
            // for both shapes through this.
            let named = p.keys.len() > 1;
            for (i, k) in p.keys.iter().enumerate() {
                let whose = match named {
                    true => format!("provider `{}`, key `{}`", p.id, k.id),
                    false => format!("provider `{}`", p.id),
                };
                if named {
                    if k.id.is_empty() {
                        return Err(anyhow!("provider `{}`: a [[provider.key]] has no id", p.id));
                    }
                    // `<provider>.<key>` is the route id, so a dot in either half would make
                    // it impossible to split back.
                    if k.id.contains('.') {
                        return Err(anyhow!(
                            "provider `{}`: key id `{}` contains a `.`, which is the separator in `<provider>.<key>`",
                            p.id,
                            k.id
                        ));
                    }
                    if p.keys[..i].iter().any(|o| o.id == k.id) {
                        return Err(anyhow!(
                            "provider `{}` declares two keys with id `{}`",
                            p.id,
                            k.id
                        ));
                    }
                }

                for (harness, binding) in &k.harness {
                    let Some(h) = self.harnesses.iter().find(|h| &h.id == harness) else {
                        return Err(anyhow!(
                            "{whose} binds to harness `{harness}`, which is not declared"
                        ));
                    };
                    // Caught here rather than at launch: the menu would otherwise offer the
                    // pair and only refuse it once the terminal has been handed over.
                    if h.kind == HarnessKind::Opencode
                        && binding.base_url.is_some()
                        && binding.npm.is_none()
                    {
                        return Err(anyhow!(
                            "{whose} gives OpenCode a base_url but no `npm`, so OpenCode has no dialect to speak it with. Use `@ai-sdk/anthropic`, `@ai-sdk/openai-compatible` or `@ai-sdk/openai`."
                        ));
                    }
                }
                if k.catalog.is_none() && k.models.is_empty() {
                    return Err(anyhow!(
                        "{whose} has neither a [provider.catalog] to list models nor any [[provider.model]] to fall back on"
                    ));
                }
                if let Some(cat) = &k.catalog {
                    // A key on a cleartext connection is a key on the wire. Refused rather
                    // than warned about, because a warning would scroll past inside a menu.
                    // Loopback is the exception and not a loose one: the bytes never leave the
                    // machine, and a local translator or a llama.cpp server is http by nature.
                    if cat.auth != CatalogAuth::None
                        && cat.url.starts_with("http://")
                        && !is_loopback_url(&cat.url)
                    {
                        return Err(anyhow!(
                            "{whose}: the catalogue url is http://, so the key would travel in cleartext. Use https://, or `auth = \"none\"` if the endpoint needs no key."
                        ));
                    }
                }
                check_unique(
                    &format!("model of {whose}"),
                    k.models.iter().map(|m| m.id.as_str()),
                )?;
                for m in &k.models {
                    if !model_id_is_plain(&m.id) {
                        return Err(anyhow!(
                            "{whose}: model id `{}` is not a plain name. Letters, digits and . _ - : / @ + [ ] only, because this ends up on a command line.",
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
            if p.keys.iter().all(|k| k.harness.is_empty()) {
                return Err(anyhow!(
                    "provider `{}` declares no [provider.harness.<id>] binding, so no harness can reach it",
                    p.id
                ));
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

    /// The providers that can serve a given harness, in config order. One key binding it is
    /// enough: the model list is what narrows to the keys that do.
    pub fn providers_for(&self, harness_id: &str) -> Vec<usize> {
        self.providers
            .iter()
            .enumerate()
            .filter(|(_, p)| p.binds(harness_id))
            .map(|(i, _)| i)
            .collect()
    }

    /// Splits a route id into the provider and the key it addresses.
    ///
    /// `crof` names a provider holding one key. `codex-everywhere.openai` names one of
    /// several. A bare id on a provider holding several is deliberately not resolved to a
    /// default: the caller reports the ambiguity and lists what it could have meant.
    pub fn route(&self, id: &str) -> Option<(usize, usize)> {
        if let Some(pi) = self.providers.iter().position(|p| p.id == id) {
            return (self.providers[pi].keys.len() == 1).then_some((pi, 0));
        }
        let (pid, kid) = id.split_once('.')?;
        let pi = self.providers.iter().position(|p| p.id == pid)?;
        let ki = self.providers[pi].keys.iter().position(|k| k.id == kid)?;
        Some((pi, ki))
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
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))
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
        Config::parse(&toml)
    }

    fn err(extra: &str) -> String {
        parse(extra).unwrap_err().to_string()
    }

    #[test]
    fn the_shipped_starter_config_parses_and_validates() {
        // It is written to every new install by `ensure_config`, so a typo in it is a
        // broken first run for everyone.
        let mut cfg: Config =
            toml::from_str(DEFAULT_CONFIG).expect("the example config must parse");
        cfg.normalise()
            .expect("the example config must fold into keys");
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

/// Kept apart from `tests` above: these are all about the two config shapes and the fold
/// between them, and they need a fixture with two harnesses rather than that module's one.
#[cfg(test)]
mod key_tests {
    use super::*;

    const HARNESSES: &str = r#"
        [[harness]]
        id = "claude-code"
        name = "Claude Code"
        kind = "claude-code"
        bin = "claude"

        [[harness]]
        id = "codex"
        name = "Codex"
        kind = "codex"
        bin = "codex"
    "#;

    fn parse(providers: &str) -> Result<Config> {
        Config::parse(&format!("{HARNESSES}{providers}"))
    }

    #[test]
    fn the_short_form_becomes_one_key_named_after_its_provider() {
        let cfg = parse(
            r#"
            [[provider]]
            id = "acme"
            name = "Acme"
            auth_token_file = "~/.acme/client.key"
            small_fast_model = "acme-mini"

            [provider.harness.claude-code]
            base_url = "https://acme.invalid"

            [[provider.model]]
            id = "acme-mini"
            "#,
        )
        .unwrap();

        let p = &cfg.providers[0];
        assert_eq!(p.keys.len(), 1);
        assert_eq!(
            p.keys[0].id, "acme",
            "a lone key answers to the provider id"
        );
        assert_eq!(
            p.keys[0].label, None,
            "there is nothing to tell it apart from"
        );
        assert_eq!(
            p.keys[0].auth_token_file.as_deref(),
            Some("~/.acme/client.key")
        );
        assert_eq!(p.keys[0].small_fast_model.as_deref(), Some("acme-mini"));
        assert!(p.keys[0].binding("claude-code").is_some());
        assert_eq!(p.route_id(0), "acme", "no `acme.acme` in --set-key");
    }

    #[test]
    fn a_route_field_left_on_a_provider_that_declares_keys_is_named_and_refused() {
        let err = parse(
            r#"
            [[provider]]
            id = "acme"
            name = "Acme"
            auth_token_file = "~/.acme/stray.key"

            [[provider.key]]
            id = "first"

            [provider.key.harness.claude-code]

            [[provider.key.model]]
            id = "m"
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("auth_token_file"), "{err}");
    }

    #[test]
    fn two_keys_cannot_share_an_id_and_none_may_hold_a_dot() {
        let same = parse(
            r#"
            [[provider]]
            id = "acme"
            name = "Acme"

            [[provider.key]]
            id = "one"
            [provider.key.harness.claude-code]
            [[provider.key.model]]
            id = "m"

            [[provider.key]]
            id = "one"
            [provider.key.harness.claude-code]
            [[provider.key.model]]
            id = "m"
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(same.contains("two keys"), "{same}");

        let dotted = parse(
            r#"
            [[provider]]
            id = "acme"
            name = "Acme"

            [[provider.key]]
            id = "one.two"
            [provider.key.harness.claude-code]
            [[provider.key.model]]
            id = "m"

            [[provider.key]]
            id = "three"
            [provider.key.harness.claude-code]
            [[provider.key.model]]
            id = "m"
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(dotted.contains("separator"), "{dotted}");
    }

    #[test]
    fn a_provider_is_offered_as_soon_as_one_key_binds_the_harness() {
        let cfg = parse(
            r#"
            [[provider]]
            id = "acme"
            name = "Acme"

            [[provider.key]]
            id = "anthropic"
            label = "anthropic"
            [provider.key.harness.claude-code]
            base_url = "https://acme.invalid"
            [[provider.key.model]]
            id = "a"

            [[provider.key]]
            id = "openai"
            label = "openai"
            [provider.key.harness.codex]
            base_url = "https://acme.invalid/v1"
            [[provider.key.model]]
            id = "b"
            "#,
        )
        .unwrap();

        let p = &cfg.providers[0];
        assert_eq!(cfg.providers_for("claude-code"), vec![0]);
        assert_eq!(cfg.providers_for("codex"), vec![0]);
        // The provider shows up for both, and the model list is what narrows to one key.
        assert_eq!(p.keys_for("claude-code"), vec![0]);
        assert_eq!(p.keys_for("codex"), vec![1]);
        assert_eq!(p.route_id(1), "acme.openai");
        assert_eq!(cfg.route("acme.openai"), Some((0, 1)));
        assert_eq!(
            cfg.route("acme"),
            None,
            "a bare id on a provider holding several must not resolve to a default"
        );
    }

    #[test]
    fn a_key_env_wins_over_the_site_it_belongs_to() {
        let cfg = parse(
            r#"
            [[provider]]
            id = "acme"
            name = "Acme"

            [provider.env]
            SHARED = "site"
            BOTH = "site"

            [[provider.key]]
            id = "one"
            [provider.key.env]
            BOTH = "key"
            OWN = "key"
            [provider.key.harness.claude-code]
            [[provider.key.model]]
            id = "m"

            [[provider.key]]
            id = "two"
            [provider.key.harness.claude-code]
            [[provider.key.model]]
            id = "m"
            "#,
        )
        .unwrap();

        let env = cfg.providers[0].env_for(0);
        assert_eq!(env.get("SHARED").map(String::as_str), Some("site"));
        assert_eq!(env.get("BOTH").map(String::as_str), Some("key"));
        assert_eq!(env.get("OWN").map(String::as_str), Some("key"));
        // The other key never sees what the first one declared for itself.
        assert_eq!(cfg.providers[0].env_for(1).get("OWN"), None);
    }

    #[test]
    fn a_provider_no_key_of_which_binds_anything_is_refused() {
        let err = parse(
            r#"
            [[provider]]
            id = "acme"
            name = "Acme"

            [[provider.key]]
            id = "one"
            [[provider.key.model]]
            id = "m"
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no harness can reach it"), "{err}");
    }

    #[test]
    fn the_bundled_example_config_loads() {
        Config::parse(DEFAULT_CONFIG).unwrap();
    }
}
