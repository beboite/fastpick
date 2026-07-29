<h1 align="center">fastpick</h1>
<p align="center">Terminal picker that runs before your coding agent: harness, provider, model, system prompts, launch.</p>

<p align="center">
  <a href="https://github.com/beboite/fastpick/releases"><img src="https://img.shields.io/github/v/release/beboite/fastpick?display_name=tag" alt="Release" /></a>
  <a href="./LICENSE"><img src="https://img.shields.io/github/license/beboite/fastpick" alt="License" /></a>
  <a href="https://github.com/beboite/fastpick/stargazers"><img src="https://img.shields.io/github/stars/beboite/fastpick" alt="Stars" /></a>
  <a href="https://github.com/beboite/fastpick/issues"><img src="https://img.shields.io/github/issues/beboite/fastpick" alt="Issues" /></a>
  <img src="https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-0078D6" alt="Platform" />
  <img src="https://img.shields.io/badge/Rust-1.88%2B-CE422B?logo=rust" alt="Rust" />
</p>

Every agent and every endpoint need slightly different setup, and shell wrappers duplicate
that setup once per pair. Here it is one config file, and the model list is not config at
all: it is fetched from the provider.

## Install

Grab the binary for your platform from [Releases](https://github.com/beboite/fastpick/releases),
make it executable and put it on your PATH. It is self-contained.

## Use

```
fastpick                                     # menu
fastpick -p "hello"                          # menu, then those arguments go to the agent
fastpick --harness opencode                  # skip the first screen
fastpick --harness codex --provider acme --model acme-large    # no menu at all
fastpick --list --provider acme              # what that provider serves right now
fastpick --list --json                       # the same, for another program
fastpick --dry-run                           # the exact command and environment
fastpick --update                            # install the newest signed release
```

Each of `--harness`, `--provider` and `--model` skips its own screen; the menu opens on the
first one you left out.

Up and down move, right goes forward, left goes back. On the model list Enter launches
straight away: the matching system prompt file is already checked and the effort is the
model's default, so the usual case is one key. Right opens the options panel beside the
list, where space changes whatever the cursor is on and `a` lists every file in the prompts
folder. `tab` refetches the model list and typing filters it.

## Setting it up

Run `fastpick` once: it writes a starter config and stops, because the providers it ships
are examples rather than endpoints. Editing that file is the whole configuration.

```
fastpick --edit             # opens it in $VISUAL, $EDITOR, or your platform's default
fastpick --set-key acme     # prompts, does not echo, writes owner-only
fastpick --paths            # where everything lives, and who can read each key file
```

[`config.example.toml`](./config.example.toml) is that file, commented block by block:
harnesses, providers, per-harness bindings, model catalogues, local proxies, host checks.
Only the harnesses whose binary is on this machine are offered, so one config can follow you
across machines that do not have the same agents installed.

Paths accept `~`, `$VAR` and `%VAR%`. Keys are referenced by file path, never inlined, and
never reach a config file, a command line or a log. Every variable an adapter owns is either
set or removed, so picking a third-party endpoint clears `ANTHROPIC_API_KEY` instead of
sending it there.

## Harnesses

| Harness | Endpoint goes in as | Model | Extra instructions |
|---|---|---|---|
| Claude Code | environment variables | `--model` | `--append-system-prompt-file`, appends |
| OpenCode | inline JSON in `OPENCODE_CONFIG_CONTENT` | `--model provider/model` | its `instructions` array, appends |
| Codex | dotted TOML overrides with `-c` | `--model` | **not supported** |

Nothing writes to your agents' own config files, and OpenCode's inline config is merged over
`opencode.json` rather than replacing it. Codex gets no system prompt row because its
instructions override replaces the base prompt, tool rules included: swapping an agent's own
prompt for yours is not something to do quietly.

Two Claude Code behaviours the config works around, both easy to get wrong by hand:
`CLAUDE_CODE_MAX_CONTEXT_TOKENS` does nothing for `claude-*` models, so fastpick refuses to
set it there; and `ANTHROPIC_SMALL_FAST_MODEL` defaults to a model most third-party
endpoints reject, which kills resumes and subagents until `small_fast_model` is set.

## Updating

Every release ships one signed binary per platform, with the signatures collected in a
single `SIGNATURES.json`. `fastpick --update` checks the minisign signature against the key
compiled into the running binary before replacing anything. The
menu checks for a newer version once a day at most, on a background thread, and only ever
prints a line naming the version: nothing installs itself.

## Build

```bash
cargo build --release
cargo test
```

Releasing: bump `Cargo.toml`, then push a `v` tag that agrees with it. The workflow builds
five targets, signs each one, and opens a **draft** release for a human to publish.

## License

MIT.
