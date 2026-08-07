//! The machine-readable side of `--list` and `--dry-run`, for callers that drive fastpick
//! instead of reading it.
//!
//! It has its own view structs rather than deriving `Serialize` on the config, so what a
//! caller may see is a deliberate list. A field added to the schema later cannot start
//! appearing here on its own, which matters most for the key files: a provider reports
//! whether its key is there, never where it is and never what is in it.
//!
//! The contract callers rely on is that exit code 0 means stdout holds one JSON document
//! and nothing else. Every notice and every error goes to stderr.

use anyhow::{anyhow, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use crate::catalog::{self, Source};
use crate::config::{Config, Provider, ProviderKey};
use crate::launch::{self, Selection};
use crate::paths::expand;
use crate::prompts;

/// Bumped only when a consumer would have to change. A new field is not a bump.
/// 2: `note` dropped from harnesses, providers and models. It never carried anything a
/// caller could act on, only prose about one machine's setup.
/// 3: a provider reports its keys. `needsKey`, `keyPresent`, `harnesses`, `proxyPort` and
/// `hostCheck` moved off the provider and onto each key, because a provider holding two
/// subscriptions answers those questions differently for each of them.
pub const SCHEMA: u32 = 3;

/// Environment variables whose value is a credential. Matched by exact name, never by
/// substring: `CLAUDE_CODE_MAX_CONTEXT_TOKENS` also contains `TOKEN`, and hiding a context
/// window would gut the output a caller asked for.
fn is_secret(key: &str) -> bool {
    key == "ANTHROPIC_AUTH_TOKEN" || key == "ANTHROPIC_API_KEY" || key == launch::KEY_ENV
}

pub fn print<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

// ---------------------------------------------------------------------------------------
// --list --json
// ---------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Listing {
    fastpick: &'static str,
    schema: u32,
    config: String,
    system_prompts_dir: Option<String>,
    harnesses: Vec<HarnessView>,
    providers: Vec<ProviderView>,
    prompts: Vec<PromptView>,
    /// Only when a provider was named: listing every provider's models would mean one HTTP
    /// call per provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    models: Option<ModelsView>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HarnessView {
    id: String,
    name: String,
    kind: crate::config::HarnessKind,
    bin: String,
    supports_system_prompts: bool,
    supports_effort: bool,
    /// Whether `bin` was found on this machine. A caller offering its own menu should hide
    /// what is not installed, the way the picker does.
    installed: bool,
    /// Ids of the providers wired to this harness, in config order. A pair absent here
    /// cannot be launched.
    providers: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderView {
    id: String,
    name: String,
    group: Option<String>,
    /// Never empty: a provider written the short way still reports one key, whose id is the
    /// provider's own.
    keys: Vec<KeyView>,
}

/// One credential and the route it reaches. Everything here is per-credential because two
/// keys on the same site can bind different harnesses and sit behind different proxies.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct KeyView {
    /// What `--provider` takes after the dot, and the whole id on a provider holding one
    /// key, where it equals the provider's.
    id: String,
    /// Shown beside a model to say which subscription serves it. Null when there is only
    /// one key and so nothing to tell apart.
    label: Option<String>,
    /// Whether this key authenticates with a key file at all. `false` means the harness
    /// keeps its own login.
    needs_key: bool,
    /// Whether that file is there right now. The path is deliberately not reported: a
    /// caller decides whether to offer the row, it never reads the file itself.
    key_present: bool,
    harnesses: BTreeMap<String, BindingView>,
    /// Local port a proxy has to be listening on before a launch. fastpick starts it.
    proxy_port: Option<u16>,
    host_check: Option<HostCheckView>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BindingView {
    /// Null means the harness keeps its own endpoint, which is how a native provider is
    /// declared. That is not the same as having no binding at all.
    base_url: Option<String>,
    npm: Option<String>,
    wire_api: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HostCheckView {
    host: String,
    /// `warn` launches anyway, `abort` refuses.
    on_down: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PromptView {
    /// The file name on disk.
    name: String,
    /// What `--md` takes.
    stem: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelsView {
    provider: String,
    source: SourceView,
    items: Vec<ModelView>,
}

/// Where the list came from, so a caller can say so rather than passing a stale list off as
/// a live one.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceView {
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    age_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// One `<key>: <reason>` per key whose catalogue could not be reached, when several were
    /// asked. Omitted when they all answered, so its presence alone means the list is short.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    failed: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelView {
    id: String,
    /// Id of the key that serves this model. A launch has to use that one: another key on
    /// the same provider may not list this model at all.
    key: String,
    label: Option<String>,
    context_window: Option<u64>,
    effort: Vec<String>,
    effort_default: Option<String>,
    /// System prompt files matching this model, most specific first, as `--md` names.
    prompts: Vec<String>,
}

impl From<&Source> for SourceView {
    fn from(s: &Source) -> SourceView {
        let (kind, count, age_secs, error, failed) = match s {
            Source::Live(n) => ("live", Some(*n), None, None, Vec::new()),
            Source::Cache(n, age) => ("cache", Some(*n), Some(*age), None, Vec::new()),
            // Kept apart from `cache`: a consumer has to be able to tell "still fresh" from
            // "served because the fetch failed", which is what the error field says here.
            Source::Stale(n, age, e) => {
                ("stale", Some(*n), Some(*age), Some(e.clone()), Vec::new())
            }
            Source::Config(n) => ("config", Some(*n), None, None, Vec::new()),
            Source::Failed(e) => ("failed", None, None, Some(e.clone()), Vec::new()),
            // One entry per key that could not be reached, since the merged list is still
            // usable and naming the whole thing failed would be wrong.
            Source::Several { count, .. } => {
                ("several", Some(*count), None, None, s.failures().to_vec())
            }
        };
        SourceView {
            kind,
            count,
            age_secs,
            error,
            failed,
        }
    }
}

fn key_view(k: &ProviderKey) -> KeyView {
    KeyView {
        id: k.id.clone(),
        label: k.label.clone(),
        needs_key: k.auth_token_file.is_some(),
        key_present: k
            .auth_token_file
            .as_deref()
            .is_some_and(|f| expand(f).is_file()),
        harnesses: k
            .harness
            .iter()
            .map(|(id, b)| {
                (
                    id.clone(),
                    BindingView {
                        base_url: b.base_url.clone(),
                        npm: b.npm.clone(),
                        wire_api: b.wire_api.map(|w| w.as_str().to_string()),
                    },
                )
            })
            .collect(),
        proxy_port: k.proxy.as_ref().map(|x| x.port),
        host_check: k.host_check.as_ref().map(|c| HostCheckView {
            host: c.host.clone(),
            on_down: c.on_down.to_string(),
        }),
    }
}

fn provider_view(p: &Provider) -> ProviderView {
    ProviderView {
        id: p.id.clone(),
        name: p.name.clone(),
        group: p.group.clone(),
        keys: p.keys.iter().map(key_view).collect(),
    }
}

/// Everything the config declares. With `provider` named, that provider's models too.
///
/// An unknown provider id is an error rather than an empty model list: a caller that
/// mistyped one would otherwise read the silence as "this provider serves nothing".
pub fn listing(
    cfg: &Config,
    cfg_path: &Path,
    provider: Option<&str>,
    refresh: bool,
) -> Result<Listing> {
    let dir = cfg.prompts_dir();

    let models = match provider {
        None => None,
        Some(id) => {
            let p = cfg
                .providers
                .iter()
                .find(|p| p.id == id)
                .ok_or_else(|| anyhow!("no provider with id `{id}`, see --list"))?;
            // No harness to narrow by here, so every key is asked: a caller listing a
            // provider wants the whole of what it serves, not one agent's slice of it.
            let (rows, source) = catalog::run_all(catalog::requests_for(cfg, p, None, refresh));
            Some(ModelsView {
                provider: p.id.clone(),
                source: SourceView::from(&source),
                items: rows
                    .iter()
                    .map(|row| {
                        let m = &row.model;
                        ModelView {
                            id: m.id.clone(),
                            key: p.keys[row.key].id.clone(),
                            label: m.label.clone(),
                            context_window: m.context_window,
                            effort: m.effort.clone(),
                            effort_default: m.effort_default.clone(),
                            prompts: dir
                                .as_ref()
                                .map(|d| {
                                    prompts::matches_for(d, m.prompt_name())
                                        .into_iter()
                                        .map(|f| f.stem)
                                        .collect()
                                })
                                .unwrap_or_default(),
                        }
                    })
                    .collect(),
            })
        }
    };

    Ok(Listing {
        fastpick: env!("CARGO_PKG_VERSION"),
        schema: SCHEMA,
        config: cfg_path.display().to_string(),
        system_prompts_dir: dir.as_ref().map(|d| d.display().to_string()),
        harnesses: cfg
            .harnesses
            .iter()
            .map(|h| HarnessView {
                id: h.id.clone(),
                name: h.name.clone(),
                kind: h.kind,
                bin: h.bin.clone(),
                supports_system_prompts: h.kind.supports_system_prompts(),
                supports_effort: h.kind.supports_effort(),
                installed: crate::paths::locate(&h.bin).is_some(),
                providers: cfg
                    .providers_for(&h.id)
                    .into_iter()
                    .map(|i| cfg.providers[i].id.clone())
                    .collect(),
            })
            .collect(),
        providers: cfg.providers.iter().map(provider_view).collect(),
        prompts: dir
            .as_deref()
            .map(|d| {
                prompts::all_in(d)
                    .into_iter()
                    .map(|f| PromptView {
                        name: f
                            .path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| f.stem.clone()),
                        stem: f.stem,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        models,
    })
}

// ---------------------------------------------------------------------------------------
// --dry-run --json
// ---------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DryRun {
    fastpick: &'static str,
    schema: u32,
    harness: String,
    provider: String,
    /// The route id the launch resolved to, `crof` or `codex-everywhere.openai`. It says
    /// which credential was picked, which a provider id alone cannot.
    key: String,
    model: String,
    effort: Option<String>,
    prompts: Vec<String>,
    cmd: String,
    args: Vec<String>,
    /// Everything the launch adds to the inherited environment, credentials excluded.
    env: BTreeMap<String, String>,
    /// The credential variables, by name and length only. A length of 0 means the launch
    /// clears an inherited value rather than setting one.
    secret_env: BTreeMap<String, Secret>,
    prechecks: Prechecks,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Secret {
    chars: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Prechecks {
    /// A local proxy fastpick starts and waits for, or null.
    proxy_port: Option<u16>,
    host_check: Option<HostCheckView>,
}

pub fn dry_run(cfg: &Config, sel: &Selection) -> Result<DryRun> {
    let cmd: Command = launch::build(cfg, sel)?;

    let mut env = BTreeMap::new();
    let mut secret_env = BTreeMap::new();
    for (k, v) in cmd.get_envs() {
        let key = k.to_string_lossy().to_string();
        let val = v
            .map(|v| v.to_string_lossy().to_string())
            .unwrap_or_default();
        if is_secret(&key) {
            secret_env.insert(key, Secret { chars: val.len() });
        } else {
            env.insert(key, val);
        }
    }

    Ok(DryRun {
        fastpick: env!("CARGO_PKG_VERSION"),
        schema: SCHEMA,
        harness: sel.harness.id.clone(),
        provider: sel.provider.id.clone(),
        key: sel.provider.route_id(sel.key),
        model: sel.model.id.clone(),
        effort: sel.effort.clone(),
        prompts: sel
            .prompts
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        cmd: cmd.get_program().to_string_lossy().to_string(),
        args: cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect(),
        env,
        secret_env,
        prechecks: Prechecks {
            proxy_port: sel.provider_key().proxy.as_ref().map(|x| x.port),
            host_check: sel
                .provider_key()
                .host_check
                .as_ref()
                .map(|c| HostCheckView {
                    host: c.host.clone(),
                    on_down: c.on_down.to_string(),
                }),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Model;
    use crate::prompts::tempdir::TempDir;
    use serde_json::Value;

    /// One provider on a key file that exists, one native provider on none, and a prompts
    /// folder. No `[provider.catalog]` anywhere, so nothing here touches the network.
    fn fixture() -> (TempDir, Config) {
        let dir = TempDir::new();
        let key = dir.path().join("client.key");
        std::fs::write(&key, "sk-test-token\n").unwrap();
        let key = key.display().to_string().replace('\\', "/");

        let prompts = dir.path().join("system-prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(prompts.join("orca-v4.md"), "x").unwrap();
        std::fs::write(prompts.join("nova-4.5.md"), "x").unwrap();
        let prompts = prompts.display().to_string().replace('\\', "/");

        let toml = format!(
            r#"
            system_prompts_dir = "{prompts}"

            [[harness]]
            id = "claude-code"
            name = "Claude Code"
            kind = "claude-code"
            bin = "fastpick-test-claude"

            [[harness]]
            id = "codex"
            name = "Codex"
            kind = "codex"
            bin = "fastpick-test-codex"

            [[provider]]
            id = "acme"
            name = "Acme"
            group = "acme.invalid"
            auth_token_file = "{key}"
            small_fast_model = "orca-v4-flash"

            [provider.harness.claude-code]
            base_url = "https://acme.invalid"

            [[provider.model]]
            id = "orca-v4-pro"
            context_window = 1000000

            [[provider]]
            id = "builtin"
            name = "Built in"

            [provider.harness.claude-code]

            [[provider.model]]
            id = "claude-opus-5"
            "#
        );
        let cfg = Config::parse(&toml).unwrap();
        (dir, cfg)
    }

    /// One provider holding two keys, each binding a different harness and listing its own
    /// model. No key file and no catalogue, so nothing here touches the disk or the network.
    fn multi_key_fixture() -> (TempDir, Config) {
        let dir = TempDir::new();
        let prompts = dir.path().join("system-prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        let prompts = prompts.display().to_string().replace('\\', "/");

        let toml = format!(
            r#"
            system_prompts_dir = "{prompts}"

            [[harness]]
            id = "claude-code"
            name = "Claude Code"
            kind = "claude-code"
            bin = "fastpick-test-claude"

            [[harness]]
            id = "codex"
            name = "Codex"
            kind = "codex"
            bin = "fastpick-test-codex"

            [[provider]]
            id = "duo"
            name = "Duo"

            [[provider.key]]
            id = "left"
            label = "Left seat"

            [provider.key.harness.claude-code]
            base_url = "https://left.invalid"

            [[provider.key.model]]
            id = "left-model"

            [[provider.key]]
            id = "right"
            label = "Right seat"

            [provider.key.harness.codex]
            base_url = "https://right.invalid"

            [[provider.key.model]]
            id = "right-model"
            "#
        );
        let cfg = Config::parse(&toml).unwrap();
        (dir, cfg)
    }

    fn listing_json(provider: Option<&str>) -> (TempDir, Value) {
        let (dir, cfg) = fixture();
        let out = listing(&cfg, Path::new("config.toml"), provider, false).unwrap();
        let v = serde_json::to_value(&out).unwrap();
        (dir, v)
    }

    #[test]
    fn a_harness_lists_only_the_providers_it_can_reach() {
        let (_d, v) = listing_json(None);
        let by_id = |id: &str| -> Value {
            v["harnesses"]
                .as_array()
                .unwrap()
                .iter()
                .find(|h| h["id"] == id)
                .unwrap()
                .clone()
        };
        assert_eq!(
            by_id("claude-code")["providers"],
            serde_json::json!(["acme", "builtin"])
        );
        // Nothing binds Codex, so offering it a provider would offer a launch that fails.
        assert_eq!(by_id("codex")["providers"], serde_json::json!([]));
    }

    #[test]
    fn a_harness_reports_what_it_cannot_do() {
        let (_d, v) = listing_json(None);
        let codex = v["harnesses"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["id"] == "codex")
            .unwrap()
            .clone();
        assert_eq!(codex["supportsSystemPrompts"], false);
        assert_eq!(codex["supportsEffort"], false);
    }

    #[test]
    fn a_key_is_reported_as_present_never_as_a_path_or_a_value() {
        let (_d, v) = listing_json(None);
        let acme = v["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "acme")
            .unwrap()
            .clone();
        assert_eq!(acme["keys"][0]["needsKey"], true);
        assert_eq!(acme["keys"][0]["keyPresent"], true);
        // Written the short way, so the single key answers to the provider's own id.
        assert_eq!(acme["keys"][0]["id"], "acme");

        let whole = serde_json::to_string(&v).unwrap();
        assert!(!whole.contains("sk-test-token"), "the key value leaked");
        assert!(!whole.contains("client.key"), "the key path leaked");
    }

    #[test]
    fn a_native_provider_needs_no_key_and_declares_an_empty_binding() {
        let (_d, v) = listing_json(None);
        let builtin = v["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "builtin")
            .unwrap()
            .clone();
        assert_eq!(builtin["keys"][0]["needsKey"], false);
        assert_eq!(builtin["keys"][0]["keyPresent"], false);
        // Present but empty: "change nothing", which is not the same as no binding.
        assert!(builtin["keys"][0]["harnesses"]["claude-code"].is_object());
        assert!(builtin["keys"][0]["harnesses"]["claude-code"]["baseUrl"].is_null());
    }

    #[test]
    fn models_are_absent_until_a_provider_is_named() {
        let (_d, v) = listing_json(None);
        assert!(v.get("models").is_none());
    }

    #[test]
    fn a_named_provider_carries_its_models_and_where_they_came_from() {
        let (_d, v) = listing_json(Some("acme"));
        assert_eq!(v["models"]["provider"], "acme");
        // No catalogue declared, so the config list is the whole truth and nothing was
        // fetched.
        assert_eq!(v["models"]["source"]["kind"], "config");
        let items = v["models"]["items"].as_array().unwrap();
        assert_eq!(items[0]["id"], "orca-v4-pro");
        assert_eq!(items[0]["contextWindow"], 1_000_000);
    }

    #[test]
    fn a_model_carries_the_prompt_files_that_match_it() {
        let (_d, v) = listing_json(Some("acme"));
        let items = v["models"]["items"].as_array().unwrap();
        // `orca-v4.md` covers the family, `nova-4.5.md` is a different one.
        assert_eq!(items[0]["prompts"], serde_json::json!(["orca-v4"]));
        assert_eq!(v["prompts"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn an_unknown_provider_is_an_error_not_an_empty_list() {
        let (_d, cfg) = fixture();
        let err = listing(&cfg, Path::new("config.toml"), Some("nope"), false).unwrap_err();
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn a_dry_run_names_the_credentials_and_prints_none_of_them() {
        let (_d, cfg) = fixture();
        let model = Model::new("orca-v4-pro".into());
        let harness = cfg
            .harnesses
            .iter()
            .find(|h| h.id == "claude-code")
            .unwrap();
        let provider = cfg.providers.iter().find(|p| p.id == "acme").unwrap();
        let sel = Selection {
            harness,
            provider,
            key: 0,
            binding: provider.keys[0].binding("claude-code").unwrap(),
            model: &model,
            effort: None,
            prompts: Vec::new(),
            passthrough: Vec::new(),
        };
        let out = dry_run(&cfg, &sel).unwrap();
        let v = serde_json::to_value(&out).unwrap();

        assert_eq!(v["secretEnv"]["ANTHROPIC_AUTH_TOKEN"]["chars"], 13);
        // Cleared rather than set: an inherited key would otherwise outrank the token.
        assert_eq!(v["secretEnv"]["ANTHROPIC_API_KEY"]["chars"], 0);
        assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "https://acme.invalid");
        assert_eq!(v["args"], serde_json::json!(["--model", "orca-v4-pro"]));
        // One key, so the route id is the bare provider id rather than `acme.acme`.
        assert_eq!(v["key"], "acme");

        let whole = serde_json::to_string(&v).unwrap();
        assert!(!whole.contains("sk-test-token"), "the key value leaked");
    }

    #[test]
    fn each_key_reports_its_own_route_and_owns_the_models_it_serves() {
        let (_d, cfg) = multi_key_fixture();
        let out = listing(&cfg, Path::new("config.toml"), Some("duo"), false).unwrap();
        let v = serde_json::to_value(&out).unwrap();

        let keys = v["providers"][0]["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0]["id"], "left");
        assert_eq!(keys[0]["label"], "Left seat");
        // Two subscriptions, two different sets of harnesses: reporting one map for the
        // provider would offer Codex a binding only the other key has.
        assert!(keys[0]["harnesses"]["claude-code"].is_object());
        assert!(keys[0]["harnesses"].get("codex").is_none());
        assert!(keys[1]["harnesses"]["codex"].is_object());
        assert!(keys[1]["harnesses"].get("claude-code").is_none());

        let items = v["models"]["items"].as_array().unwrap();
        let of = |id: &str| -> Value { items.iter().find(|m| m["id"] == id).unwrap().clone() };
        assert_eq!(of("left-model")["key"], "left");
        assert_eq!(of("right-model")["key"], "right");
        // Several keys answered, and none of them failed.
        assert_eq!(v["models"]["source"]["kind"], "several");
        assert_eq!(v["models"]["source"]["count"], 2);
        assert!(v["models"]["source"].get("failed").is_none());
    }
}
