# dsh-rs — 一切皆插件（Rust 版）

[![crates.io](https://img.shields.io/crates/v/dsh-rs)](https://crates.io/crates/dsh-rs)
[![docs.rs](https://img.shields.io/docsrs/dsh-rs)](https://docs.rs/dsh-rs)

[DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)（`dsh`）的
独立 Rust 移植版：一个**一切皆插件**的 agent 宿主，建立在
[cordis-rust](https://github.com/redoop/cordis-rust)（cordis 内核的 Rust 移植，
即 DSH 插件系统的内核）之上。以**单一 crate** 发布在 crates.io 上：

```toml
[dependencies]
dsh-rs = "0.2"
```

```console
$ cargo install dsh-rs     # 安装 dsh 可执行文件（provider 配置在 ~/.dsh/config.json）
$ dsh run --prompt "hi" --provider opencode
```

思维模型与参考实现完全一致：模型适配器、工具、会话持久化、系统提示词装配
与 agent 循环本身，都是挂在同一个共享 `Context` 上的插件。**没有需要打补丁的
特权内核**——你可以通过"在旁边挂一个新插件"来扩展宿主，而每一次注册都是一个
effect，当它所属的插件卸载时自动回卷。

## 核心思想

不需要任何手工连线。一个**插件**声明它 *需要什么*（`inject`）与 *提供什么*
（`ctx.provide`）。cordis 内核自动计算依赖图，并把每个 fiber 收敛到它期望的
epoch：

```text
consumer 先注册（PENDING）
      |  provider 出现
      v
provider LOADING --> ACTIVE --notify--> consumer 重新检查 --> ACTIVE
```

在 dsh-rs 中，agent 循环是一个 `inject` 了 `sessions`、`systemPrompt`、
`tools`、`llm`、`llmStreams` 五个服务的插件——只有当整条脊柱就绪后它才会
激活；任一依赖消失时它按 LIFO 顺序卸载。

## 与参考宿主的架构对照

| DeepSeek Harness (TS) | dsh-rs 模块 | `ctx` 键 |
| --- | --- | --- |
| `packages/llm` — Message / ContentBlock / StreamChunk、适配器板缝 | `llm` | `llm`, `llmStreams` |
| `packages/core/session` — 只追加的 `SessionEvent` 日志 | `session` | `sessions` |
| `packages/core/tools` — 作用域注册表 + 守卫管线 | `tools` | `tools` |
| `packages/core/system-prompt` — 提示词分段/变量 | `core` | `systemPrompt` |
| `packages/core/agent` + `agent-loop` — Agent 句柄 + 驱动 | `core` | `agents` |
| `dsh-base` bundle 组合 | `bundle` | — |
| `dsh` 无头 CLI | `cli`（二进制 `dsh`） | — |
| — | `types` / `api`（共享词汇 / 接口层 + typed events + manifest） | — |

事件词汇与回合流镜像参考实现：

```text
turn/start
  领取 next-step 输入 + 一条排队消息
  装配提示词分段与工具 schema
  -> agent/pre-step                  reject | enter(messages)
     step/start
     追加进入的消息为 user/message
     从日志推导模型历史
     agent/request -> llm/stream -> assistant/chunk* -> assistant/message
     tool/call* -> tools/pre-execute -> guards -> tools/execute -> tool/result*
     step/end
     工具还有请求，或 next-step 输入到达 -> 进入下一步
  -> agent/turn-stopping
turn/end
```

`turn/*`、`step/*`、`user/message`、`assistant/*` 与 `tool/*` 是持久化的会话
事件；`agent/pre-step`、`agent/request`、`llm/stream` 与三个 `tools/*` 事件是
cordis **瀑布**（waterfall）——监听器可通过调用 `next()` 否决或改写。
`session/event` 火线喂养持久化，`session/flush` 是被等待的耐久化检查点。

## 目录结构（单一 crate）

```text
Cargo.toml       包 dsh-rs 0.2.0（库 `dsh_rs` + 二进制 `dsh`）
src/
  types/         共享词汇（消息、流、会话事件、agent 选项）
  api/           接口层：服务 trait + wrapper、typed events、插件 manifest
  llm/           适配器板缝：mock + OpenAI 兼容 SSE 适配器、分块组装、路由
  session/       只追加会话日志、SessionStore、surface 投影（derive_messages）、
                 JSONL 持久化与崩溃回合修复
  tools/         ToolDefinition/ToolSchema、ToolRegistry 守卫管线
                 （pre-execute -> guards -> execute -> post-execute）、
                 内置：bash, read_file, write_file, edit_file, glob, grep, todo_write
  core/          SystemPromptService（sections/contexts/variables）、agent 注册表
                 + 句柄（followup/steer/inject/cancel/when_idle）、agent-loop 驱动
  bundle/        base bundle 组合 + JSON profile 安装器 + manifest 注册表 + 动态插件宿主
  cli/           runner 辅助 + 终端聊天 UI（`dsh chat`）
  main.rs        `dsh` 二进制：run / chat / transcript / providers / plugin load / dump-config
tests/           集成测试（对话流、TUI 状态、动态插件、会话/工具/LLM 回归）
crates/
  dsh-plugin-contract/   已发布的 C-ABI 契约（0.2.0，crates.io）
  dsh-plugin-hello/      示例 cdylib 插件（独立构建）
```

## 构建与测试

```sh
cargo build                          # 单 crate：根目录直接构建
cargo test                           # 70 个测试，全部通过
```

## 运行

`mock` provider 无需网络：它会回显最后一条用户消息，也可以被脚本化用于
工具调用流（测试套件全程使用）。

```sh
# 对 mock provider 运行一次提示词
./target/debug/dsh run --prompt "list the files" --cwd /tmp

# 交互式聊天 —— 终端上为完整 TUI，管道输入时为行模式
./target/debug/dsh chat          # TUI：enter 发送，↑/↓ 历史，pgup/pgdn 滚动，
                                 # ctrl-u 清空，ctrl-c / ctrl-q 退出（IME 安全：
                                 # 单独的 'q' 永不退出）
printf "hello\n" | ./target/debug/dsh chat   # 管道输入 → 行模式
./target/debug/dsh chat --line   # 强制行模式

# 将会话持久化到 JSONL store，然后读回 transcript
./target/debug/dsh run --prompt "hello" --store /tmp/dsh-sessions
./target/debug/dsh transcript session-1 --store /tmp/dsh-sessions

# OpenAI 兼容 provider（基于 SSE 的 chat-completions）
./target/debug/dsh run --prompt "hello" \
    --provider openai --model gpt-4o-mini \
    --openai-base https://api.openai.com/v1 --openai-key sk-...

# JSON profile：{ "bundles": ["base"], "config": { "store_dir": ..., "openai": ... } }
./target/debug/dsh run --prompt "hello" --profile profile.json
```

## DeepSeek / 多 provider 配置

`dsh` 会按 `$DSH_CONFIG`、`~/.dsh/config.json`、`./dsh.json` 的顺序自动发现默认
配置（首个命中生效；`dsh.json` 已被 gitignore，密钥不会进入提交）。配置就位后，
`dsh run` 与 `dsh chat`（TUI）即可直连 DeepSeek：agent 循环以流式输出，工具
调用（bash、文件工具……）真实执行：

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
  },
  "adapters": {
    "opencode": {
      "providers": ["opencode"],
      "base_url": "https://opencode.ai/zen/go/v1",
      "api_key": "sk-...",
      "model": "deepseek-v4-flash"
    }
  }
}
```

```sh
./target/debug/dsh chat                 # 对 DeepSeek 的 TUI
./target/debug/dsh run --prompt "请执行 date 命令并把结果告诉我"
./target/debug/dsh run --provider opencode --prompt "用一句话介绍你自己"
```

> `adapters.<name>` 每段注册一个 OpenAI 兼容适配器，段名即 provider 路由
> （可用段内 `providers` 覆盖）；`dsh providers` 列出当前全部路由。

OpenAI 兼容适配器针对 DeepSeek 的思考模式做了调优：推理增量拥有独立的块索引，
后续请求会把 `reasoning_content` 回显回去（DeepSeek 拒绝省略该字段的调用），
空 `tool_calls` 数组不会出现在线上消息里。

## 测试 `dsh chat`

`tests/chat_flow.rs` 在三个层面验证 chat TUI：

- **二进制冒烟测试** —— 在封闭环境（临时 `HOME`、无默认配置、mock provider）
  中启动真实的 `dsh` 二进制，断言一次完整的管道对话：横幅、每行一次回显、干净退出。
- **按键处理** —— 用真实 harness 驱动 `ChatTui::handle_key`：输入、Enter 提交、
  `↑/↓` 历史回看、`Esc` 清空、`q`/Ctrl-C 退出语义。
- **渲染** —— 把帧渲染进 ratatui `TestBackend` 缓冲区，断言可见布局（标题 +
  会话 id、对话行、输入提示、状态/快捷键），包括一段脚本化工具调用回合：
  `bash` → 结果 → 最终回答。

```sh
cargo test --test chat_flow   # 16 个测试，无需终端
cargo test                    # 全量 70 个测试
```

## 动态插件（独立编译 + 运行时加载）

插件可以**独立编译为 cdylib** 并**在运行时加载**——宿主 dlopen 它并适配为
cordis `Plugin` trait。插件只讲一个零依赖的 C-ABI 契约（`dsh-plugin-contract`），
**绝不链接 cordis 或 tokio**：宿主实现框架，插件只负责计算（与 cordis-rust 的
`dynhost` 同款设计）。

```sh
# 1. 把示例插件编译为独立库
(cd crates/dsh-plugin-hello && cargo build)

# 2. 把它加载进运行中的宿主；它声明工具并完成注册
./target/debug/dsh plugin load crates/dsh-plugin-hello/target/debug/libdsh_plugin_hello.so
#   loaded plugin: dsh-plugin-hello (state: Active)
#   declared tools: dsh_hello
#   tools now registered: bash, dsh_hello, edit_file, ...

# 3. agent 循环现在可以像内置工具一样调用 dsh_hello
./target/debug/dsh run --prompt "use dsh_hello" --provider mock
```

插件的 JSON 声明（`describe`）列出它需要的服务（`inject`）与提供的工具；每个
工具的 `execute` 通过 `invoke` 回调进库。加载、注册与卸载都是 cordis fiber
effect——卸载插件会自动注销其工具。带有 `inject: ["tools"]` 的插件只在服务
就绪后收敛，与静态插件完全一致。写你自己的插件：实现 `DshPluginExports`
（参见 `crates/dsh-plugin-hello`）并导出为 `dsh_plugin_exports`。

## 扩展

加工具：构造 `ToolDefinition` 并注册到 `ctx.tools`。加 provider：实现
`LlmAdapter` 并注册到 `ctx.llm`。加提示词段：`ctx.systemPrompt.section(...)`。
拦截行为：监听 `agent/pre-step`、`agent/request`、`tools/pre-execute` 或
`llm/stream` 瀑布。一切皆插件，一切都可以从配置替换。

## License

MIT