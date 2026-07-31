//! fastpick: pick a coding harness (Claude Code, Codex, OpenCode), a provider and a model,
//! then launch that agent with the environment the combination needs.
//!
//! The menu asks in that order because that is the order the answers constrain each other:
//! a harness can only reach the providers wired to it, and a provider only serves the
//! models it serves. Each step can be answered on the command line instead, and any step
//! answered there is not asked.

mod catalog;
mod config;
mod json;
mod launch;
mod paths;
mod prompts;
mod secrets;
mod state;
mod tui;
mod update;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const HELP: &str = "\
fastpick - pick a coding agent, a provider and a model, then launch it

USAGE
  fastpick [options] [-- <args passed to the agent>]

OPTIONS
  -c, --config <file>    config file to use (default: the per-user one, see PATHS)
  -e, --edit             open the config in $VISUAL, $EDITOR or the system default
  -l, --list             print the harnesses, providers and models, then exit
      --paths            print every path fastpick uses and the state of each key file
      --set-key <id>     read a key from stdin and write it to that key file, owner-readable.
                         <id> is a provider, or <provider>.<key> when it holds several.
                         Never pass a key as an argument
  -n, --dry-run          show the command and environment a launch would use, run nothing
      --json             print --list or --dry-run as JSON instead, for another program
  -r, --refresh          refetch the model catalogue instead of using the cached one
      --harness <id>     skip the harness screen
      --provider <id>    skip the provider screen
      --model <id>       skip the model screen
      --effort <level>   effort level, when the harness takes one
      --md <file>        system prompt file, repeatable. A bare name is resolved inside the
                         prompts folder; anything else is taken as a path
      --no-md            launch without a system prompt even if one matches the model
  -u, --update           install the newest signed release over this binary
  -h, --help             this text
  -V, --version          version

Each of --harness, --provider and --model skips its own screen; the menu opens on the
first one you left out. Anything not recognised is forwarded to the agent, so
`fastpick -p \"hello\"` works. Use `--` when an argument would otherwise be read as a
fastpick option.

Under --json, exit code 0 means stdout holds one JSON document and nothing else; every
notice and every error goes to stderr. `--list --json` describes the config without
touching the network, and adding --provider <id> also lists that provider's models.
No credential is ever printed, in either mode.

Only the harnesses whose binary is installed are offered. --harness names one anyway.

KEYS
  up/down   move             right   next screen, or the options panel on a model
  left      back             enter   launch
  space     change the row under the cursor in the options panel
  tab       refetch the model catalogue for this provider
  a         list every file in the prompts folder, not only the ones matching the model
  type      filter the model list

PATHS
  config    %APPDATA%\\fastpick\\config.toml, or $XDG_CONFIG_HOME/fastpick/config.toml,
            or ~/.config/fastpick/config.toml
  catalog   <config dir>/catalog/, one JSON per key, refreshed on a timer
  prompts   whatever `system_prompts_dir` points at, `<config dir>/system-prompts` by default
  keys      whatever each key's `auth_token_file` points at. Run --paths to see them
";

fn main() -> ExitCode {
    match real_main() {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            eprintln!("fastpick: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Default)]
struct Args {
    config: Option<PathBuf>,
    edit: bool,
    paths: bool,
    set_key: Option<String>,
    list: bool,
    dry_run: bool,
    json: bool,
    refresh: bool,
    harness: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    md: Vec<String>,
    no_md: bool,
    update: bool,
    passthrough: Vec<String>,
}

fn parse_args() -> std::result::Result<Option<Args>, String> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("fastpick {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-c" | "--config" => {
                args.config = Some(PathBuf::from(it.next().ok_or("--config needs a file")?));
            }
            "-e" | "--edit" => args.edit = true,
            "--paths" => args.paths = true,
            "--set-key" => args.set_key = Some(it.next().ok_or("--set-key needs a provider id")?),
            "-l" | "--list" => args.list = true,
            "-n" | "--dry-run" => args.dry_run = true,
            "--json" => args.json = true,
            "-r" | "--refresh" => args.refresh = true,
            "--harness" => args.harness = Some(it.next().ok_or("--harness needs an id")?),
            "--provider" => args.provider = Some(it.next().ok_or("--provider needs an id")?),
            "--model" => args.model = Some(it.next().ok_or("--model needs an id")?),
            "--effort" => args.effort = Some(it.next().ok_or("--effort needs a level")?),
            "--md" => args.md.push(it.next().ok_or("--md needs a file")?),
            "--no-md" => args.no_md = true,
            "-u" | "--update" => args.update = true,
            "--" => {
                args.passthrough.extend(it);
                break;
            }
            // A near miss on a fastpick option is a typo, not something to forward. Passed
            // through, `--modle gpt-5` silently opened the full menu and handed the agent
            // an argument it does not know either.
            _ if looks_like_a_typo(&a) => {
                return Err(format!("unknown option `{a}`. Use `--` before it if the agent is meant to receive it, or see --help."));
            }
            _ => args.passthrough.push(a),
        }
    }
    if args.no_md && !args.md.is_empty() {
        return Err("--md and --no-md contradict each other".into());
    }
    Ok(Some(args))
}

/// Whether an unrecognised `--word` is close enough to one of ours to be a mistake.
///
/// Everything else is forwarded, because forwarding is the documented behaviour that makes
/// `fastpick -p "hello"` work. Only a one-character slip on a known name is refused.
fn looks_like_a_typo(a: &str) -> bool {
    const KNOWN: [&str; 10] = [
        "--config",
        "--list",
        "--dry-run",
        "--refresh",
        "--harness",
        "--provider",
        "--model",
        "--effort",
        "--no-md",
        "--update",
    ];
    let Some(name) = a.split('=').next() else {
        return false;
    };
    if !name.starts_with("--") || KNOWN.contains(&name) || name == "--md" || name == "--help" {
        return false;
    }
    KNOWN.iter().any(|k| is_near_miss(name, k))
}

/// One slip apart: a substitution, an insertion, a deletion, or two adjacent letters
/// swapped. Bounded at one on purpose, so a genuinely different flag is never called a typo
/// and stays forwarded to the agent.
fn is_near_miss(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }

    if a.len() == b.len() {
        let diffs: Vec<usize> = (0..a.len()).filter(|&i| a[i] != b[i]).collect();
        return match diffs[..] {
            [_] => true,
            // A swap is two substitutions, so it needs its own arm, and it is the most
            // common way to mistype a word.
            [i, j] => j == i + 1 && a[i] == b[j] && a[j] == b[i],
            _ => false,
        };
    }

    // One insertion or deletion: the shorter must be the longer with exactly one character
    // skipped.
    let (short, long) = if a.len() < b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    let mut i = 0;
    let mut skipped = false;
    for c in long.iter() {
        if i < short.len() && short[i] == *c {
            i += 1;
        } else if skipped {
            return false;
        } else {
            skipped = true;
        }
    }
    i == short.len()
}

fn real_main() -> Result<i32> {
    let Some(args) = parse_args().map_err(anyhow::Error::msg)? else {
        return Ok(0);
    };

    // A JSON menu is not a thing, so the flag only means something once a mode that
    // already prints rather than launches has been asked for.
    if args.json && !args.list && !args.dry_run {
        anyhow::bail!("--json describes a run rather than opening one: add --list or --dry-run");
    }

    // Whatever the last `--update` could not delete while it was running.
    update::sweep_leftovers();

    // Before the config: updating is the one thing that has to work on a machine whose
    // config is broken, and it is often what fixes it.
    if args.update {
        return update::run();
    }

    let cfg_path = match args.config.clone() {
        Some(p) => p,
        None => config::config_path().context("no config directory available on this system")?,
    };

    if config::ensure_config(&cfg_path)? {
        // Under --json this is a failure, not a notice: the starter config declares
        // example providers, and a caller has nothing to consume from them.
        if args.json {
            anyhow::bail!(
                "a starter config was written at {}. Edit it, then ask again",
                cfg_path.display()
            );
        }
        println!("fastpick wrote a starter config at {}", cfg_path.display());
        println!("Edit it, then run fastpick again. It ships one block per kind of harness");
        println!("and per kind of provider, each commented.");
        return Ok(0);
    }

    // Before the parse on purpose: a config that no longer loads is exactly when opening it
    // in an editor is the thing you want.
    if args.edit {
        return edit(&cfg_path);
    }

    let cfg = config::Config::load(&cfg_path)?;

    if args.paths {
        print_paths(&cfg, &cfg_path);
        return Ok(0);
    }

    if let Some(id) = &args.set_key {
        return set_key(&cfg, id);
    }

    // Before `--list`, not after. Listing used to run first, so `--list --provider typo`
    // printed the whole catalogue and exited 0 as though the id had been honoured.
    if let Some(id) = &args.harness {
        if !cfg.harnesses.iter().any(|h| &h.id == id) {
            anyhow::bail!("no harness with id `{id}`, see --list");
        }
    }
    if let Some(id) = &args.provider {
        if !cfg.providers.iter().any(|p| &p.id == id) {
            anyhow::bail!("no provider with id `{id}`, see --list");
        }
    }

    if args.list {
        if args.json {
            let listing = json::listing(&cfg, &cfg_path, args.provider.as_deref(), args.refresh)?;
            json::print(&listing)?;
        } else {
            print_list(&cfg, &args)?;
        }
        return Ok(0);
    }

    let saved = state::load();

    // A flag names a thing and skips its screen. The saved state only moves the cursor.
    let harness_idx = match &args.harness {
        Some(id) => {
            let idx = cfg
                .harnesses
                .iter()
                .position(|h| &h.id == id)
                .with_context(|| format!("no harness with id `{id}`, see --list"))?;
            // Named explicitly, so it runs even when the binary was not found. A warning
            // rather than a refusal: the detection can be wrong, the flag cannot.
            let bin = &cfg.harnesses[idx].bin;
            if paths::locate(bin).is_none() {
                eprintln!("fastpick: `{bin}` is not on PATH, launching it anyway");
            }
            Some(idx)
        }
        None => saved
            .last_harness
            .as_ref()
            .and_then(|id| cfg.harnesses.iter().position(|h| &h.id == id)),
    };

    let provider_idx = match &args.provider {
        Some(id) => {
            let idx = cfg
                .providers
                .iter()
                .position(|p| &p.id == id)
                .with_context(|| format!("no provider with id `{id}`, see --list"))?;
            if let Some(h) = harness_idx {
                let harness = &cfg.harnesses[h];
                if !cfg.providers[idx].binds(&harness.id) {
                    anyhow::bail!(
                        "provider `{id}` declares no binding for harness `{}` on any of its keys, so that pair cannot be launched",
                        harness.id
                    );
                }
            }
            Some(idx)
        }
        None => saved
            .last_provider
            .as_ref()
            .and_then(|id| cfg.providers.iter().position(|p| &p.id == id)),
    };

    let start = tui::Start {
        harness_idx,
        provider_idx,
        // Named on the command line only. A remembered model is deliberately not applied:
        // it would launch on a model the user never confirmed for this provider.
        model_id: args.model.clone(),
        skip_harness: args.harness.is_some(),
        skip_provider: args.provider.is_some(),
        refresh: args.refresh,
    };

    let Some(picked) = tui::run(&cfg, &start)? else {
        return Ok(130);
    };

    let harness = &cfg.harnesses[picked.harness_idx];
    let provider = &cfg.providers[picked.provider_idx];
    // The key comes from the model: picking a row is what says which credential serves it,
    // so the endpoint and the token can never be resolved from two different blocks.
    let binding = provider.keys[picked.key]
        .binding(&harness.id)
        .context("the chosen key has no binding for the chosen harness")?;
    state::save(&harness.id, &provider.id, &picked.model.id);

    let prompts = if args.md.is_empty() && !args.no_md {
        picked.prompts
    } else {
        resolve_prompts(&cfg, &picked.model, &args)?
    };

    let sel = launch::Selection {
        harness,
        provider,
        key: picked.key,
        binding,
        model: &picked.model,
        effort: args.effort.clone().or(picked.effort),
        prompts,
        passthrough: args.passthrough.clone(),
    };

    if args.dry_run {
        if args.json {
            json::print(&json::dry_run(&cfg, &sel)?)?;
        } else {
            print_dry_run(&cfg, &sel)?;
        }
        return Ok(0);
    }

    println!(
        "{} · {} · {}{}{}",
        harness.name,
        provider.name,
        sel.model.display(),
        sel.effort
            .as_ref()
            .map(|e| format!(" · effort {e}"))
            .unwrap_or_default(),
        if sel.prompts.is_empty() {
            String::new()
        } else {
            format!(
                " · {}",
                sel.prompts
                    .iter()
                    .filter_map(|p| p.file_name())
                    .map(|n| n.to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join(" + ")
            )
        }
    );

    launch::run(&cfg, &sel)
}

/// System prompt files named with `--md`, or none at all under `--no-md`.
fn resolve_prompts(
    cfg: &config::Config,
    model: &config::Model,
    args: &Args,
) -> Result<Vec<PathBuf>> {
    if args.no_md {
        return Ok(Vec::new());
    }
    let dir = cfg.prompts_dir();

    if args.md.is_empty() {
        let Some(dir) = dir else {
            return Ok(Vec::new());
        };
        return Ok(prompts::matches_for(&dir, model.base_name())
            .into_iter()
            .take(1)
            .map(|f| f.path)
            .collect());
    }

    let mut out = Vec::new();
    for name in &args.md {
        let direct = paths::expand(name);
        if direct.is_file() {
            out.push(direct);
            continue;
        }
        let with_ext = if name.to_lowercase().ends_with(".md") {
            name.clone()
        } else {
            format!("{name}.md")
        };
        match dir.as_ref().map(|d| d.join(&with_ext)) {
            Some(p) if p.is_file() => out.push(p),
            _ => {
                let where_ = dir
                    .map(|d| d.display().to_string())
                    .unwrap_or_else(|| "no prompts folder configured".into());
                anyhow::bail!("--md: no file `{with_ext}` in {where_}, and `{name}` is not a path");
            }
        }
    }
    Ok(out)
}

/// Opens the config in the user's editor.
///
/// `$VISUAL` and `$EDITOR` are command lines rather than program names (`code -w`,
/// `vim -p`), so the first word is the program and the rest are arguments. Without either,
/// the platform's own way of opening a text file is used, and on Linux there is no such
/// thing, so a couple of editors that are always there are tried before giving up.
fn edit(path: &Path) -> Result<i32> {
    let named = ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|v| std::env::var(v).ok())
        .filter(|e| !e.trim().is_empty());

    // `.cmd` throughout: `program` also reports whether a `cmd.exe` sits in front, which
    // matters for the launch path but not here, where the only argument is a path we chose.
    let mut cmd = match &named {
        Some(line) => {
            let mut parts = line.split_whitespace();
            let prog = parts.next().unwrap_or("");
            let mut c = paths::program(prog).cmd;
            for a in parts {
                c.arg(a);
            }
            c
        }
        None if cfg!(windows) => paths::program("notepad").cmd,
        None if cfg!(target_os = "macos") => {
            let mut c = paths::program("open").cmd;
            c.arg("-t");
            c
        }
        None => {
            let found = ["nano", "vi"].iter().find(|b| paths::locate(b).is_some());
            match found {
                Some(b) => paths::program(b).cmd,
                None => anyhow::bail!(
                    "no editor found: set $EDITOR, or open {} yourself",
                    path.display()
                ),
            }
        }
    };

    let status = cmd
        .arg(path)
        .status()
        .with_context(|| format!("opening {}", path.display()))?;
    Ok(status.code().unwrap_or(0))
}

/// Every path fastpick reads or writes, and the state of each key file.
fn print_paths(cfg: &config::Config, cfg_path: &Path) {
    let dir = config::config_dir();
    let show = |p: Option<PathBuf>| {
        p.map(|p| p.display().to_string())
            .unwrap_or_else(|| "(no config directory on this system)".into())
    };
    println!("config    {}", cfg_path.display());
    println!("prompts   {}", show(cfg.prompts_dir()));
    println!(
        "catalog   {}",
        show(dir.as_ref().map(|d| d.join("catalog")))
    );
    println!(
        "state     {}",
        show(dir.as_ref().map(|d| d.join("state.toml")))
    );

    println!("\nharnesses");
    for h in &cfg.harnesses {
        match paths::locate(&h.bin) {
            Some(p) => println!("  {}  [{}]  {}", h.name, h.id, p.display()),
            None => println!("  {}  [{}]  {} is not installed", h.name, h.id, h.bin),
        }
    }

    // The path and the verdict, never the contents. Somebody reading this over a shoulder
    // learns where the key is, which they would learn from the config anyway.
    println!("\nkeys");
    for p in &cfg.providers {
        for (ki, k) in p.keys.iter().enumerate() {
            let route = p.route_id(ki);
            match &k.auth_token_file {
                None => println!("  {}  [{}]  the agent's own login", p.name, route),
                Some(f) => {
                    let path = paths::expand(f);
                    let access = secrets::access(&path);
                    println!(
                        "  {}  [{}]  {}  {}{}",
                        p.name,
                        route,
                        path.display(),
                        access.label(),
                        match access {
                            secrets::Access::Missing => format!("  (fastpick --set-key {route})"),
                            _ => String::new(),
                        }
                    );
                }
            }
        }
    }
}

/// Writes one key, read from stdin so it never reaches a shell history.
///
/// The argument is a route id: `crof` for a provider holding one credential,
/// `codex-everywhere.openai` for one of several. A bare id on a provider holding several is
/// refused rather than guessed at, because writing a subscription over the wrong file is
/// silent until the next launch fails.
fn set_key(cfg: &config::Config, id: &str) -> Result<i32> {
    let Some((pi, ki)) = cfg.route(id) else {
        if let Some(p) = cfg.providers.iter().find(|p| p.id == id) {
            let names: Vec<String> = p.keys.iter().map(|k| format!("{id}.{}", k.id)).collect();
            anyhow::bail!(
                "provider `{id}` holds {} keys, so name the one to write: {}",
                p.keys.len(),
                names.join(", ")
            );
        }
        anyhow::bail!("no provider or key with id `{id}`, see --list");
    };
    let p = &cfg.providers[pi];
    let k = &p.keys[ki];

    let Some(file) = &k.auth_token_file else {
        anyhow::bail!(
            "`{id}` declares no auth_token_file, so it has nowhere to keep a key. \
             Give it one in the config, then run this again"
        );
    };

    let label = match &k.label {
        Some(l) => format!("{} ({l})", p.name),
        None => p.name.clone(),
    };
    let path = paths::expand(file);
    let secret = secrets::read_secret(&format!("Key for {label} (not shown): "))?;
    secrets::write(&path, &secret)?;
    println!(
        "{label}: key written to {}, {}",
        path.display(),
        secrets::access(&path).label()
    );
    Ok(0)
}

fn print_list(cfg: &config::Config, args: &Args) -> Result<()> {
    for h in &cfg.harnesses {
        // --list is the diagnostic view, so a harness that is not installed is shown and
        // labelled rather than hidden the way the menu hides it.
        let installed = match paths::locate(&h.bin) {
            Some(_) => String::new(),
            None => "  (not installed)".to_string(),
        };
        println!("{}  [{}]  {}{}", h.name, h.id, h.bin, installed);
        let mut group: Option<&str> = None;
        for (row, &pi) in cfg.providers_for(&h.id).iter().enumerate() {
            let p = &cfg.providers[pi];
            // Same rule as the menu: a change of group opens a block, separated by a blank
            // line, and the heading is printed only when there is one. Without the blank
            // line an ungrouped provider keeps the indentation of the block above it and
            // reads as belonging to a heading that is not its own.
            if p.group.as_deref() != group || row == 0 {
                if row > 0 {
                    println!();
                }
                if let Some(g) = p.group.as_deref() {
                    println!("  {g}");
                }
                group = p.group.as_deref();
            }
            let keys = p.keys_for(&h.id);
            // One line per key that reaches this harness: two of them are two endpoints, and
            // collapsing them would hide which credential a model is about to travel on.
            if keys.len() < 2 {
                let url = keys
                    .first()
                    .and_then(|&ki| p.keys[ki].binding(&h.id))
                    .and_then(|b| b.base_url.as_deref())
                    .unwrap_or("(the agent's own endpoint)");
                println!("    {}  [{}]  {}", p.name, p.id, url);
            } else {
                println!("    {}  [{}]", p.name, p.id);
                for &ki in &keys {
                    let url = p.keys[ki]
                        .binding(&h.id)
                        .and_then(|b| b.base_url.as_deref())
                        .unwrap_or("(the agent's own endpoint)");
                    println!("      [{}]  {}", p.route_id(ki), url);
                }
            }

            // Listing every model of every provider would mean one HTTP call per provider
            // per harness, so the catalogue is only queried when a provider is named.
            if args.provider.as_deref() == Some(&p.id) {
                let reqs = catalog::requests_for(cfg, p, Some(&h.id), args.refresh);
                let (rows, source) = catalog::run_all(reqs);
                println!("      {}", source.label());
                for line in source.failures() {
                    println!("      {line}");
                }
                for row in &rows {
                    let ctx = row
                        .model
                        .context_window
                        .map(|c| format!("  {}K", c / 1000))
                        .unwrap_or_default();
                    let key = row
                        .key_label
                        .as_deref()
                        .map(|l| format!("  [{l}]"))
                        .unwrap_or_default();
                    println!("      {}{}{}", row.model.id, ctx, key);
                }
            }
        }
        println!();
    }
    if args.provider.is_none() {
        println!("Add --provider <id> to list that provider's models.");
    }
    if let Some(dir) = cfg.prompts_dir() {
        println!("system prompts: {}", dir.display());
    }
    Ok(())
}

/// Whether a variable's value must not be printed.
///
/// By pattern, not by a list of three names. `build` copies arbitrary pairs out of
/// `[provider.env]` and `[provider.harness.*.env]`, so a user who supplies their key as
/// `OPENAI_API_KEY` would otherwise have it echoed in full into the output people paste
/// into bug reports.
///
/// The exceptions are named because they are the reason `--dry-run` is read at all: both
/// carry `TOKEN` in the name and neither is one.
fn is_secret_env(key: &str) -> bool {
    const NOT_SECRETS: [&str; 2] = [
        "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
        "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    ];
    if NOT_SECRETS.contains(&key) {
        return false;
    }
    let upper = key.to_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL"]
        .iter()
        .any(|needle| upper.contains(needle))
}

fn print_dry_run(cfg: &config::Config, sel: &launch::Selection) -> Result<()> {
    let cmd = launch::build(cfg, sel)?;
    println!("env:");
    for (k, v) in cmd.get_envs() {
        let key = k.to_string_lossy();
        let Some(raw) = v else {
            // Told to unset rather than to set. Worth showing: "this variable is being
            // taken away from the child" is half of what --dry-run exists to answer.
            println!("  {key} (removed)");
            continue;
        };
        let val = raw.to_string_lossy().to_string();
        let shown = if is_secret_env(&key) {
            format!("({} chars, hidden)", val.len())
        } else {
            val
        };
        println!("  {key}={shown}");
    }
    print!("cmd: {}", cmd.get_program().to_string_lossy());
    for a in cmd.get_args() {
        print!(" {}", a.to_string_lossy());
    }
    println!();
    if let Some(proxy) = &sel.provider_key().proxy {
        println!("precheck: proxy on 127.0.0.1:{}", proxy.port);
    }
    if let Some(check) = &sel.provider_key().host_check {
        println!("precheck: ping {} ({})", check.host, check.on_down);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anything_that_looks_like_a_credential_is_hidden() {
        // Matched by pattern, because `build` copies arbitrary pairs out of the config's
        // `env` tables and a user's key can be called anything.
        for k in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "OPENAI_API_KEY",
            "OPENROUTER_API_KEY",
            "MY_SECRET",
            "DB_PASSWORD",
            "gh_token",
        ] {
            assert!(is_secret_env(k), "{k} should be hidden");
        }
    }

    #[test]
    fn the_two_variables_dry_run_exists_for_are_not_hidden() {
        // Both carry TOKEN or KEY in the name and neither is one. Hiding them would gut
        // the output people run --dry-run to read.
        assert!(!is_secret_env("CLAUDE_CODE_MAX_CONTEXT_TOKENS"));
        assert!(!is_secret_env("CLAUDE_CODE_AUTO_COMPACT_WINDOW"));
        assert!(!is_secret_env("ANTHROPIC_BASE_URL"));
        assert!(!is_secret_env("ANTHROPIC_SMALL_FAST_MODEL"));
    }

    #[test]
    fn a_one_character_slip_is_caught_and_a_real_agent_flag_is_not() {
        for typo in ["--modle", "--mode", "--provder", "--harnes", "--efort"] {
            assert!(looks_like_a_typo(typo), "{typo} should be refused");
        }
        // Everything else is forwarded, which is what makes `fastpick -p "hello"` work.
        for ok in [
            "--print",
            "--verbose",
            "--resume",
            "--permission-mode",
            "-p",
            "--md",
            "--help",
            "--model",
        ] {
            assert!(!looks_like_a_typo(ok), "{ok} should be forwarded");
        }
    }
}
