//! Asking a provider what it actually serves, instead of trusting a hand-written list.
//!
//! A static list rots: a model gets added upstream and nobody notices, or one gets removed
//! and the launch fails with a `model_not_found` that looks like a config bug. So the menu
//! is built from the provider's own `/v1/models`, cached on disk so it still opens offline,
//! with the config layered on top for the things no API reports (labels, context windows,
//! effort levels) and for ids the API does not list but that work anyway.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::{Catalog, Config, Model, Provider};
use crate::paths::expand;

/// Everything a lookup needs, owned, so it can run on a background thread while the menu
/// stays responsive. Borrowing the config here would pin the fetch to the main thread and
/// freeze the UI for the length of an HTTP timeout.
#[derive(Debug, Clone)]
pub struct Request {
    pub provider_id: String,
    pub catalog: Option<Catalog>,
    pub token_file: Option<String>,
    pub config_models: Vec<Model>,
    pub ttl_secs: u64,
    pub force: bool,
}

impl Request {
    pub fn new(cfg: &Config, p: &Provider, force: bool) -> Request {
        Request {
            provider_id: p.id.clone(),
            catalog: p.catalog.clone(),
            token_file: p.auth_token_file.clone(),
            config_models: p.models.clone(),
            ttl_secs: cfg.catalog_ttl_secs,
            force,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Cached {
    fetched_at: u64,
    models: Vec<Entry>,
}

/// Where the model list on screen came from. Shown in the menu so a stale or fallback list
/// never passes for a live one.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// Fetched just now.
    Live(usize),
    /// Read from disk, with its age in seconds.
    Cache(usize, u64),
    /// No catalogue declared, the config list is the whole truth.
    Config(usize),
    /// The fetch failed and nothing was cached, so the config list is all there is.
    Failed(String),
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Source::Live(n) => format!("{n} models, live"),
            Source::Cache(n, age) => format!("{n} models, cached {}", human_age(*age)),
            Source::Config(n) => format!("{n} models, from the config"),
            Source::Failed(e) => format!("catalogue unreachable ({e}), config list only"),
        }
    }
}

fn human_age(secs: u64) -> String {
    if secs < 90 {
        format!("{secs}s ago")
    } else if secs < 90 * 60 {
        format!("{}m ago", secs / 60)
    } else if secs < 48 * 3600 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn cache_path(provider_id: &str) -> Option<PathBuf> {
    // The id is used as a file name, so keep it to something a file system accepts.
    let safe: String = provider_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    crate::config::config_dir().map(|d| d.join("catalog").join(format!("{safe}.json")))
}

fn read_token(token_file: Option<&String>) -> Option<String> {
    let path = expand(token_file?);
    let raw = std::fs::read_to_string(path).ok()?;
    let t = raw.trim().to_string();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// One HTTP call, parsed leniently: providers agree on `data[].id` and disagree on
/// everything else, so only the id is required.
fn fetch(r: &Request) -> Result<Vec<Entry>> {
    let cat = r
        .catalog
        .as_ref()
        .ok_or_else(|| anyhow!("no catalogue declared"))?;

    let mut req = ureq::get(&cat.url).timeout(Duration::from_secs(15));
    match cat.auth.as_str() {
        "none" => {}
        "x-api-key" => {
            let token = read_token(r.token_file.as_ref())
                .ok_or_else(|| anyhow!("no key file to authenticate with"))?;
            req = req.set("x-api-key", &token);
            req = req.set("anthropic-version", "2023-06-01");
        }
        _ => {
            let token = read_token(r.token_file.as_ref())
                .ok_or_else(|| anyhow!("no key file to authenticate with"))?;
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
    }

    let body: serde_json::Value = req
        .call()
        .map_err(|e| anyhow!(short_http_error(e)))?
        .into_json()
        .context("the catalogue answered something that is not JSON")?;

    let items = body
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| anyhow!("no `data` array in the answer"))?;

    let mut out = Vec::new();
    for item in items {
        let Some(id) = item.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        if !cat.only_prefixes.is_empty() && !cat.only_prefixes.iter().any(|pre| id.starts_with(pre))
        {
            continue;
        }
        if cat.exclude_contains.iter().any(|bad| id.contains(bad)) {
            continue;
        }
        out.push(Entry {
            id: id.to_string(),
            context_length: item
                .get("context_length")
                .or_else(|| item.get("context_window"))
                .and_then(|v| v.as_u64()),
            label: item
                .get("display_name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        });
    }
    if out.is_empty() {
        return Err(anyhow!("the catalogue listed no usable model"));
    }
    Ok(out)
}

/// ureq renders a failed call as several lines including the whole body. One line is
/// enough on a status bar.
fn short_http_error(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, _) => format!("HTTP {code}"),
        ureq::Error::Transport(t) => {
            let s = t.to_string();
            s.lines().next().unwrap_or("transport error").to_string()
        }
    }
}

fn read_cache(provider_id: &str) -> Option<Cached> {
    let raw = std::fs::read_to_string(cache_path(provider_id)?).ok()?;
    serde_json::from_str(&raw).ok()
}

/// How many models the last fetch left on disk for this provider, config overrides
/// included. Read from the cache only: the menu shows it before anything is fetched, so it
/// must never touch the network.
pub fn cached_count(config_models: &[Model], provider_id: &str) -> Option<usize> {
    let c = read_cache(provider_id)?;
    Some(merge(config_models, &c.models).len())
}

fn write_cache(provider_id: &str, models: &[Entry]) {
    let Some(path) = cache_path(provider_id) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let payload = Cached {
        fetched_at: now(),
        models: models.to_vec(),
    };
    if let Ok(raw) = serde_json::to_string(&payload) {
        let _ = std::fs::write(path, raw);
    }
}

/// The model list for a provider, plus where it came from.
///
/// `force` skips the freshness check and always goes to the network. Everything that fails
/// degrades one step rather than erroring: live, then cache, then the config list.
pub fn run(r: &Request) -> (Vec<Model>, Source) {
    if r.catalog.is_none() {
        return (
            r.config_models.clone(),
            Source::Config(r.config_models.len()),
        );
    }

    let cached = read_cache(&r.provider_id);
    let fresh = cached
        .as_ref()
        .filter(|c| !r.force && now().saturating_sub(c.fetched_at) < r.ttl_secs);

    if let Some(c) = fresh {
        let merged = merge(&r.config_models, &c.models);
        let n = merged.len();
        return (merged, Source::Cache(n, now().saturating_sub(c.fetched_at)));
    }

    match fetch(r) {
        Ok(entries) => {
            write_cache(&r.provider_id, &entries);
            let merged = merge(&r.config_models, &entries);
            let n = merged.len();
            (merged, Source::Live(n))
        }
        Err(e) => match cached {
            Some(c) => {
                let merged = merge(&r.config_models, &c.models);
                let n = merged.len();
                (merged, Source::Cache(n, now().saturating_sub(c.fetched_at)))
            }
            None => (r.config_models.clone(), Source::Failed(e.to_string())),
        },
    }
}

/// Config first, then whatever the provider listed.
///
/// Config-declared ids come first on purpose: they are curated, and some of them are not in
/// the catalogue at all yet still work, `claude-opus-5[1m]` being the standing example. The
/// rest of the catalogue follows in the order the provider returned it, each entry picking
/// up any override the config declares for it.
fn merge(config_models: &[Model], entries: &[Entry]) -> Vec<Model> {
    let mut out: Vec<Model> = Vec::with_capacity(entries.len() + config_models.len());

    for m in config_models {
        if !entries.iter().any(|e| e.id == m.id) {
            out.push(m.clone());
        }
    }

    for e in entries {
        let mut m = match config_models.iter().find(|m| m.id == e.id) {
            Some(o) => o.clone(),
            None => Model::new(e.id.clone()),
        };
        if m.label.is_none() {
            m.label = e.label.clone();
        }
        if m.context_window.is_none() {
            m.context_window = e.context_length;
        }
        out.push(m);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_only_id_survives_a_catalogue_that_never_lists_it() {
        let config_models = vec![Model::new("claude-opus-5[1m]".into())];
        let live = vec![Entry {
            id: "claude-opus-5".into(),
            context_length: None,
            label: None,
        }];
        let merged = merge(&config_models, &live);
        assert_eq!(merged[0].id, "claude-opus-5[1m]");
        assert_eq!(merged[1].id, "claude-opus-5");
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn an_override_wins_over_the_catalogue_but_only_where_it_says_something() {
        let mut over = Model::new("m".into());
        over.label = Some("Nice name".into());

        let live = vec![Entry {
            id: "m".into(),
            context_length: Some(262144),
            label: Some("api name".into()),
        }];
        let merged = merge(&[over], &live);
        assert_eq!(merged.len(), 1, "the id must not be listed twice");
        assert_eq!(merged[0].label.as_deref(), Some("Nice name"));
        // The config said nothing about the window, so the catalogue fills it in.
        assert_eq!(merged[0].context_window, Some(262144));
    }

    #[test]
    fn age_reads_in_the_right_unit() {
        assert_eq!(human_age(30), "30s ago");
        assert_eq!(human_age(600), "10m ago");
        assert_eq!(human_age(7200), "2h ago");
        assert_eq!(human_age(3 * 86400), "3d ago");
    }
}
