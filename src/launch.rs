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
use std::ffi::{OsStr, OsString};
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

/// A command under construction.
///
/// Arguments are collected instead of going straight into `Command` because how they must
/// be quoted is not known until the end: on Windows an npm-installed agent is a `.cmd`
/// shim, the process actually spawned is `cmd.exe`, and it parses the command line before
/// the shim ever sees it.
struct Builder {
    cmd: Command,
    args: Vec<OsString>,
    /// Only ever read on Windows, where it decides between `cmd.exe` quoting and the C
    /// runtime rules. Kept unconditionally so `finish` has one shape on every platform.
    #[cfg_attr(not(windows), allow(dead_code))]
    via_shell: bool,
}

impl Builder {
    fn new(bin: &str) -> Self {
        let p = crate::paths::program(bin);
        Builder {
            cmd: p.cmd,
            args: Vec::new(),
            via_shell: p.via_shell,
        }
    }

    fn arg(&mut self, a: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(a.as_ref().to_os_string());
        self
    }

    fn env(&mut self, k: impl AsRef<OsStr>, v: impl AsRef<OsStr>) -> &mut Self {
        self.cmd.env(k, v);
        self
    }

    fn env_remove(&mut self, k: impl AsRef<OsStr>) -> &mut Self {
        self.cmd.env_remove(k);
        self
    }

    fn finish(mut self) -> Command {
        #[cfg(windows)]
        if self.via_shell {
            use std::os::windows::process::CommandExt;
            for a in &self.args {
                // `raw_arg` appends verbatim, which is the point: the quoting below is ours
                // and Rust's would be applied on top of it.
                self.cmd.raw_arg(quote_for_cmd(a));
            }
            return self.cmd;
        }
        for a in &self.args {
            self.cmd.arg(a);
        }
        self.cmd
    }
}

/// Quotes one argument for a command line `cmd.exe` parses before the program does.
///
/// Rust quotes only what it must, which is an argument holding a space or a quote, so
/// `--model x&calc` arrives bare and the `&` starts a second command. Wrapping every
/// argument in double quotes is what makes `&`, `|`, `<`, `>`, `(`, `)` and `^` inert:
/// inside a quoted run `cmd.exe` acts on none of them.
///
/// An inner `"` is doubled rather than backslash-escaped. `cmd.exe` counts quotes and has
/// no notion of `\"`, so the backslash form leaves it outside the quoted run for the rest
/// of the line, which is the whole exploit. `""` keeps the count even, and the C runtime
/// splitting the line inside the target program reads it back as one literal quote.
///
/// `%VAR%` is still expanded here and has no escape that works outside a batch file. It
/// stays possible and stays harmless: the only values reaching this point come from the
/// user's own config and command line, and the only environment to expand from is their
/// own. Catalogue ids never get here: `config::model_id_is_plain` refuses anything that is
/// not a plain identifier long before a model can be selected.
#[cfg(windows)]
fn quote_for_cmd(arg: &OsStr) -> OsString {
    let s = arg.to_string_lossy();
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in s.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                backslashes = 0;
                out.push_str("\"\"");
            }
            c => {
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // A run of backslashes touching the closing quote would escape it for the C runtime.
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    OsString::from(out)
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
    // The proxy runs for the whole session behind the agent's full-screen interface, so
    // inherited stdio would scribble its log over the agent's own drawing, and request
    // bodies with it. `logs_hint` is how the user is meant to read it instead.
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    detach(&mut cmd);
    let child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", exe.display()))?;
    // Deliberately leaked. Dropping the handle would leave a zombie on Unix for as long as
    // fastpick lives, and the proxy is meant to outlive this launch rather than be waited
    // on: `detach` has already put it in its own session.
    std::mem::forget(child);

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

#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // A new session, so the proxy leaves this process group: otherwise it shares the
    // terminal's signals and the Ctrl-C that ends the agent takes the proxy with it,
    // which is the opposite of outliving the launch.
    unsafe {
        cmd.pre_exec(|| {
            libc_setsid();
            Ok(())
        });
    }
}

#[cfg(unix)]
fn libc_setsid() {
    // Declared here rather than pulling in a crate for one call.
    extern "C" {
        fn setsid() -> i32;
    }
    unsafe {
        setsid();
    }
}

#[cfg(not(any(windows, unix)))]
fn detach(_cmd: &mut Command) {}

/// One ICMP echo. A box that is powered off is the normal case for a local provider, so
/// this decides between a warning and a refusal rather than being a health check.
///
/// Not `status().success()`. On Windows `ping` exits 0 when the local router answers
/// "Destination host unreachable" on behalf of a machine that is off, which is exactly the
/// case this exists to catch, so the reply itself has to be read. Both platforms print a
/// TTL only for an answer that came from the host.
fn host_is_up(host: &str) -> bool {
    let mut cmd = Command::new("ping");
    #[cfg(windows)]
    cmd.args(["-n", "1", "-w", "1000", host]);
    #[cfg(not(windows))]
    cmd.args(["-c", "1", "-W", "1", host]);
    let Ok(out) = cmd.stderr(std::process::Stdio::null()).output() else {
        // No `ping` on this system at all, which is normal in a minimal container. Not
        // knowing is not the same as knowing the host is down, so the check passes and
        // the launch is left to fail on its own terms if the endpoint really is gone.
        return true;
    };
    if !out.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).to_lowercase();
    stdout.contains("ttl=") || stdout.contains("ttl ")
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
    if check.on_down == crate::config::OnDown::Abort {
        return Err(anyhow!(msg));
    }
    println!("{msg}");
    let _ = std::io::stdout().flush();
    Ok(())
}

/// Builds the command without running it. Split out so `--dry-run` shows exactly what a
/// real launch would do, environment included.
pub fn build(cfg: &Config, sel: &Selection) -> Result<Command> {
    let mut cmd = Builder::new(&sel.harness.bin);
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
    Ok(cmd.finish())
}

/// Claude Code: the endpoint is environment, the model and the prompts are flags.
///
/// Every variable this function owns is either set or removed, never left to whatever the
/// parent shell exported. A value that is merely *not set* here is still inherited by the
/// child, and for credentials that is how a first-party key ends up at a third-party
/// endpoint.
fn claude_code(cmd: &mut Builder, sel: &Selection, token: Option<&str>) {
    match &sel.binding.base_url {
        // An endpoint that is not Anthropic's. Both inherited credentials go, whether or
        // not this provider brings one of its own: a binding with a `base_url` and no key
        // file is a local or unauthenticated endpoint, and an ANTHROPIC_API_KEY exported
        // in the user's shell would otherwise be sent to it verbatim.
        Some(url) => {
            cmd.env("ANTHROPIC_BASE_URL", url);
            cmd.env_remove("ANTHROPIC_API_KEY");
            cmd.env_remove("ANTHROPIC_AUTH_TOKEN");
        }
        // The agent's own login, so its own credentials are the point and stay. Only the
        // endpoint is cleared: an ANTHROPIC_BASE_URL left over in the environment would
        // point Claude Code at someone else's server while it authenticates as itself.
        None => {
            cmd.env_remove("ANTHROPIC_BASE_URL");
        }
    }

    if let Some(t) = token {
        cmd.env("ANTHROPIC_AUTH_TOKEN", t);
        // An ANTHROPIC_API_KEY outranks the token above and would be sent instead of it.
        // Removed rather than emptied: several clients treat a present-but-empty key as a
        // key and answer 401 instead of falling through to the token.
        cmd.env_remove("ANTHROPIC_API_KEY");
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

    match sel
        .model
        .small_fast_model
        .as_ref()
        .or(sel.provider.small_fast_model.as_ref())
    {
        Some(sfm) => {
            cmd.env("ANTHROPIC_SMALL_FAST_MODEL", sfm);
        }
        // Nothing declared and an endpoint of our own: an inherited value names a model
        // this provider does not serve, and every background call 404s. Harmless to keep
        // only when the agent is talking to its own endpoint, which is the other arm.
        None if sel.binding.base_url.is_some() => {
            cmd.env_remove("ANTHROPIC_SMALL_FAST_MODEL");
        }
        None => {}
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
fn opencode(cmd: &mut Builder, sel: &Selection, token: Option<&str>) -> Result<()> {
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

    // Both variables belong to fastpick, so both are always decided here rather than
    // inherited: an OPENCODE_CONFIG_CONTENT left in the environment by an earlier launch
    // or by hand would silently apply its provider block to this one.
    match token {
        Some(t) => cmd.env(KEY_ENV, t),
        None => cmd.env_remove(KEY_ENV),
    };
    if cfg.is_empty() {
        cmd.env_remove("OPENCODE_CONFIG_CONTENT");
    } else {
        cmd.env(
            "OPENCODE_CONFIG_CONTENT",
            serde_json::to_string(&serde_json::Value::Object(cfg))?,
        );
    }

    // A provider with no base_url is one of OpenCode's own, already named by its id.
    cmd.arg("--model").arg(format!("{pid}/{}", sel.model.id));
    Ok(())
}

/// A TOML basic string, quotes included, with everything the grammar reserves escaped.
///
/// Without this a `name` or a `base_url` holding a `"` closes the string early and the
/// rest of the value is parsed as more TOML, which is how one config field becomes two
/// overrides.
fn toml_string(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 2);
    out.push('"');
    for c in v.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Codex: dotted TOML overrides, so `~/.codex/config.toml` is never touched.
fn codex(cmd: &mut Builder, sel: &Selection, token: Option<&str>) {
    cmd.arg("--model").arg(&sel.model.id);

    let Some(url) = &sel.binding.base_url else {
        // No endpoint override: Codex uses whatever it is already logged into. The key
        // variable is still ours to clear, so a leftover cannot follow into that session.
        if token.is_none() {
            cmd.env_remove(KEY_ENV);
        }
        return;
    };

    let pid = "fastpick";
    // The value of a `-c` override is parsed as TOML, so strings are quoted explicitly
    // rather than relying on the raw-string fallback.
    let mut set = |k: &str, v: &str| {
        cmd.arg("-c").arg(format!("{k}={}", toml_string(v)));
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
        sel.binding
            .wire_api
            .map(|w| w.as_str())
            .unwrap_or("responses"),
    );
    if let Some(t) = token {
        set(&format!("model_providers.{pid}.env_key"), KEY_ENV);
        cmd.env(KEY_ENV, t);
    } else {
        cmd.env_remove(KEY_ENV);
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
    if let Some(code) = status.code() {
        return Ok(code);
    }
    // Killed by a signal, so there is no exit code to pass on. The shell convention is
    // 128 + signal, which keeps "interrupted" distinguishable from a plain failure.
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return Ok(128 + sig);
        }
    }
    Ok(1)
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

            # An endpoint of its own and no key file: a local runtime, or a relay that
            # injects its own credentials. The case where an inherited key must not travel.
            [[provider]]
            id = "keyless"
            name = "Keyless"

            [provider.harness.claude-code]
            base_url = "https://keyless.invalid"

            [[provider.model]]
            id = "acme-large"
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

    /// Whether the child is told to *unset* this variable, as opposed to never being told
    /// anything about it. `env_of` cannot tell those apart, and the difference is the
    /// whole point: a variable merely left alone is inherited from the parent shell.
    fn env_removed(cmd: &Command, key: &str) -> bool {
        cmd.get_envs()
            .any(|(k, v)| k.to_string_lossy() == key && v.is_none())
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
        assert_eq!(env_of(&cmd, "ANTHROPIC_AUTH_TOKEN"), None);
        assert_eq!(env_of(&cmd, "ANTHROPIC_API_KEY"), None);
        assert!(
            !env_removed(&cmd, "ANTHROPIC_API_KEY"),
            "this is the agent's own login, its own key is the point"
        );
        // The endpoint is the exception: an ANTHROPIC_BASE_URL exported in the shell would
        // send those first-party credentials to whatever it names.
        assert!(env_removed(&cmd, "ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn an_endpoint_of_our_own_never_inherits_the_first_party_key() {
        // The provider declares a base_url and no key file at all, which is what a local
        // llama.cpp or an unauthenticated relay looks like. An ANTHROPIC_API_KEY sitting in
        // the user's shell would otherwise be handed straight to it.
        let (_d, cfg) = fixture();
        let m = Model::new("acme-large".into());
        let sel = selection(&cfg, "claude-code", "keyless", &m);
        let cmd = build(&cfg, &sel).unwrap();
        assert_eq!(
            env_of(&cmd, "ANTHROPIC_BASE_URL").as_deref(),
            Some("https://keyless.invalid")
        );
        assert!(env_removed(&cmd, "ANTHROPIC_API_KEY"));
        assert!(env_removed(&cmd, "ANTHROPIC_AUTH_TOKEN"));
    }

    #[test]
    fn an_undeclared_small_fast_model_is_cleared_for_a_third_party_endpoint() {
        // Inherited, it names a model the endpoint does not serve, and every background
        // call 404s with an error that points nowhere near the cause.
        let (_d, cfg) = fixture();
        let m = Model::new("acme-large".into());
        let sel = selection(&cfg, "claude-code", "keyless", &m);
        let cmd = build(&cfg, &sel).unwrap();
        assert!(env_removed(&cmd, "ANTHROPIC_SMALL_FAST_MODEL"));
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
        assert!(
            env_removed(&cmd, "ANTHROPIC_API_KEY"),
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
        sel.prompts = vec![PathBuf::from("/prompts/acme.md")];
        let cmd = build(&cfg, &sel).unwrap();

        let json = env_of(&cmd, "OPENCODE_CONFIG_CONTENT").unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            v.pointer("/instructions/0").and_then(|k| k.as_str()),
            Some("/prompts/acme.md")
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
                let (_, after) = v.split_once('=').unwrap();
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

#[cfg(test)]
mod quoting_tests {
    use super::*;

    #[test]
    fn a_toml_override_cannot_be_closed_early_by_its_own_value() {
        // A `"` in a provider name used to end the string and turn the rest into a second
        // dotted override.
        assert_eq!(toml_string(r#"a"b"#), r#""a\"b""#);
        assert_eq!(toml_string(r"a\b"), r#""a\\b""#);
        assert_eq!(toml_string("a\nb"), r#""a\nb""#);
        assert_eq!(toml_string("plain"), r#""plain""#);

        // And the result parses back to exactly what went in.
        let round: toml::Value = toml::from_str(&format!("v = {}", toml_string(r#"x", y = "z"#)))
            .expect("must stay one value");
        assert_eq!(round["v"].as_str(), Some(r#"x", y = "z"#));
    }

    #[cfg(windows)]
    #[test]
    fn every_argument_is_quoted_so_cmd_never_sees_syntax() {
        // Rust quotes only what holds a space, so `x&calc` would otherwise arrive bare and
        // `cmd.exe` would read the `&` as a command separator.
        let q = |s: &str| quote_for_cmd(OsStr::new(s)).to_string_lossy().to_string();
        assert_eq!(q("x&calc"), r#""x&calc""#);
        assert_eq!(q("plain"), r#""plain""#);
        assert_eq!(q("a > b"), r#""a > b""#);
        // Doubled, not backslash-escaped: cmd.exe counts quotes and has no notion of \".
        assert_eq!(q(r#"a"b"#), r#""a""b""#);
        // A backslash run touching the closing quote would escape it for the C runtime.
        assert_eq!(q(r"C:\path\"), r#""C:\path\\""#);
    }
}
