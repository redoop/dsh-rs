# dsh-rs — everything is a plugin, in Rust

A standalone Rust port of [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)
(`dsh`): an agent harness where **everything is a plugin**, built on
[cordis-rs](https://github.com/redoop/cordis-rs) — the Rust port of the cordis
kernel that powers the original DSH plugin system.

The mental model is identical to the reference implementation: model adapters,
tools, session persistence, the system-prompt assembler, and the agent loop
itself are all plugins providing services on one shared `Context`. There is no
privileged core to patch — you extend the harness by mounting a plugin beside
the others, and every registration is an effect that unwinds when its plugin
unloads.

## The one idea

Nothing wires anything by hand. A **plugin** declares what it *requires*
(`inject`) and what it *provides* (`ctx.provide`). The cordis kernel computes
the dependency graph and converges every fiber toward its desired epoch:

```text
register consumer first (PENDING)
      |  provider appears
      v
provider LOADING --> ACTIVE --notify--> consumer re-checks --> ACTIVE
```

In dsh-rs the agent loop is a plugin that `inject`s `sessions`, `systemPrompt`,
`tools`, `llm`, and `llmStreams` — so it only activates once the whole spine is
live, and unloads (LIFO) when any dependency disappears.

## Architecture vs the reference harness

| DeepSeek Harness (TS) | dsh-rs | `ctx` key |
| --- | --- | --- |
| `packages/llm` — Message / ContentBlock / StreamChunk, adapter seam | `dsh-llm` | `llm`, `llmStreams` |
| `packages/core/session` — append-only `SessionEvent` log | `dsh-session` | `sessions` |
| `packages/core/tools` — scoped registry + guarded pipeline | `dsh-tools` | `tools` |
| `packages/core/system-prompt` — prompt sections/variables | `dsh-core` | `systemPrompt` |
| `packages/core/agent` + `agent-loop` — Agent handle + driver | `dsh-core` | `agents` |
| `dsh-base` bundle composition | `dsh-bundle` | — |
| `dsh` headless CLI | `dsh-cli` | — |

The event vocabulary and turn flow mirror the reference:

```text
turn/start
  claim next-step input plus one queued message
  assemble prompt sections + tool schemas
  -> agent/pre-step                   reject | enter(messages)
     step/start
     append entered messages as user/message
     derive model history from the log
     agent/request -> llm/stream -> assistant/chunk* -> assistant/message
     tool/call* -> tools/pre-execute -> guards -> tools/execute -> tool/result*
     step/end
     tools owe another request, or next-step input arrived -> next step
  -> agent/turn-stopping
turn/end
```

`turn/*`, `step/*`, `user/message`, `assistant/*`, and `tool/*` are durable
session events; `agent/pre-step`, `agent/request`, `llm/stream`, and the three
`tools/*` events are cordis **waterfalls** — listeners may veto or rewrite by
calling `next()`. The `session/event` firehose feeds persistence, and
`session/flush` is the awaited durability checkpoint.

## Workspace layout

```text
crates/
  dsh-llm/      Message/ContentBlock/StreamChunk vocabulary, LlmAdapter seam,
                LlmRuntime registry, BlockAssembler, mock adapter,
                OpenAI-compatible SSE adapter (feature "openai")
  dsh-session/  append-only SessionEvent log, Session, SessionStore,
                surface projection (derive_messages), JSONL persistence
                with crash-turn repair
  dsh-tools/    ToolDefinition/ToolSchema, ToolRegistry with the guarded
                pipeline (pre-execute -> guards -> execute -> post-execute),
                built-ins: bash, read_file, write_file, edit_file, glob, grep
  dsh-core/     SystemPromptService (sections/contexts/variables), Agent
                registry + handle (followup/steer/inject/cancel/when_idle),
                the agent-loop driver, todo_write tool
  dsh-bundle/   base-bundle composition + JSON profile installer
  dsh-cli/      the `dsh` binary: run / chat / transcript / providers
```

## Build & test

```sh
source ./env.sh        # points CARGO_HOME at the workspace-local .cargo-home
cargo build --workspace
cargo test --workspace # 18 suites, all green
```

## Run

The `mock` provider needs no network: it echoes the last user message, and can
be scripted for tool-call flows (used throughout the test suite).

```sh
# One prompt against the mock provider
./target/debug/dsh run --prompt "list the files" --cwd /tmp

# Interactive chat — a full TUI on a terminal, line mode when piped
./target/debug/dsh chat          # TUI: enter to send, ↑/↓ history,
                                 #      pgup/pgdn scroll, ctrl-u clear,
                                 #      ctrl-c / ctrl-q quit (IME-safe: bare
                                 #      'q' never quits)
printf "hello\n" | ./target/debug/dsh chat   # piped input → line mode
./target/debug/dsh chat --line   # force line mode

# Persist sessions to a JSONL store, then read a transcript back
./target/debug/dsh run --prompt "hello" --store /tmp/dsh-sessions
./target/debug/dsh transcript session-1 --store /tmp/dsh-sessions

# OpenAI-compatible provider (chat-completions over SSE)
./target/debug/dsh run --prompt "hello" \
    --provider openai --model gpt-4o-mini \
    --openai-base https://api.openai.com/v1 --openai-key sk-...

# JSON profile: { "bundles": ["base"], "config": { "store_dir": ..., "openai": ... } }
./target/debug/dsh run --prompt "hello" --profile profile.json
```

## DeepSeek configuration

`dsh` auto-discovers a default config at `$DSH_CONFIG`, `~/.dsh/config.json`, or
`./dsh.json` (first match wins; `dsh.json` is gitignored so keys never get
committed). With it in place, `dsh run` and `dsh chat` (TUI) talk to DeepSeek
out of the box — the agent loop streams, and tool calls (bash, file tools,
...) execute for real:

```json
{
  "provider": "deepseek",
  "model": "deepseek-v4-flash",
  "store_dir": "/tmp/dsh-sessions",
  "openai": {
    "providers": ["deepseek", "openai"],
    "base_url": "https://api.deepseek.com/v1",
    "api_key": "sk-...",
    "model": "deepseek-v4-flash"
  }
}
```

```sh
./target/debug/dsh chat                 # TUI against DeepSeek
./target/debug/dsh run --prompt "请执行 date 命令并把结果告诉我"
```

The OpenAI-compatible adapter is tuned for DeepSeek's thinking mode: reasoning
deltas get their own block index, `reasoning_content` is echoed back on later
requests (DeepSeek rejects calls that omit it), and empty `tool_calls` arrays
are omitted from wire messages.`

## Testing `dsh chat`

`crates/dsh-cli/tests/chat_flow.rs` verifies the chat TUI at three layers:

- **Binary smoke test** — spawns the real `dsh` binary in a hermetic
  environment (temp `HOME`, no default config, mock provider) and asserts a
  full piped conversation: banner, one echo per line, clean exit.
- **Key handling** — drives `ChatTui::handle_key` against a live harness:
  typing, Enter-to-submit, `↑/↓` history recall, `Esc` clear, and `q` /
  Ctrl-C quit semantics.
- **Rendering** — renders frames into a ratatui `TestBackend` buffer and
  asserts the visible layout (title + session id, conversation lines, input
  prompt, status/hint), including a scripted tool-call turn showing
  `bash` → result → final answer.

```sh
cargo test -p dsh-cli --test chat_flow   # 7 tests, no terminal required
cargo test --workspace                   # 53 tests, all green
```

## Extending

Add a tool: build a `ToolDefinition` and register it on `ctx.tools`. Add a
provider: implement `LlmAdapter` and register it on `ctx.llm`. Add a prompt
section: `ctx.systemPrompt.section(...)`. Intercept behavior: listen on the
`agent/pre-step`, `agent/request`, `tools/pre-execute`, or `llm/stream`
waterfalls. Everything is a plugin, and everything is replaceable from
configuration.

## License

MIT
