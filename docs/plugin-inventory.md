# dsh-rs 插件清单

> 分支：`feature/abstraction`
> 来源：代码勘察 + 远程实测验证（`dsh dump-config` 输出的 7 份契约清单，
> `dsh providers` 实列 provider 路由，`dsh plugin load` 实测动态插件）。

---

## 1. 总览

dsh-rs 以 cordis-rs 为内核，一切皆为插件。当前代码包含：

- **7 个静态插件**：随 base bundle 安装（`install_base`），其中
  `session-persistence` 仅在配置了 `store_dir` 时激活；
- **1 套动态插件机制**：运行时 `dlopen` 独立编译的 cdylib（C-ABI，
  见 `dsh-plugin-contract`），宿主经接口把声明工具注册到 `ctx.tools`。

```text
                  ┌──────────────────────────────────────┐
                  │  base bundle（install_base）           │
                  │  manifest ─ llm ─ sessions ─ tools     │
                  │  system-prompt ─ agent-loop            │
                  │  (+ session-persistence，可选)          │
                  └──────────────┬───────────────────────┘
                                 │
              ┌──────────────────┼──────────────────┐
              ▼                  ▼                  ▼
      dsh-plugin-hello   任意动态插件 cdylib     dsh CLI / 用户
      （示例工具 dsh_hello）（dsh plugin load）    （dsh run/chat）
```

---

## 2. 静态插件（随 base bundle 安装）

| # | 插件名 | 实现 crate | 提供服务（ctx.*） | 依赖（inject） | 安装条件 |
|---|---|---|---|---|---|
| 0 | `manifest` | dsh-bundle | `ctx.manifest`（契约注册表） | — | 总是 |
| 1 | `llm` | dsh-llm | `ctx.llm`、`ctx.llmStreams` | — | 总是 |
| 2 | `sessions` | dsh-session | `ctx.sessions` | — | 总是 |
| 3 | `session-persistence` | dsh-session | `ctx.sessionPersistence` | `sessions` | 配置 `store_dir` 时 |
| 4 | `tools` | dsh-tools | `ctx.tools` | — | 总是 |
| 5 | `system-prompt` | dsh-core | `ctx.systemPrompt` | — | 总是 |
| 6 | `agent-loop` | dsh-core | `ctx.agents` | `sessions`、`systemPrompt`、`tools`、`llm`、`llmStreams` | 总是 |

依赖覆盖校验（`ManifestRegistry::validate_coverage`）：agent-loop 声明 5 个
`requires`，全部被其余插件 `provides` 满足；启动时 `dsh dump-config` 输出
`dependency coverage: OK`。

### 各插件职责

- **manifest** — 契约层：声明式登记每个插件的 provides / requires / tools /
  config，供 `dsh dump-config` 导出与启动期覆盖校验。
- **llm** — 模型适配器板缝：内置 `mock` 适配器；配置的
  `adapters.<name>` 每段注册一个 OpenAI 兼容适配器（如 deepseek /
  opencode），段名即 provider 路由（可被段内 `providers` 覆盖）。
- **sessions** — 事件溯源会话日志与内存 store；经 `SessionView` 暴露读写表面。
- **session-persistence** — JSONL 耐久后端；经接口
  `Arc<dyn SessionPersistenceApi>` 提供 `ctx.sessionPersistence`。
- **tools** — 工具注册表（含 pre-execute / guards / execute / post-execute
  守卫管线）。内置 7 个工具：`bash`、`read_file`、`write_file`、`edit_file`、
  `glob`、`grep`、`todo_write`。
- **system-prompt** — 提示词分段（有序 section）与变量（`{{var}}`）装配。
- **agent-loop** — 默认 agent 驱动：创建 agent、派发 driver 循环、接入
  `agent/pre-step`、`agent/request`、`agent/turn-stopping` 等瀑布扩展点。

---

## 3. 动态插件（运行时加载）

| 项 | 说明 |
|---|---|
| 契约 | `dsh-plugin-contract`：零依赖纯 Rust C-ABI（`DshPluginExports`，
  `ABI_VERSION = 1`；name / setup / describe / invoke / teardown） |
| 宿主 | dsh-bundle `DynamicPlugin`（dlopen）：检查 ABI、读取 JSON 声明
  （inject + tools），激活时经 `DynamicToolSpec`（接口）把工具注册到
  `ctx.tools`，卸载时自动注销并 dispose 插件状态 |
| 示例 | `dsh-plugin-hello`（cdylib）：声明 `inject: ["tools"]`，注册
  `dsh_hello` 工具；独立编译，不依赖 cordis/tokio |
| 用法 | `dsh plugin load target/debug/libdsh_plugin_hello.so` |

---

## 4. 服务提供方 quick reference

| 服务键 | wrapper / 值 | 提供插件 |
|---|---|---|
| `manifest` | `ManifestService` | manifest |
| `llm` | `LlmService` | llm（+ mock、deepseek、opencode 路由） |
| `llmStreams` | `StreamTable`（具体类型，设计另行抽象） | llm |
| `sessions` | `SessionService` | sessions |
| `sessionPersistence` | `Arc<dyn SessionPersistenceApi>` | session-persistence |
| `tools` | `ToolsService` | tools |
| `systemPrompt` | `SystemPromptService` | system-prompt |
| `agents` | `AgentRegistryService` | agent-loop |

---

## 5. 如何查看当前实例的插件与契约

```bash
dsh dump-config          # 全部插件契约清单 JSON + 依赖覆盖校验
dsh providers            # 当前注册的 provider 路由（mock/deepseek/opencode…）
dsh plugin load <path>   # 装载一个新动态插件并列出其声明工具
```

与设计文档（`docs/abstraction-design.md`）和需求文档
（`docs/abstraction-requirements.md`）配套阅读：本清单即
`dsh dump-config` 对需求 FR-C（服务契约清单）的落地明细。