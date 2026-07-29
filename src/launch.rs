//! Turning a selection into a running agent process.
//!
//! One adapter per harness kind, because the three agents disagree about everything that
//! matters here: Claude Code takes an endpoint through environment variables, OpenCode
//! takes a whole provider block as JSON, and Codex takes dotted TOML overrides on the
//! command line. What they have in common is only the prechecks and the token file.
//!
//! Everything happens after the terminal has been restored, so warnings and the session
//! that follows both start on a clean screen.

use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::config::{Binding, Config, Harness, HarnessKind, Model, Provider};
use crate::paths::expand;

/// The environment variable a generated provider block points at for its key, so the
/// secret is passed through the process environment and never written into a config file.
pub const KEY_ENV: &str = "FASTPICK_PROVIDER_KEY";

pub struct Selection<'a> {
    pub harness: &'a Harness,
    pub provider: &'a Provider,
    pub binding: &'a Binding,
    pub model: &'a Model,
    pub effort: Option<String>,
    pub prompts: Vec<PathBuf>,
    pub passthrough: Vec<String>,
}

/// Reads a single-line token file, trimming the trailing newline a text editor adds.
fn read_token(path: &PathBuf) -> Result<String> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading the key file {}", path.display()))?;
    let token = raw.trim().to_string();
    if token.is_empty() {
        return Err(anyhow!("the key file {} is empty", path.display()));
    }
    Ok(token)
}

fn token_of(p: &Provider) -> Result<Option<String>> {
    let Some(keyfile) = &p.auth_token_file else {
        return Ok(None);
    };
    let path = expand(keyfile);
    if !path.exists() {
        return Err(anyhow!(
            "{}: missing key file {}. `fastpick --set-key {}` writes it",
            p.name,
            path.display(),
            p.id
        ));
    }
    // Said once, at the moment the file is actually read, and never fatal: refusing to
    // launch over a permission bit would be fastpick deciding something that is the
    // owner's call.
    let access = crate::secrets::access(&path);
    if access.is_problem() {
        eprintln!("fastpick: {} is {}", path.display(), access.label());
    }
    Ok(Some(read_token(&path)?))
}

/// Starts the provider's proxy if nothing is listening yet, then waits for the port.
fn ensure_proxy(p: &Provider) -> Result<()> {
    let Some(proxy) = &p.proxy else { return Ok(()) };
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, proxy.port));

    if TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok() {
        return Ok(());
    }

    let exe = expand(&proxy.exe);
    if !exe.exists() {
        return Err(anyhow!(
            "{} needs a proxy on port {} and its binary is missing: {}",
            p.name,
            proxy.port,
            exe.display()
        ));
    }

    println!(
        "Starting the proxy for {} on 127.0.0.1:{} ...",
        p.name, proxy.port
    );
    let mut cmd = Command::new(&exe);
    for a in &proxy.args {
        cmd.arg(expand(a));
    }
    if let Some(dir) = &proxy.workdir {
        cmd.current_dir(expand(dir));
    }
    detach(&mut cmd);
    cmd.spawn()
        .with_context(|| format!("spawning {}", exe.display()))?;

    let deadline = Instant::now() + Duration::from_secs(proxy.timeout_secs);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(300));
    }

    let hint = proxy
        .logs_hint
        .as_deref()
        .map(|h| format!(" Check {h} for details."))
        .unwrap_or_default();
    Err(anyhow!(
        "the proxy did not accept connections on port {} within {}s.{}",
        proxy.port,
        proxy.timeout_secs,
        hint
    ))
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: the proxy outlives this launch and does
    // not take a console window with it.
    cmd.creation_flags(0x0000_0008 | 0x0000_0200);
}

#[cfg(not(windows))]
fn detach(_cmd: &mut Command) {}

/// One ICMP echo. A box that is powered off is the normal case for a local provider, so
/// this decides between a warning and a refusal rather than being a health check.
fn host_is_up(host: &str) -> bool {
    let mut cmd = Command::new("ping");
    #[cfg(windows)]
    cmd.args(["-n", "1", "-w", "1000", host]);
    #[cfg(not(windows))]
    cmd.args(["-c", "1", "-W", "1", host]);
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_host_check(p: &Provider) -> Result<()> {
    let Some(check) = &p.host_check else {
        return Ok(());
    };
    if host_is_up(&check.host) {
        return Ok(());
    }
    let msg = check
        .message
        .clone()
        .unwrap_or_else(|| format!("{} does not answer.", check.host));
    if check.on_down == "abort" {
        return Err(anyhow!(msg));
    }
    println!("{msg}");
    let _ = std::io::stdout().flush();
    Ok(())
}

/// Builds the command without running it. Split out so `--dry-run` shows exactly what a
/// real launch would do, environment included.
pub fn build(cfg: &Config, sel: &Selection) -> Result<Command> {
    let mut cmd = crate::paths::program(&sel.harness.bin);
    let token = token_of(sel.provider)?;

    match sel.harness.kind {
        HarnessKind::ClaudeCode => claude_code(&mut cmd, sel, token.as_deref()),
        HarnessKind::Opencode => opencode(&mut cmd, sel, token.as_deref())?,
        HarnessKind::Codex => codex(&mut cmd, sel, token.as_deref()),
    }

    // Provider-wide first, then the binding's, so a harness-specific value wins.
    for (k, v) in &sel.provider.env {
        cmd.env(k, v);
    }
    for (k, v) in &sel.binding.env {
        cmd.env(k, v);
    }

    for a in &sel.harness.extra_args {
        cmd.arg(a);
    }
    for a in &sel.binding.extra_args {
        cmd.arg(a);
    }
    for a in &sel.passthrough {
        cmd.arg(a);
    }

    let _ = cfg;
    Ok(cmd)
}

/// Claude Code: the endpoint is environment, the model and the prompts are flags.
fn claude_code(cmd: &mut Command, sel: &Selection, token: Option<&str>) {
    if let Some(url) = &sel.binding.base_url {
        cmd.env("ANTHROPIC_BASE_URL", url);
    }
    if let Some(t) = token {
        cmd.env("ANTHROPIC_AUTH_TOKEN", t);
        // An inherited ANTHROPIC_API_KEY outranks the token above and would be sent
        // upstream instead of it.
        cmd.env("ANTHROPIC_API_KEY", "");
    }

    // Only declared for models Claude Code does not know: the resolver ignores this
    // variable for any name starting with `claude-`, where the window comes from the
    // built-in table and the id is the only lever on it.
    if let Some(ctx) = sel.model.context_window {
        if !sel.model.id.starts_with("claude-") {
            cmd.env("CLAUDE_CODE_MAX_CONTEXT_TOKENS", ctx.to_string());
            let ratio = sel.model.compact_ratio.unwrap_or(0.9).clamp(0.5, 1.0);
            cmd.env(
                "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
                ((ctx as f64 * ratio) as u64).to_string(),
            );
        }
    }

    if let Some(sfm) = sel
        .model
        .small_fast_model
        .as_ref()
        .or(sel.provider.small_fast_model.as_ref())
    {
        cmd.env("ANTHROPIC_SMALL_FAST_MODEL", sfm);
    }

    cmd.arg("--model").arg(&sel.model.id);
    if let Some(effort) = &sel.effort {
        cmd.arg("--effort").arg(effort);
    }
    for prompt in &sel.prompts {
        cmd.arg("--append-system-prompt-file").arg(prompt);
    }
}

/// OpenCode: everything travels as one inline JSON config.
///
/// `OPENCODE_CONFIG_CONTENT` is merged over the user's own config rather than replacing
/// it, and sits near the top of the precedence list, so their MCP servers, plugins and
/// agents survive untouched. The key is referenced as `{env:...}` so it stays in the
/// process environment instead of being written anywhere.
fn opencode(cmd: &mut Command, sel: &Selection, token: Option<&str>) -> Result<()> {
    let pid = &sel.provider.id;
    let mut cfg = serde_json::Map::new();

    if let Some(url) = &sel.binding.base_url {
        let npm = sel.binding.npm.as_deref().ok_or_else(|| {
            anyhow!(
                "provider `{pid}` gives OpenCode a base_url but no `npm`, so OpenCode has no dialect to speak it with. Use `@ai-sdk/anthropic`, `@ai-sdk/openai-compatible` or `@ai-sdk/openai`."
            )
        })?;

        let mut options = serde_json::Map::new();
        options.insert("baseURL".into(), url.clone().into());
        if token.is_some() {
            options.insert("apiKey".into(), format!("{{env:{KEY_ENV}}}").into());
        }

        let mut models = serde_json::Map::new();
        // Declaring the chosen model is what makes it selectable. No `limit` block: the
        // schema wants both the context and the output cap, and inventing an output cap
        // would silently truncate answers.
        models.insert(sel.model.id.clone(), serde_json::json!({}));

        let mut provider = serde_json::Map::new();
        provider.insert("npm".into(), npm.into());
        provider.insert("name".into(), sel.provider.name.clone().into());
        provider.insert("options".into(), options.into());
        provider.insert("models".into(), models.into());

        cfg.insert(
            "provider".into(),
            serde_json::json!({ pid.clone(): provider }),
        );
    }

    if !sel.prompts.is_empty() {
        // `instructions` appends: each file arrives as its own system block on top of the
        // base prompt and the AGENTS.md chain, it never replaces them.
        let paths: Vec<serde_json::Value> = sel
            .prompts
            .iter()
            .map(|p| p.display().to_string().replace('\\', "/").into())
            .collect();
        cfg.insert("instructions".into(), paths.into());
    }

    if let Some(sfm) = sel
        .model
        .small_fast_model
        .as_ref()
        .or(sel.provider.small_fast_model.as_ref())
    {
        cfg.insert("small_model".into(), format!("{pid}/{sfm}").into());
    }

    if let Some(t) = token {
        cmd.env(KEY_ENV, t);
    }
    if !cfg.is_empty() {
        cmd.env(
            "OPENCODE_CONFIG_CONTENT",
            serde_json::to_string(&serde_json::Value::Object(cfg))?,
        );
    }

    // A provider with no base_url is one of OpenCode's own, already named by its id.
    cmd.arg("--model").arg(format!("{pid}/{}", sel.model.id));
    Ok(())
}

/// Codex: dotted TOML overrides, so `~/.codex/config.toml` is never touched.
fn codex(cmd: &mut Command, sel: &Selection, token: Option<&str>) {
    cmd.arg("--model").arg(&sel.model.id);

    let Some(url) = &sel.binding.base_url else {
        // No endpoint override: Codex uses whatever it is already logged into.
        return;
    };

    let pid = "fastpick";
    // The value of a `-c` override is parsed as TOML, so strings are quoted explicitly
    // rather than relying on the raw-string fallback.
    let mut set = |k: &str, v: &str| {
        cmd.arg("-c").arg(format!("{k}=\"{v}\""));
    };
    set("model_provider", pid);
    set(&format!("model_providers.{pid}.name"), &sel.provider.name);
    set(&format!("model_providers.{pid}.base_url"), url);
    // `responses` is the default because Codex dropped support for the other one: a
    // provider config saying `wire_api = "chat"` is now refused outright with
    // "`wire_api = \"chat\"` is no longer supported". An endpoint that only speaks
    // /v1/chat/completions cannot be driven by Codex at all, whatever is written here.
    set(
        &format!("model_providers.{pid}.wire_api"),
        sel.binding.wire_api.as_deref().unwrap_or("responses"),
    );
    if let Some(t) = token {
        set(&format!("model_providers.{pid}.env_key"), KEY_ENV);
        cmd.env(KEY_ENV, t);
    }
}

/// Runs the prechecks, then hands the terminal to the agent and returns its exit code.
pub fn run(cfg: &Config, sel: &Selection) -> Result<i32> {
    run_host_check(sel.provider)?;
    ensure_proxy(sel.provider)?;

    let mut cmd = build(cfg, sel)?;
    let status = cmd
        .status()
        .with_context(|| format!("launching {}", sel.harness.bin))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    use crate::prompts::tempdir::TempDir;

    /// A config with one provider reachable from all three harnesses, and a real key file
    /// so the launch path is exercised end to end rather than bailing on a missing token.
    fn fixture() -> (TempDir, Config) {
        let dir = TempDir::new();
        let key = dir.path().join("client.key");
        std::fs::write(&key, "sk-test-token\n").unwrap();
        let key = key.display().to_string().replace('\\', "/");

        let toml = format!(
            r#"
            [[harness]]
            id = "claude-code"
            name = "Claude Code"
            kind = "claude-code"
            bin = "fastpick-test-claude"

            [[harness]]
            id = "opencode"
            name = "OpenCode"
            kind = "opencode"
            bin = "fastpick-test-opencode"

            [[harness]]
            id = "codex"
            name = "Codex"
            kind = "codex"
            bin = "fastpick-test-codex"

            [[provider]]
            id = "acme"
            name = "Acme"
            auth_token_file = "{key}"
            small_fast_model = "acme-mini"

            [provider.harness.claude-code]
            base_url = "https://acme.invalid"

            [provider.harness.opencode]
            base_url = "https://acme.invalid/v1"
            npm = "@ai-sdk/openai-compatible"

            [provider.harness.codex]
            base_url = "https://acme.invalid/v1"
            wire_api = "chat"

            [[provider.model]]
            id = "acme-mini"

            [[provider]]
            id = "builtin"
            name = "Built in"

            [provider.harness.claude-code]

            [[provider.model]]
            id = "claude-opus-5"
            "#
        );
        let cfg: Config = toml::from_str(&toml).unwrap();
        (dir, cfg)
    }

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    fn env_of(cmd: &Command, key: &str) -> Option<String> {
        cmd.get_envs()
            .find(|(k, _)| k.to_string_lossy() == key)
            .and_then(|(_, v)| v.map(|v| v.to_string_lossy().to_string()))
    }

    fn selection<'a>(
        cfg: &'a Config,
        harness_id: &str,
        provider_id: &str,
        model: &'a Model,
    ) -> Selection<'a> {
        let harness = cfg.harnesses.iter().find(|h| h.id == harness_id).unwrap();
        let provider = cfg.providers.iter().find(|p| p.id == provider_id).unwrap();
        Selection {
            harness,
            provider,
            binding: provider.binding(harness_id).unwrap(),
            model,
            effort: None,
            prompts: Vec::new(),
            passthrough: Vec::new(),
        }
    }

    #[test]
    fn claude_code_never_declares_a_window_for_a_claude_model() {
        let (_d, cfg) = fixture();
        let mut m = Model::new("claude-opus-5".into());
        m.context_window = Some(1_000_000);
        let sel = selection(&cfg, "claude-code", "builtin", &m);
        let cmd = build(&cfg, &sel).unwrap();
        assert_eq!(
            env_of(&cmd, "CLAUDE_CODE_MAX_CONTEXT_TOKENS"),
            None,
            "the resolver ignores it for claude-* names, setting it would be a lie"
        );
    }

    #[test]
    fn a_builtin_provider_leaves_the_credentials_alone() {
        let (_d, cfg) = fixture();
        let m = Model::new("claude-opus-5".into());
        let sel = selection(&cfg, "claude-code", "builtin", &m);
        let cmd = build(&cfg, &sel).unwrap();
        assert_eq!(env_of(&cmd, "ANTHROPIC_BASE_URL"), None);
        assert_eq!(env_of(&cmd, "ANTHROPIC_AUTH_TOKEN"), None);
        assert_eq!(env_of(&cmd, "ANTHROPIC_API_KEY"), None);
    }

    #[test]
    fn claude_code_declares_a_window_for_anything_else() {
        let (_d, cfg) = fixture();
        let mut m = Model::new("acme-large".into());
        m.context_window = Some(500_000);
        let sel = selection(&cfg, "claude-code", "acme", &m);
        let cmd = build(&cfg, &sel).unwrap();
        assert_eq!(
            env_of(&cmd, "CLAUDE_CODE_MAX_CONTEXT_TOKENS").as_deref(),
            Some("500000")
        );
        assert_eq!(
            env_of(&cmd, "CLAUDE_CODE_AUTO_COMPACT_WINDOW").as_deref(),
            Some("450000")
        );
        assert_eq!(
            env_of(&cmd, "ANTHROPIC_AUTH_TOKEN").as_deref(),
            Some("sk-test-token")
        );
        assert_eq!(
            env_of(&cmd, "ANTHROPIC_API_KEY").as_deref(),
            Some(""),
            "an inherited key would outrank the token and get sent upstream"
        );
    }

    #[test]
    fn opencode_gets_a_provider_block_and_never_the_raw_key_in_it() {
        let (_d, cfg) = fixture();
        let m = Model::new("acme-large".into());
        let sel = selection(&cfg, "opencode", "acme", &m);
        let cmd = build(&cfg, &sel).unwrap();

        let args = args_of(&cmd);
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--model" && w[1] == "acme/acme-large"));

        let json = env_of(&cmd, "OPENCODE_CONFIG_CONTENT").expect("a provider block is needed");
        assert!(
            !json.contains("sk-test-token"),
            "the key must be referenced through the environment, not written into the config"
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            v.pointer("/provider/acme/options/apiKey")
                .and_then(|k| k.as_str()),
            Some(format!("{{env:{KEY_ENV}}}").as_str())
        );
        assert_eq!(
            v.pointer("/provider/acme/options/baseURL")
                .and_then(|k| k.as_str()),
            Some("https://acme.invalid/v1")
        );
        assert!(v.pointer("/provider/acme/models/acme-large").is_some());
        assert_eq!(
            v.pointer("/small_model").and_then(|k| k.as_str()),
            Some("acme/acme-mini")
        );
        assert_eq!(env_of(&cmd, KEY_ENV).as_deref(), Some("sk-test-token"));
    }

    #[test]
    fn opencode_puts_prompt_files_in_instructions_which_appends() {
        let (_d, cfg) = fixture();
        let m = Model::new("acme-large".into());
        let mut sel = selection(&cfg, "opencode", "acme", &m);
        sel.prompts = vec![PathBuf::from("D:/prompts/acme.md")];
        let cmd = build(&cfg, &sel).unwrap();

        let json = env_of(&cmd, "OPENCODE_CONFIG_CONTENT").unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            v.pointer("/instructions/0").and_then(|k| k.as_str()),
            Some("D:/prompts/acme.md")
        );
    }

    #[test]
    fn opencode_refuses_a_base_url_it_has_no_dialect_for() {
        let (_d, cfg) = fixture();
        // Same fixture with the npm field dropped: OpenCode cannot guess how to speak to
        // an endpoint, so this has to fail loudly rather than launch something broken.
        let toml = r#"
            [[harness]]
            id = "opencode"
            name = "OpenCode"
            kind = "opencode"
            bin = "fastpick-test-opencode"

            [[provider]]
            id = "acme"
            name = "Acme"

            [provider.harness.opencode]
            base_url = "https://acme.invalid/v1"

            [[provider.model]]
            id = "m"
        "#;
        let broken: Config = toml::from_str(toml).unwrap();
        let m = Model::new("m".into());
        let sel = selection(&broken, "opencode", "acme", &m);
        let err = build(&cfg, &sel).unwrap_err().to_string();
        assert!(err.contains("npm"), "{err}");
    }

    #[test]
    fn codex_overrides_are_quoted_toml_and_carry_the_key_by_reference() {
        let (_d, cfg) = fixture();
        let m = Model::new("acme-large".into());
        let sel = selection(&cfg, "codex", "acme", &m);
        let cmd = build(&cfg, &sel).unwrap();
        let args = args_of(&cmd);

        for (i, a) in args.iter().enumerate() {
            if a == "-c" {
                let v = &args[i + 1];
                let after = v.split_once('=').map(|(_, a)| a).unwrap();
                assert!(
                    after.starts_with('"') && after.ends_with('"'),
                    "codex parses the value as TOML, so it must be quoted: {v}"
                );
                assert!(
                    !v.contains("sk-test-token"),
                    "the key goes through the environment, not the command line: {v}"
                );
            }
        }
        assert!(args.contains(&format!("model_providers.fastpick.env_key=\"{KEY_ENV}\"")));
        assert_eq!(env_of(&cmd, KEY_ENV).as_deref(), Some("sk-test-token"));
    }

    #[test]
    fn codex_on_a_builtin_provider_overrides_nothing() {
        let (_d, cfg) = fixture();
        let toml = r#"
            [[harness]]
            id = "codex"
            name = "Codex"
            kind = "codex"
            bin = "fastpick-test-codex"

            [[provider]]
            id = "builtin"
            name = "Built in"

            [provider.harness.codex]

            [[provider.model]]
            id = "gpt-5.6"
        "#;
        let native: Config = toml::from_str(toml).unwrap();
        let m = Model::new("gpt-5.6".into());
        let sel = selection(&native, "codex", "builtin", &m);
        let cmd = build(&cfg, &sel).unwrap();
        let args = args_of(&cmd);
        assert_eq!(args, vec!["--model", "gpt-5.6"]);
    }
}
