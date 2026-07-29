# fastpick

A terminal picker that runs before your coding agent: choose the harness, then a provider,
then one of the models that provider actually serves, then which system prompt files to
append. It launches the agent with the environment that combination needs.

It exists because every agent and every endpoint need slightly different setup, and shell
wrappers duplicate that setup once per pair. Here it is data in one config file, and the
model list is not data at all: it is fetched from the provider.

```
fastpick                                        # menu
fastpick -p "hello"                             # menu, then those arguments go to the agent
fastpick --harness opencode                     # skip the first screen
fastpick --harness codex --provider acme --model acme-large        # no menu at all
fastpick --list --provider acme                 # what that provider serves right now
fastpick --dry-run                              # the exact command and environment
```

Each of `--harness`, `--provider` and `--model` skips its own screen. The menu opens on the
first one you left out.

Keys: arrows move, Enter selects, Esc goes back, Space checks a system prompt file,
left/right changes the effort level, `tab` refetches the model list, `a` lists every file
in the prompts folder, and typing filters the model list.

## Harnesses

| Harness | Endpoint goes in as | Model | Extra instructions |
|---|---|---|---|
| Claude Code | environment variables | `--model` | `--append-system-prompt-file`, appends |
| OpenCode | one inline JSON config in `OPENCODE_CONFIG_CONTENT` | `--model provider/model` | its `instructions` array, appends |
| Codex | dotted TOML overrides with `-c` | `--model` | **not supported**, see below |

Nothing writes to your agents' own config files. OpenCode's inline config is *merged* over
`opencode.json` rather than replacing it, so MCP servers, plugins and agents survive
untouched, and Codex's `-c` overrides never touch `~/.codex/config.toml`.

**Codex gets no system prompt row on purpose.** It has no append-only surface for extra
instructions: its instructions override replaces the base prompt, tool rules included.
Rather than quietly swapping the agent's own prompt for yours, the options screen says the
harness cannot do it. Effort levels are likewise offered only where the harness passes one
through, which today is Claude Code.

## Providers

A provider is an endpoint. It declares one binding per harness it can serve, and shows up
only for those:

```toml
[[provider]]
id = "acme"
name = "Acme"
group = "acme.example"                       # optional heading in the menu
auth_token_file = "~/.acme/client.key"

  [provider.harness.claude-code]
  base_url = "https://acme.example"          # speaks the Anthropic wire format

  [provider.harness.opencode]
  base_url = "https://acme.example/v1"
  npm = "@ai-sdk/openai-compatible"          # which dialect OpenCode should speak

  [provider.harness.codex]
  base_url = "https://acme.example/v1"
  wire_api = "responses"                     # the only value Codex still accepts
```

A binding with no `base_url` means "change nothing", which is how the agent's own login is
declared. Leaving the binding out entirely is different: the provider then does not appear
for that harness at all.

`group` is a heading in the menu, and providers sharing one are drawn as a block in the
order this file lists them. One site reached with two keys, or directly and through a
proxy, is several entries but one place, and the menu should say so. Leave it out and the
provider gets a block of its own, which is what you want when its name is already the
site.

Two more optional blocks, both prechecks that run before the launch:

- `[provider.proxy]` starts a local translator if its port is free and waits for it, for an
  endpoint that refuses the dialect your agent speaks. Nothing runs when the provider is
  not picked.
- `[provider.host_check]` pings a host first. `on_down = "warn"` prints a message and
  launches anyway, `on_down = "abort"` refuses. Pick `warn` only when something between you
  and the host can bring it back on its own.

## Models come from the provider

Hand-written model lists rot: something gets added upstream and nobody notices, or gets
removed and the launch fails with a `model_not_found` that reads like a config bug. So each
provider is asked:

```toml
[provider.catalog]
url = "https://acme.example/v1/models"
auth = "bearer"                        # bearer | x-api-key | none
exclude_contains = ["embed", "image"]  # drop what an agent cannot drive
```

The answer is cached per provider under the config directory and reused for
`catalog_ttl_secs`. `tab` in the menu, or `--refresh`, goes back to the network. Failures
degrade one step at a time rather than erroring: live, then cache, then the config list.
The status line always says which one you are looking at, so a stale list never passes for
a live one.

`[[provider.model]]` entries stay useful for what no API reports:

- a label, a context window, effort levels
- an id that works but is never advertised, `claude-opus-5[1m]` being the standing example.
  Those are listed first, then the rest of the catalogue.

## System prompts

`system_prompts_dir` holds `.md` files named after the models they belong to. Pick a model
and the matching file is already checked, so the common case is one Enter.

Matching is on the file name, case-insensitively:

- `claude-opus-5.md` matches `claude-opus-5`, and also `claude-opus-5[1m]` since any window
  suffix is stripped first
- `orca-v4.md` matches `orca-v4-pro` and `orca-v4-flash`, so one file covers a family
  without being copied per variant
- the dash is required, so `zeta-5.md` covers `zeta-5-air` but never `zeta-5.2`, which is a
  different model rather than a variant

Press `a` to check a file that matches nothing, or pass `--md <name>` on a menu-less
launch. `--no-md` launches with none.

## Two things worth knowing about Claude Code

Both are why this tool sets what it sets, and both are easy to get wrong by hand.

**`CLAUDE_CODE_MAX_CONTEXT_TOKENS` does nothing for `claude-*` models.** The resolver skips
it for any model name starting with `claude-` and reads the built-in table instead, so for
those the model id is the only lever on the window. fastpick refuses to set it there even
when a `context_window` is declared, because a variable that looks like it works and does
not is worse than no variable.

**`ANTHROPIC_SMALL_FAST_MODEL` defaults to a model most third-party endpoints do not
serve.** Claude Code draws its background model from it for agent inspection, resumes,
conversation titles and any subagent without a `model:` frontmatter. When the endpoint
rejects that name, the stream dies before its first content block and the resume reports
`API Error: Content block not found`. Set `small_fast_model` on the provider. OpenCode reads
the same setting from its `small_model` key, which this fills in too.

## Config

Written to your config directory on first run, with one commented block per kind of harness
and per kind of provider:

- `%APPDATA%\fastpick\config.toml` on Windows
- `~/.config/fastpick/config.toml` elsewhere

Keys are referenced by file path and never inlined, and they never reach a config file or a
command line: OpenCode gets `{env:FASTPICK_PROVIDER_KEY}` and Codex gets an `env_key`
pointing at the same variable. `--dry-run` prints the resolved environment with anything
named like a credential replaced by its length.

Every variable an adapter owns is either set or removed, never left to whatever the shell
exported. That is the difference between picking a third-party endpoint and sending your
Anthropic key to it: `ANTHROPIC_API_KEY` is cleared whenever a provider brings its own
`base_url`, whether or not it also brings a key, and `ANTHROPIC_BASE_URL` is cleared when
you pick the agent's own login. A catalogue url on `http://` is refused outright unless it
is loopback or `auth = "none"`, and model ids from a provider are checked against a plain
allowlist before they can reach a command line.

## Build

```
cargo build --release
cargo test
```

The binary is self-contained. Drop it anywhere on your PATH.

## License

MIT.
