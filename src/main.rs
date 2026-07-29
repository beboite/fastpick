//! fastpick: pick a coding harness, a provider and a model, then launch that agent with the
//! environment the combination needs.
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
mod state;
mod tui;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::ExitCode;

const HELP: &str = "\
fastpick - pick a coding agent, a provider and a model, then launch it

USAGE
  fastpick [options] [-- <args passed to the agent>]

OPTIONS
  -c, --config <file>    config file to use (default: the per-user one, see PATHS)
  -l, --list             print the harnesses, providers and models, then exit
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

KEYS
  up/down   move             enter   select / launch
  esc       back             space   check a system prompt file
  tab       refetch the model catalogue for this provider
  a         list every file in the prompts folder, not only the ones matching the model
  type      filter the model list

PATHS
  config    %APPDATA%\\fastpick\\config.toml, or ~/.config/fastpick/config.toml
  catalog   <config dir>/catalog/, one JSON per provider, refreshed on a timer
  prompts   whatever `system_prompts_dir` points at, `<config dir>/system-prompts` by default
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
            "--" => {
                args.passthrough.extend(it);
                break;
            }
            _ => args.passthrough.push(a),
        }
    }
    Ok(Some(args))
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

    let cfg = config::Config::load(&cfg_path)?;

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
        Some(id) => Some(
            cfg.harnesses
                .iter()
                .position(|h| &h.id == id)
                .with_context(|| format!("no harness with id `{id}`, see --list"))?,
        ),
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
                if cfg.providers[idx].binding(&harness.id).is_none() {
                    anyhow::bail!(
                        "provider `{id}` declares no binding for harness `{}`, so that pair cannot be launched",
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
    let binding = provider
        .binding(&harness.id)
        .context("the chosen provider has no binding for the chosen harness")?;
    state::save(&harness.id, &provider.id, &picked.model.id);

    let prompts = if args.md.is_empty() && !args.no_md {
        picked.prompts
    } else {
        resolve_prompts(&cfg, &picked.model, &args)?
    };

    let sel = launch::Selection {
        harness,
        provider,
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
        let Some(dir) = dir else { return Ok(Vec::new()) };
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

fn print_list(cfg: &config::Config, args: &Args) -> Result<()> {
    for h in &cfg.harnesses {
        println!("{}  [{}]  {}", h.name, h.id, h.bin);
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
            let binding = p.binding(&h.id);
            let url = binding
                .and_then(|b| b.base_url.as_deref())
                .unwrap_or("(the agent's own endpoint)");
            println!("    {}  [{}]  {}", p.name, p.id, url);

            // Listing every model of every provider would mean one HTTP call per provider
            // per harness, so the catalogue is only queried when a provider is named.
            if args.provider.as_deref() == Some(&p.id) {
                let req = catalog::Request::new(cfg, p, args.refresh);
                let (models, source) = catalog::run(&req);
                println!("      {}", source.label());
                for m in &models {
                    let ctx = m
                        .context_window
                        .map(|c| format!("  {}K", c / 1000))
                        .unwrap_or_default();
                    println!("      {}{}", m.id, ctx);
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

fn print_dry_run(cfg: &config::Config, sel: &launch::Selection) -> Result<()> {
    let cmd = launch::build(cfg, sel)?;
    println!("env:");
    for (k, v) in cmd.get_envs() {
        let key = k.to_string_lossy();
        let val = v.map(|v| v.to_string_lossy().to_string()).unwrap_or_default();
        // The credentials are the one thing in here that must not be echoed. Matched by
        // exact name, not substring: `CLAUDE_CODE_MAX_CONTEXT_TOKENS` also contains
        // `TOKEN` and hiding its value would gut the output people use --dry-run for.
        let shown = if key == "ANTHROPIC_AUTH_TOKEN"
            || key == "ANTHROPIC_API_KEY"
            || key == launch::KEY_ENV
        {
            if val.is_empty() {
                "(cleared)".to_string()
            } else {
                format!("({} chars, hidden)", val.len())
            }
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
    if let Some(proxy) = &sel.provider.proxy {
        println!("precheck: proxy on 127.0.0.1:{}", proxy.port);
    }
    if let Some(check) = &sel.provider.host_check {
        println!("precheck: ping {} ({})", check.host, check.on_down);
    }
    Ok(())
}
