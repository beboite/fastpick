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
fastpick --list --json                          # the same, for another program
```

Each of `--harness`, `--provider` and `--model` skips its own screen. The menu opens on the
first one you left out.

Up and down move, right goes forward, left goes back. On the model list Enter launches
straight away: the system prompt file matching the model is already checked and the effort
is the model's default, so the usual case is one key.

Right on a model opens the options panel beside the list rather than after it. There, space
changes whatever the cursor is on, the effort level or a system prompt file, `a` lists every
file in the prompts folder, left closes it and Enter launches. `tab` refetches the model
list and typing filters it.

## Setting it up

Drop the binary anywhere on your PATH, then run `fastpick` once. It writes a starter config
and stops, because the providers it ships are examples rather than endpoints:

```
fastpick                    # writes the config, tells you where, exits
fastpick --edit             # opens it in $VISUAL, $EDITOR, or your platform's default
```

The file has one commented block per kind of harness and per kind of provider. Editing it
is the whole configuration; nothing else has a setup step.

1. **Keep the harnesses you use.** They are declared, not detected, but only the ones whose
   `bin` is actually installed are offered, so leaving a block in for the machine where you
   do have that agent costs nothing.
2. **Replace the example providers with your endpoints.** One `[[provider]]` per endpoint,
   with a `[provider.harness.<id>]` block for each agent that can reach it. A provider with
   no block for an agent simply does not appear when that agent is picked.
3. **Give each one a key file** with `auth_token_file`, then write the key without it ever
   reaching your shell history:

```
fastpick --set-key acme     # prompts, does not echo, writes owner-only
pass show acme | fastpick --set-key acme      # or pipe it in
fastpick --paths            # where everything lives, and who can read each key file
```

4. **Point `system_prompts_dir` wherever you keep your `.md` files**, or drop them in the
   folder fastpick already made next to the config.

Everything is a path you choose: the config with `--config`, the prompts folder with
`system_prompts_dir`, each key with `auth_token_file`, and each agent with its `bin`, which
takes a full path when the binary is not on PATH. Paths accept `~`, `$VAR` and `%VAR%`, so
one config can follow you across machines.

Nothing is written to your agents' own config files, and no credential is ever written into
a config file, a command line or a log.

## Harnesses

| Harness | Endpoint goes in as | Model | Extra instructions |
|---|---|---|---|
| Claude Code | environment variables | `--model` | `--append-system-prompt-file`, appends |
| OpenCode | one inline JSON config in `OPENCODE_CONFIG_CONTENT` | `--model provider/model` | its `instructions` array, appends |
| Codex | dotted TOML overrides with `-c` | `--model` | **not supported**, see below |

Only the harnesses whose binary is on this machine are offered: the config describes what
could be launched anywhere, and one file is meant to follow you across machines that do not
have the same agents installed. `--list` shows them all, marking what is missing, and
`--harness <id>` runs one regardless, in case the lookup is wrong.

Nothing writes to your agents' own config files. OpenCode's inline config is *merged* over
`opencode.json` rather than replacing it, so MCP servers, plugins and agents survive
untouched, and Codex's `-c` overrides never touch `~/.codex/config.toml`.

**Codex gets no system prompt row on purpose.** It has no append-only surface for extra
instructions: its instructions override replaces the base prompt, tool rules included.
Rather than quietly swapping the agent's own prompt for yours, the options panel says the
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
- `$XDG_CONFIG_HOME/fastpick/config.toml`, or `~/.config/fastpick/config.toml`, on macOS and
  Linux alike. Not `~/Library/Application Support` on macOS: this is a file you edit and
  keep in a dotfiles repo, not application state.

Beside it sit `system-prompts/`, `catalog/` (one cached model list per provider) and
`state.toml`, which remembers where the menu was left. `--paths` prints all of them.

Keys are referenced by file path and never inlined, and they never reach a config file or a
command line: OpenCode gets `{env:FASTPICK_PROVIDER_KEY}` and Codex gets an `env_key`
pointing at the same variable. `--dry-run` prints the resolved environment with the
credentials replaced by their length.

`--set-key <provider>` writes one, reading it from stdin so it is never an argument: a key
typed as an argument lands in your shell history and in the process list of every user on
the machine. The file is created owner-only, and on Unix a key that anyone else can read is
reported by `--paths` and again on the launch that uses it. On Windows there are no mode
bits to check, so it inherits the permissions of your user profile, which is what the agents
themselves rely on for their own credentials.

## Driving it from another program

`--json` turns `--list` and `--dry-run` into machine output, so an editor or a terminal
multiplexer can offer the same three choices in its own interface and then launch fastpick
with them. Nothing else has to be reimplemented: the key files, the proxy it starts, the
host it wakes and the environment it builds stay on this side.

```
fastpick --list --json                    # harnesses, providers, bindings. No network
fastpick --list --json --provider acme    # and that provider's models, cache first
fastpick --harness claude-code --provider acme --model acme-large   # launch, no menu
```

Exit code 0 means stdout holds one JSON document and nothing else; notices and errors go to
stderr. The payload carries a `schema` number, bumped only when a consumer would have to
change.

Two things are deliberately absent. A provider reports `needsKey` and `keyPresent` but
never where its key file is and never what is in it, and `--dry-run --json` names the
credential variables under `secretEnv` with their length alone. So the picking can happen
anywhere, while the secret is only ever read on the machine that runs the agent.

Listing the models means one HTTP call, which is why they only appear when `--provider` is
named. The answer is cached, so a caller can open its own menu without waiting; `--refresh`
is the explicit way to go and look again.

## Build

```
cargo build --release
cargo test
```

The binary is self-contained. Drop it anywhere on your PATH.

## License

MIT.
