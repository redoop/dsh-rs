# dsh-rs 架构抽象：需求与设计

> 分支：`feature/abstraction`（提交 `e4e8512`）
> 主题：将插件式架构推向"教科书级的低耦合、高内聚"——通过接口下沉、事件类型化、契约清单三个手段。

---

## 1. 背景与动机

dsh-rs 以 cordis-rs 为内核，一切皆为插件：`llm`、`sessions`、`tools`、
`system-prompt`、`agent-loop`、`session-persistence` 六类插件挂载在同一个
`Context` 上，依赖图自动收敛。这套架构已实现"实现可替换、扩展即新增插件"。

但存在三类耦合残留，阻碍进一步解耦：

| 耦合点 | 现状 | 问题 |
|---|---|---|
| **实现耦合** | `agent-loop` 等消费者直接 `ctx.require::<LlmRuntime>`、`require::<SessionStore>` 等**具体实现类型** | 消费者在编译期依赖实现 crate；换实现 = 改消费者 |
| **事件契约脆弱** | 事件沿 cordis 总线传输 `serde_json::Value`，事件名与 payload 是"约定" | 改 payload 只在运行时暴露；两端各自序列化易错 |
| **契约不可见** | 谁提供什么服务/依赖什么，散落在插件代码里 | 启动时无法整体查看与校验依赖覆盖 |

**目标**：把耦合从"实现层"转移到"契约层"——契约稳定、实现自由，且契约本身
可被文档化、可被校验。

---

## 2. 需求

### R1 接口隔离（trait 下沉独立 crate）

- 消费者只依赖接口 crate（`dsh-api`），**永不 import 实现 crate 的类型**。
- 每个服务暴露为：接口 trait + 廉价克隆的 wrapper（`LlmService` 等），wrapper
  作为 cordis 服务值提供。
- 跨插件的实体（会话、agent）通过"视图接口"（`SessionView` / `AgentView`）
  暴露，隐藏具体实现。
- 动态加载的插件注册工具也走接口（`DynamicToolSpec`），不暴露具体
  `ToolDefinition`。

### R2 事件类型化

- 事件名与 payload 类型**绑定为一个单元**（`EventPayload` trait：`NAME` +
  serde 编解码）。
- 提供 typed `emit` / `on` helper；改 payload 结构 → 生产端与监听端**同时编译失败**。
- 核心事件全部迁移：`session/*`、`agent/*`。

### R3 服务契约清单与校验

- 每个插件注册声明式 `PluginManifest`（provided 服务 / required 依赖 /
  tools / config）。
- 启动时可导出全量清单（`dsh dump-config`）并对依赖覆盖做**校验**：
  每个 `requires` 必须被某插件的 `provides` 满足。

---

## 3. 总体设计

### 3.1 分层与依赖方向

```
┌─────────────────────────────────────────────────────────────┐
│ 消费者：agent-loop 插件 / TUI / CLI / 动态插件宿主              │
│   只依赖 dsh-api（接口）                                        │
└───────────────┬───────────────────────────────┬──────────────┘
                │ ctx.require::<LlmService>()    │ 注册契约 (manifest)
                ▼                               ▼
┌────────────────────────┐      ┌──────────────────────────────┐
│ dsh-api（接口层）        │      │ manifest 注册表                 │
│  · services: trait+     │      │  · ManifestRegistry            │
│    wrapper+视图          │      │  · PluginManifest             │
│  · events: EventPayload │      └──────────────────────────────┘
│  · manifest: 契约结构    │
└───────────────┬────────┘
                │ 实现 trait / 提供 wrapper
                ▼
┌────────────────────────┐
│ dsh-types（契约词汇）    │ ← dsh-api 与全部实现 crate 共同依赖
│  Message / StreamChunk /│    纯数据，唯一框架类型是 cordis
│  SessionEvent /         │    Context（在 ToolRunContext 内）
│  ToolExecutionResult …  │
└────────────────────────┘
```

- **dsh-types**：共享词汇，零 dsh 实现依赖（仅 cordis `Context` 句柄）。
- **dsh-api**：接口 trait、wrapper、视图接口、typed events、manifest 结构。
  依赖 `dsh-types` + `cordis`。
- **实现 crate**（dsh-llm/session/tools/core）：实现接口 trait，插件 `apply`
  时提供 wrapper 服务值。
- **消费方**：`ctx.require::<LlmService>` 等，经 wrapper `Deref` 到 trait object。

### 3.2 服务清单（`dsh-api::services`）

| 服务名常量 | 接口 trait | wrapper | 实现方 |
|---|---|---|---|
| `llm` | `LlmRuntimeApi` | `LlmService` | dsh-llm |
| `llmStreams` | —（保留具体 `StreamTable`） | — | dsh-llm |
| `sessions` | `SessionStoreApi` | `SessionService` | dsh-session |
| `sessionPersistence` | `SessionPersistenceApi` | — | dsh-session |
| `tools` | `ToolRegistryApi` | `ToolsService` | dsh-tools |
| `systemPrompt` | `SystemPromptApi` | `SystemPromptService` | dsh-core |
| `agents` | `AgentRegistryApi` | `AgentRegistryService` | dsh-core |
| `manifest` | `ManifestApi` | `ManifestService` | dsh-bundle |

跨实体视图接口：

- `SessionView`：`id / header_cwd / events / surface / derive_messages / append /
  request_header / open_turn`——会话的读写表面，消费者无需具体 `Session`。
- `AgentView`：`id / session / followup / steer / inject / cancel / status /
  driver_busy / when_idle`——agent 句柄。

接口签名全部使用 `dsh-types` 词汇；异步方法返回 `BoxFuture<T>`（Send）。

### 3.3 关键设计取舍

1. **wrapper 而不是裸 trait object**：cordis `ctx.provide` 内部再包一层
   `Arc`，直接提供 `dyn Trait` 会导致 `require::<T>` 的 `Any` downcast 失败；
   因此采用"Clone 的 wrapper struct 包 `Arc<dyn Trait>`"模式（macro `wrapper!`
   统一生成），并实现 `Deref` 免去一层解引用样板。

2. **视图接口代替 downcast**：`SessionStoreApi` 的方法一律用 **id 字符串**
   传参会话（`flush(&str)`、`fork(source_id,…)`），避免消费者把
   `Arc<dyn SessionView>` 转回具体 `Arc<Session>` 的 `Any` downcast。

3. **`when_idle` 的 `'static` future**：trait 对象需要 `BoxFuture<'static>`，
   而 `Agent::when_idle` 是借用 `&self` 的 async fn。方案：Agent 维护
   `pending` 计数器（入队 +1、claim 清零），`when_idle` 仅依赖可 clone 的
   状态（计数器 + busy flag + settle channel）即可构造 `'static` future。

4. **`DynamicToolSpec`**：动态插件（cdylib）注册工具不再构造具体
   `ToolDefinition`，而是提供 schema + `exec` 回调；`dsh-tools` 在接口
   实现内把它包装为 `ToolDefinition`。宿主与外来代码之间保持接口隔离。

---

## 4. 事件类型化（`dsh-api::events`）

```rust
pub trait EventPayload: Serialize + DeserializeOwned + Send + Sync + 'static {
    const NAME: &'static str;
}

pub fn emit<E: EventPayload>(ctx: &Context, payload: &E);   // → ctx.emit(E::NAME, to_value(payload)?)
pub async fn on<E, F>(ctx, handler) -> EffectGuard;         // → ctx.on(E::NAME, |ctx, v,_| from_value::<E>(v)…)
```

注册的 payload 单元：

| 事件 | payload 结构 |
|---|---|
| `session/event` | `SessionEventPayload { session, event }` |
| `session/created` / `session/disposed` | `{ session }` |
| `agent/created` / `agent/disposed` / `agent/status` | `{ agent[, status] }` |
| `agent/error` | `{ agent, error }` |
| `system-prompt/change` | 空 payload |

已迁移的发出端/监听端：dsh-session store（session/*）、dsh-core agent 与
loop_driver（agent/*）、TUI 的 `attach_listener`（typed `on`）。

界限说明：瀑布（`agent/pre-step` 等需要改写/否决的扩展点）仍走显式
`to_value`/`from_value`，保留 `next` 责任链语义；类型安全优先覆盖
"广播型"事件（session/agent 生命周期）。

---

## 5. 服务契约清单（`dsh-api::manifest`）

```rust
pub struct ServiceDecl   { name, description }
pub struct PluginManifest {
    name, description,
    provides: Vec<ServiceDecl>,
    requires: Vec<String>,        // = inject 列表
    tools:    Vec<ToolSchema>,
    config:   Value,              // 插件配置（schema 由各插件校验）
}
pub trait ManifestApi { register / list / provided_services }
pub struct ManifestRegistry { … ; validate_coverage(&[PluginManifest]) -> Vec<String> }
```

- `manifest` 插件由 dsh-bundle 提供 `ManifestService`；
- `install_base` 收敛后调用 `register_manifests(ctx)`，集中登记六个插件的契约
  （llm / sessions / session-persistence / tools / system-prompt / agent-loop），
  agent-loop 声明 5 个 `requires`；
- CLI 新增 `dsh dump-config`：打印全部清单 JSON + 依赖覆盖校验
  （缺失 → 列出"XYZ requires `name`"并报错）。

---

## 6. 接入指南

### 提供方（实现 crate）

```rust
// dsh-tools 示例
let registry = ToolRegistry::new(ctx.clone());
let api: Arc<dyn dsh_api::services::ToolRegistryApi> = Arc::new(registry.clone());
ctx.provide(dsh_api::TOOLS_SERVICE, dsh_api::services::ToolsService::new(api)).await?;
```

trait 实现放在实现 crate 内（如 `impl ToolRegistryApi for ToolRegistry`）。

### 消费方

```rust
let tools = ctx.require::<dsh_api::services::ToolsService>(dsh_api::TOOLS_SERVICE)?;
tools.schemas();                      // Deref → dyn ToolRegistryApi
let agents = ctx.require::<dsh_api::services::AgentRegistryService>(dsh_api::AGENTS_SERVICE)?;
let agent: Arc<dyn dsh_api::services::AgentView> = agents.create(…)?;
agent.session().derive_messages();    // 视图接口
```

### 事件

```rust
// 发出
dsh_api::events::emit(&ctx, &dsh_api::events::AgentErrorPayload { agent, error });
// 监听
dsh_api::events::on::<dsh_api::events::SessionEventPayload, _>(ctx, |ctx, p| {
    Box::pin(async move { … }) }).await?;
```

### 动态插件

cdylib 通过 `dsh_plugin_contract::DshPluginExports`（describe/invoke），宿主
（dsh-bundle `load_dynamic_plugin`）把声明的工具以 `DynamicToolSpec` 通过
`ToolsService::register_dynamic_tool` 注册，卸载时 `unregister_dynamic_tool`。

---

## 7. 依赖关系变化

| crate | 主要依赖 | 说明 |
|---|---|---|
| `dsh-types`（新） | cordis, serde, tokio, futures | 纯词汇 |
| `dsh-api`（新） | dsh-types, cordis | 接口 + 事件 + 清单 |
| `dsh-llm` | dsh-api, dsh-types, cordis | 实现 `LlmRuntimeApi`/`LlmAdapterApi` |
| `dsh-session` | dsh-api, dsh-types, cordis | 实现 `SessionStoreApi`/`SessionPersistenceApi` |
| `dsh-tools` | dsh-api, dsh-types, cordis | 实现 `ToolRegistryApi`；**内含 todo 工具** |
| `dsh-core` | dsh-api, dsh-types, 各实现 crate | 实现 `AgentRegistryApi`/`SystemPromptApi`/视图 |
| `dsh-bundle` | 全部 + contract | manifest 插件、`register_manifests`、动态加载 |
| `dsh-cli` | dsh-api, dsh-bundle, … | 走接口；新增 `dump-config` |

要点：`todo_write` 从 dsh-core 迁入 dsh-tools 内置工具（经 `SessionService`
写 `todo/write` 事件），消除了 agent-loop 对具体 `ToolRegistry::register` 的
依赖；agent-loop 对五个服务全部通过 wrapper 注入。

---

## 8. 与 main 分支（`d1e878b`）的差异

| 维度 | main | feature/abstraction |
|---|---|---|
| 新增 crate | — | `dsh-types`、`dsh-api` |
| 消费者可见类型 | 具体 `LlmRuntime`/`SessionStore`/`ToolRegistry`/`AgentRegistry` | wrapper + trait object（`LlmService` 等） |
| 会话/agent 接口 | 具体 `Session`/`Agent` | `SessionView`/`AgentView` |
| 事件 | 裸 `serde_json::Value` + 手写解析 | `EventPayload` typed emit/on |
| 契约可见性 | 无 | `PluginManifest` + `dsh dump-config` + 依赖校验 |
| todo 工具位置 | dsh-core（agent-loop 注册） | dsh-tools 内置 |
| 动态插件注册 | 直接构造 `ToolDefinition` | `DynamicToolSpec` 走接口 |
| 测试规模 | 69 | 69（同类，已按接口改写） |

---

## 9. 开放问题与后续建议

1. **瀑布事件类型化**：`agent/pre-step`、`agent/request`、`tools/*` 等
   waterfall 目前仍走显式 Value（因需 `Next` 改写语义）。可进一步提供
   typed waterfall helper（payload 类型化 + 保留 next）。
2. **`llmStreams` 具体暴露**：`StreamTable` 仍以具体类型提供，是"跨插件
   传输流句柄"的实现通道；可评估是否抽 `StreamTableApi`。
3. **契约版本化**：`PluginManifest` 增加 `abi/contract_version`，配合
   `dsh-plugin-contract::ABI_VERSION` 做启动时契约校验。
4. **生成式清单**：可考虑 build-time 生成 manifests（如从插件源码注解），
   避免手写清单与实际 `inject`/`provides` 漂移——运行时校验是对冲手段。
5. **验证状态**：本分支在回滚前已通过全量 `cargo build --workspace`；
   分 crate 测试（69 个）在重构后曾全绿；受当时执行环境故障影响，
   **最终 `cargo test --workspace` 与 `dsh dump-config` 的现场验证待补**。