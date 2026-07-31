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

use crate::config::{Catalog, CatalogAuth, Config, Model, Provider};
use crate::paths::expand;

/// Everything a lookup needs, owned, so it can run on a background thread while the menu
/// stays responsive. Borrowing the config here would pin the fetch to the main thread and
/// freeze the UI for the length of an HTTP timeout.
///
/// One request per key, never per provider: a key is asked what *it* may use, and two keys
/// on the same site answer differently because they sit in different groups.
#[derive(Debug, Clone)]
pub struct Request {
    pub provider_id: String,
    /// Index of the key inside its provider, carried through so a model row can say which
    /// credential serves it.
    pub key_idx: usize,
    pub key_id: String,
    pub key_label: Option<String>,
    pub catalog: Option<Catalog>,
    pub token_file: Option<String>,
    pub config_models: Vec<Model>,
    pub ttl_secs: u64,
    pub force: bool,
}

impl Request {
    pub fn new(cfg: &Config, p: &Provider, key_idx: usize, force: bool) -> Request {
        let k = &p.keys[key_idx];
        Request {
            provider_id: p.id.clone(),
            key_idx,
            key_id: k.id.clone(),
            key_label: k.label.clone(),
            catalog: k.catalog.clone(),
            token_file: k.auth_token_file.clone(),
            config_models: k.models.clone(),
            ttl_secs: cfg.catalog_ttl_secs,
            force,
        }
    }
}

/// One request per key that can serve `harness_id`, or per key outright when no harness
/// narrows the list, which is what a bare `--list --provider X` wants.
pub fn requests_for(
    cfg: &Config,
    p: &Provider,
    harness_id: Option<&str>,
    force: bool,
) -> Vec<Request> {
    let idx: Vec<usize> = match harness_id {
        Some(h) => p.keys_for(h),
        None => (0..p.keys.len()).collect(),
    };
    idx.into_iter()
        .map(|i| Request::new(cfg, p, i, force))
        .collect()
}

/// A model and the key that serves it. The pair is what the menu shows and what the launch
/// resolves, so the endpoint and the token can never come from two different blocks.
#[derive(Debug, Clone)]
pub struct Listed {
    pub key: usize,
    pub key_label: Option<String>,
    pub model: Model,
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
    /// Read from disk because it was still fresh, with its age in seconds.
    Cache(usize, u64),
    /// Read from disk because the fetch failed, with its age and the reason it failed.
    /// Kept apart from `Cache` so a provider broken by a revoked key stops looking healthy
    /// for as long as its cache survives.
    Stale(usize, u64, String),
    /// No catalogue declared, the config list is the whole truth.
    Config(usize),
    /// The fetch failed and nothing was cached, so the config list is all there is.
    Failed(String),
    /// Several keys answered and their lists were merged. One failing key does not empty the
    /// menu, so the failures are carried alongside the count rather than replacing it, one
    /// `<key>: <reason>` per line.
    Several { count: usize, failed: Vec<String> },
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Source::Live(n) => format!("{n} models, live"),
            Source::Cache(n, age) => format!("{n} models, cached {}", human_age(*age)),
            Source::Stale(n, age, e) => {
                format!("{n} models, cached {} ({e})", human_age(*age))
            }
            Source::Config(n) => format!("{n} models, from the config"),
            Source::Failed(e) => format!("catalogue unreachable ({e}), config list only"),
            Source::Several { count, failed } if failed.is_empty() => format!("{count} models"),
            Source::Several { count, failed } => {
                format!("{count} models, {} key(s) unreachable", failed.len())
            }
        }
    }

    /// One `<key>: <reason>` per key whose catalogue could not be reached. Empty for every
    /// other kind, whose single failure the label already carries.
    pub fn failures(&self) -> &[String] {
        match self {
            Source::Several { failed, .. } => failed,
            _ => &[],
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

/// One file per key, not per provider. Two keys on the same site list different models, so a
/// single file would have each fetch overwrite the other's answer.
fn cache_path(provider_id: &str, key_id: &str) -> Option<PathBuf> {
    // The ids are used as a file name, so keep them to something a file system accepts.
    let safe = |raw: &str| -> String {
        raw.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    // Collapsing every other character to `_` makes `acme.a`, `acme-a` and `acme_a` the
    // same file, and one key would then show another's models. The suffix keeps the name
    // readable while making it unique again, and it is taken over the pair so that two
    // providers whose ids differ only in punctuation cannot collide through their keys
    // either.
    let tag = fnv1a(&format!("{provider_id}\u{0}{key_id}"));
    crate::config::config_dir().map(|d| {
        d.join("catalog").join(format!(
            "{}_{}-{tag:08x}.json",
            safe(provider_id),
            safe(key_id)
        ))
    })
}

/// FNV-1a, 32 bits. Not a checksum, just enough to tell two ids apart in a file name.
fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
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

    // `redirects(0)` is not a preference. ureq strips `authorization` and `cookie` when it
    // follows a redirect, but not `x-api-key`, so a catalogue host answering 302 would be
    // handed the raw Anthropic key for whatever location it names.
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(Duration::from_secs(15))
        .build();
    let mut req = agent.get(&cat.url);
    match cat.auth {
        CatalogAuth::None => {}
        CatalogAuth::XApiKey => {
            let token = read_token(r.token_file.as_ref())
                .ok_or_else(|| anyhow!("no key file to authenticate with"))?;
            req = req.set("x-api-key", &token);
            req = req.set("anthropic-version", "2023-06-01");
        }
        CatalogAuth::Bearer => {
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

    let items = match body.get("data").and_then(|d| d.as_array()) {
        Some(items) => items,
        None => {
            // Several providers answer 200 with an error object. Reporting "no `data`
            // array" there throws away the one sentence that says what went wrong.
            let reason = body
                .get("error")
                .and_then(|e| e.get("message").or(Some(e)))
                .map(|m| m.as_str().map(|s| s.to_string()).unwrap_or(m.to_string()));
            return Err(match reason {
                Some(r) => anyhow!("the catalogue refused: {}", first_line(&r)),
                None => anyhow!("no `data` array in the answer"),
            });
        }
    };

    let mut out = Vec::new();
    let mut refused = 0usize;
    for item in items {
        let Some(id) = item.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        // The id travels to a command line, and on Windows through `cmd.exe` on the way.
        // This is a remote answer, so the id is a value the server picks: anything that is
        // not a plain name is dropped rather than trusted.
        if !crate::config::model_id_is_plain(id) {
            refused += 1;
            continue;
        }
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
        return Err(match (items.is_empty(), refused) {
            (true, _) => anyhow!("the catalogue is empty"),
            (false, n) if n == items.len() => {
                anyhow!("the catalogue answered {n} models and none has a usable id")
            }
            // Distinguishable from an outage on purpose: an `only_prefixes` or
            // `exclude_contains` that is one character off looks exactly like a dead
            // endpoint otherwise.
            _ => anyhow!(
                "the filters dropped all {} models the catalogue listed",
                items.len()
            ),
        });
    }
    Ok(out)
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or(s).trim();
    if line.chars().count() > 200 {
        format!("{}...", line.chars().take(200).collect::<String>())
    } else {
        line.to_string()
    }
}

/// ureq renders a failed call as several lines including the whole body. One line is
/// enough on a status bar.
///
/// The body is read rather than dropped: a bare `HTTP 401` reads the same for a wrong key,
/// an expired key, a wrong url path and a wrong organisation, and that is the failure users
/// actually hit.
fn short_http_error(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, r) => {
            // A 3xx only reaches here because redirects are off, and "HTTP 302" would say
            // nothing about why the key was not sent along.
            if (300..400).contains(&code) {
                let to = r.header("location").unwrap_or("elsewhere").to_string();
                return format!("HTTP {code}: the catalogue redirects to {to}, not followed because the key must not travel there. Point `url` at the final address.");
            }
            match r.into_string() {
                Ok(body) if !body.trim().is_empty() => {
                    format!("HTTP {code}: {}", first_line(&body))
                }
                _ => format!("HTTP {code}"),
            }
        }
        ureq::Error::Transport(t) => {
            let s = t.to_string();
            s.lines().next().unwrap_or("transport error").to_string()
        }
    }
}

fn read_cache(provider_id: &str, key_id: &str) -> Option<Cached> {
    let raw = std::fs::read_to_string(cache_path(provider_id, key_id)?).ok()?;
    serde_json::from_str(&raw).ok()
}

/// How many models the last fetch left on disk for this provider, summed over the keys that
/// can serve the harness, config overrides included. Read from the cache only: the menu
/// shows it before anything is fetched, so it must never touch the network.
///
/// `None` as soon as one key has nothing on disk. A partial sum would read as the whole
/// list, which is worse than admitting the count is not known yet.
pub fn cached_count(p: &Provider, harness_id: &str) -> Option<usize> {
    let keys = p.keys_for(harness_id);
    if keys.is_empty() {
        return None;
    }
    let mut total = 0;
    for i in keys {
        let k = &p.keys[i];
        total += match k.catalog {
            None => k.models.len(),
            Some(_) => {
                let c = read_cache(&p.id, &k.id)?;
                merge(&k.models, &c.models).len()
            }
        };
    }
    Some(total)
}

fn write_cache(provider_id: &str, key_id: &str, models: &[Entry]) {
    let Some(path) = cache_path(provider_id, key_id) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let payload = Cached {
        fetched_at: now(),
        models: models.to_vec(),
    };
    let Ok(raw) = serde_json::to_string(&payload) else {
        return;
    };
    // Written beside the target and renamed over it. A plain write truncates first, so a
    // second fastpick refreshing the same provider, or a kill halfway through, leaves a
    // half-file that parses as nothing and silently drops the whole catalogue.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, raw).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
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

    let cached = read_cache(&r.provider_id, &r.key_id);
    let fresh = cached
        .as_ref()
        .filter(|c| !r.force && age_of(c).is_some_and(|age| age < r.ttl_secs));

    if let Some(c) = fresh {
        let merged = merge(&r.config_models, &c.models);
        let n = merged.len();
        return (merged, Source::Cache(n, age_of(c).unwrap_or(0)));
    }

    match fetch(r) {
        Ok(entries) => {
            write_cache(&r.provider_id, &r.key_id, &entries);
            let merged = merge(&r.config_models, &entries);
            let n = merged.len();
            (merged, Source::Live(n))
        }
        Err(e) => match cached {
            Some(c) => {
                let merged = merge(&r.config_models, &c.models);
                let n = merged.len();
                // The age alone would read as "simply not expired yet" and a provider
                // broken by a revoked key would look healthy for as long as the cache
                // survives. The reason travels with it.
                (
                    merged,
                    Source::Stale(n, age_of(&c).unwrap_or(0), e.to_string()),
                )
            }
            None => (r.config_models.clone(), Source::Failed(e.to_string())),
        },
    }
}

/// How long ago the cache was written, or `None` if it claims to come from the future.
///
/// A `fetched_at` ahead of the clock means the machine's time moved back, or the file was
/// copied from another machine. Subtracting saturates to 0 there, which reads as "written
/// this second" and pins the entry fresh until the clock catches up.
fn age_of(c: &Cached) -> Option<u64> {
    now().checked_sub(c.fetched_at)
}

/// Every key's list, concatenated in config order, each row carrying the key that serves it.
///
/// The lookups run at once, one thread per key: they are independent HTTP calls to the same
/// site, so doing them in sequence would make a three-key provider three timeouts slow for
/// no reason. A single key returns its own `Source` untouched, so a provider that never grew
/// a second one reads exactly as it did before.
pub fn run_all(reqs: Vec<Request>) -> (Vec<Listed>, Source) {
    if reqs.len() == 1 {
        let r = &reqs[0];
        let (models, source) = run(r);
        let rows = models
            .into_iter()
            .map(|model| Listed {
                key: r.key_idx,
                key_label: r.key_label.clone(),
                model,
            })
            .collect();
        return (rows, source);
    }

    let handles: Vec<_> = reqs
        .into_iter()
        .map(|r| std::thread::spawn(move || (r.clone(), run(&r))))
        .collect();

    let mut rows = Vec::new();
    let mut failed = Vec::new();
    for h in handles {
        // A panicking lookup thread is not worth taking the menu down for: the other keys
        // still have something to show, so it is reported like any unreachable catalogue.
        let Ok((r, (models, source))) = h.join() else {
            failed.push("a catalogue lookup panicked".to_string());
            continue;
        };
        // `Stale` counts too: the list is there, but it is the one a failed fetch fell back
        // on, and saying so is the whole point of keeping the two apart.
        match &source {
            Source::Failed(e) => failed.push(format!("{}: {e}", r.key_id)),
            Source::Stale(_, _, e) => failed.push(format!("{}: {e}", r.key_id)),
            _ => {}
        }
        rows.extend(models.into_iter().map(|model| Listed {
            key: r.key_idx,
            key_label: r.key_label.clone(),
            model,
        }));
    }

    let count = rows.len();
    (rows, Source::Several { count, failed })
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
