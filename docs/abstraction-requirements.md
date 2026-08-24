# dsh-rs 架构抽象：需求文档

> 直属来源：`docs/abstraction-design.md`（提交 `3c01867`，设计基线 `e4e8512`）
> 分支：`feature/abstraction`
> 状态：草稿（提炼自设计文档，待评审）

---

## 1. 文档信息

| 项 | 值 |
|---|---|
| 文档目的 | 从设计文档提炼为可评审、可验收的需求清单 |
| 需求范围 | `feature/abstraction` 分支的架构抽象改造 |
| 上游输入 | `docs/abstraction-design.md`（§1–§8 为需求来源，§9 为范围外项） |
| 追溯方式 | 每条需求标注设计文档来源章节 |

---

## 2. 背景与问题陈述

dsh-rs 以 cordis-rs 为内核，一切皆为插件：`llm`、`sessions`、`tools`、`system-prompt`、
`agent-loop`、`session-persistence` 六类插件挂载在同一个 `Context` 上，依赖图自动收敛。
"实现可替换、扩展即新增插件"已实现，但存在三类耦合残留（设计文档 §1）：

| 编号 | 问题 | 现状 | 危害 |
|---|---|---|---|
| P1 | 实现耦合 | 消费者直接 `ctx.require::<LlmRuntime>`、`require::<SessionStore>` 等**具体实现类型** | 消费者编译期依赖实现 crate；换实现 = 改消费者 |
| P2 | 事件契约脆弱 | 事件沿 cordis 总线传输 `serde_json::Value`，事件名与 payload 是"约定" | 改 payload 只在运行时暴露；两端各自序列化易错 |
| P3 | 契约不可见 | 谁提供什么服务/依赖什么，散落在插件代码里 | 启动时无法整体查看与校验依赖覆盖 |

**业务目标**：把耦合从"实现层"转移到"契约层"——契约稳定、实现自由，且契约本身
可被文档化、可被校验。

---

## 3. 范围

### 3.1 范围内（本分支必须交付）

- **R1 接口隔离**：接口 trait 下沉到独立 crate `dsh-api`，消费者永不 import 实现 crate。
- **R2 事件类型化**：事件名与 payload 绑定为类型化单元，改 payload 生产端与监听端同时编译失败。
- **R3 服务契约清单**：声明式 `PluginManifest`、启动期导出与依赖覆盖校验。

### 3.2 范围外（设计文档 §9，后续版本候选）

- 瀑布事件（`agent/pre-step`、`agent/request`、`tools/*`）的类型化（保留 `Next` 改写语义的 typed helper）。
- `llmStreams` 的 `StreamTable` 抽象为 `StreamTableApi`。
- 契约版本化（`PluginManifest.abi/contract_version` 配合 `dsh-plugin-contract::ABI_VERSION` 启动校验）。
- 生成式清单（build-time 从插件源码注解生成 manifests，消除手写漂移）。

---

## 4. 术语表

| 术语 | 含义 |
|---|---|
| `Context` | cordis 服务容器句柄，插件挂载与依赖注入的载体 |
| wrapper | 廉价克隆（`Arc`）的 struct，包裹 `Arc<dyn Trait>`，由 `wrapper!` 宏统一生成，实现 `Deref` |
| 视图接口 | `SessionView` / `AgentView`，跨插件暴露会话/agent 读写表面的接口，隐藏具体实现 |
| `EventPayload` | 事件类型化单元：`NAME` 常量 + serde 编解码 + typed `emit`/`on` |
| `PluginManifest` | 插件声明式契约：provides / requires / tools / config |
| waterfall 事件 | 需要改写/否决语义（`next` 责任链）的事件扩展点 |
| `DynamicToolSpec` | 动态插件（cdylib）注册工具用的接口侧描述（schema + exec 回调） |

---

## 5. 功能需求

### FR-A 接口隔离（来源：设计文档 §2 R1、§3）

| 编号 | 需求 | 验收 / 可验证方式 |
|---|---|---|
| FR-A1 | 新增接口 crate `dsh-api`，消费者（agent-loop、TUI、CLI、动态插件宿主）只依赖接口，**永不 import 实现 crate 的类型** | 代码审查 + 编译依赖检查：消费方 crate 的 `Cargo.toml` 不含实现 crate 依赖 |
| FR-A2 | 每个服务暴露为「接口 trait + 廉价克隆 wrapper」，wrapper 作为 cordis 服务值提供（`LlmService` 等，见 §3.2 服务清单表） | `ctx.require::<XxxService>` 可取得并 `Deref` 调用 |
| FR-A3 | wrapper 采用 `wrapper!` 宏统一生成（Clone struct 包 `Arc<dyn Trait>` + `Deref`），避免 cordis `ctx.provide` 内层 `Arc` 导致的 `Any` downcast 失败 | 运行时 require 不 panic（downcast 成功）；无手写样板 |
| FR-A4 | 跨插件实体通过视图接口暴露：`SessionView`（`id / header_cwd / events / surface / derive_messages / append / request_header / open_turn`）、`AgentView`（`id / session / followup / steer / inject / cancel / status / driver_busy / when_idle`），消费者无需具体 `Session`/`Agent` | 消费者代码不出现具体类型，只经视图接口调用 |
| FR-A5 | 接口签名全部使用 `dsh-types` 词汇；异步方法返回 `BoxFuture<T>`（Send） | 编译 + 静态检查 |
| FR-A6 | `SessionStoreApi` 方法一律用 **id 字符串**传参会话（`flush(&str)`、`fork(source_id,…)`），避免消费者把 `Arc<dyn SessionView>` 反转回具体类型的 downcast | 代码审查：接口无具体 Session 参数/返回类型 |
| FR-A7 | `when_idle` 返回 `'static` future：Agent 维护 `pending` 计数器（入队 +1、claim 清零），仅依赖可 clone 状态（计数器 + busy flag + settle channel）构造 `BoxFuture<'static>` | 编译通过且语义与现状一致（入队后等待空闲） |
| FR-A8 | 动态插件（cdylib）注册工具走接口 `DynamicToolSpec`（schema + `exec` 回调），不暴露具体 `ToolDefinition`；`dsh-tools` 在接口实现内包装为 `ToolDefinition`；支持 `register_dynamic_tool` / 卸载时 `unregister_dynamic_tool` | 动态插件加载/卸载测试通过；cdylib 不依赖 dsh-tools |
| FR-A9 | `todo_write` 由 dsh-core 迁入 dsh-tools 作为内置工具（经 `SessionService` 写 `todo/write` 事件），消除 agent-loop 对具体 `ToolRegistry::register` 的依赖 | 功能回归：todo 工具行为不变；agent-loop 不依赖具体注册 API |
| FR-A10 | 消费者统一经 wrapper 注入五个服务（llm / sessions / tools / system-prompt / agents），`ctx.require::<XxxService>(常量)?` 形式获取 | 代码审查 + 启动冒烟 |

### FR-B 事件类型化（来源：设计文档 §2 R2、§4）

| 编号 | 需求 | 验收 / 可验证方式 |
|---|---|---|
| FR-B1 | 定义 `EventPayload` trait：`Serialize + DeserializeOwned + Send + Sync + 'static`，含 `const NAME: &'static str`，事件名与 payload 绑定为一个单元 | 编译 + 类型检查 |
| FR-B2 | 提供 typed `emit` / `on` helper（`emit(ctx, &E)` → `ctx.emit(E::NAME, to_value)`；`on::<E,F>(ctx, handler)` → 反序列化后回调） | 单元测试：emit/on 闭环 |
| FR-B3 | **编译期类型安全**：修改 payload 结构 → 生产端与监听端同时编译失败（这是核心验收标准） | 故意改 payload 字段，观察两端编译报错 |
| FR-B4 | 核心广播型事件全部迁移为 typed payload：`session/event`（`SessionEventPayload{session, event}`）、`session/created` / `session/disposed`（`{session}`）、`agent/created` / `agent/disposed` / `agent/status`（`{agent[, status]}`）、`agent/error`（`{agent, error}`）、`system-prompt/change`（空 payload） | 迁移清单逐项核对；emit 端类型为本表 payload |
| FR-B5 | 迁移已覆盖的发出端/监听端：dsh-session store（session/*）、dsh-core agent 与 loop_driver（agent/*）、TUI `attach_listener`（typed `on`） | 代码审查 + 功能冒烟 |
| FR-B6 | 瀑布事件（`agent/pre-step` 等需改写/否决语义）**保持显式 `to_value`/`from_value`**，保留 `next` 责任链；类型安全优先覆盖广播型生命周期事件 | 明确边界注释 + 瀑布功能回归 |

### FR-C 服务契约清单（来源：设计文档 §2 R3、§5）

| 编号 | 需求 | 验收 / 可验证方式 |
|---|---|---|
| FR-C1 | 定义契约结构：`ServiceDecl{name, description}`、`PluginManifest{name, description, provides, requires(= inject 列表), tools, config}` | 结构序列化 JSON 测试 |
| FR-C2 | 定义 `ManifestApi`（`register / list / provided_services`）与 `ManifestRegistry`（含 `validate_coverage(&[PluginManifest]) -> Vec<String>`） | 单元测试：覆盖/缺失场景 |
| FR-C3 | `manifest` 插件由 dsh-bundle 提供 `ManifestService`（服务名 `manifest`） | 启动注册冒烟 |
| FR-C4 | `install_base` 收敛后调用 `register_manifests(ctx)`，集中登记六插件契约（llm / sessions / session-persistence / tools / system-prompt / agent-loop） | 启动日志/测试断言清单完整 |
| FR-C5 | agent-loop 声明 5 个 `requires`（对应其注入的五个服务） | manifest 校验测试 |
| FR-C6 | CLI 新增 `dsh dump-config`：打印全部清单 JSON + 依赖覆盖校验；缺失时列出 "XYZ requires `name`" 并报错 | `dsh dump-config` 现场运行验证（当前为待补项，见 §8） |

---

## 6. 非功能需求（来源：设计文档 §1、§3、§8）

| 编号 | 需求 | 说明 / 验收 |
|---|---|---|
| NFR-1 | 可维护性（解耦） | 契约稳定、实现自由；消费者编译期与实现 crate 解耦（对应 FR-A1） |
| NFR-2 | 可验证性 | 契约本身可文档化、可校验；启动时依赖覆盖校验（FR-C2/C6） |
| NFR-3 | 兼容性/回归 | 测试规模与 main 分支一致（69 个，按接口改写而非删减）；外部行为（会话、agent、工具、事件语义）不回归 |
| NFR-4 | 性能 | wrapper 为廉价克隆（`Arc` 共享），避免额外深拷贝/锁开销 |
| NFR-5 | 可测试性 | 每实现 crate 可独立测试；接口层可用 mock 实现（dsh-llm mock adapter 延续） |

---

## 7. 约束

| 编号 | 约束 | 说明 |
|---|---|---|
| C-1 | 内核约束 | 必须基于 cordis-rs 的 `Context` 服务机制，保持"一切皆插件、依赖图自动收敛" |
| C-2 | 类型约束 | trait 对象方法需 `BoxFuture<'static>`（Send）；`Any` downcast 限制是 wrapper 模式的直接动因 |
| C-3 | 动态插件约束 | 动态插件为独立 cdylib，经 `dsh-plugin-contract::DshPluginExports`（describe/invoke）与宿主交互，接口侧只暴露 `DynamicToolSpec` |
| C-4 | 分层约束 | 依赖方向单向向下：消费者 → dsh-api → dsh-types（纯词汇）；`dsh-types` 除 cordis `Context` 句柄（ToolRunContext 内）外不依赖任何 dsh 实现 |

---

## 8. 验收标准（汇总）

1. `cargo build --workspace` 通过（分支回滚前已通过；需重新现场确认）。
2. `cargo test --workspace` 全绿，测试规模 ≥ 69（**当前待补验证**，设计文档 §9.5）。
3. `dsh dump-config` 现场运行：输出六插件清单 JSON，agent-loop 的 5 个 `requires` 均被满足；人为移除某 provide 时校验报错（**当前待补验证**）。
4. 编译期类型安全演示：修改任一 session/agent payload 结构，生产端与监听端同时编译失败。
5. 消费者 crate 静态依赖检查：无实现对 `dsh-llm`/`dsh-session`/`dsh-tools`/`dsh-core` 的类型依赖。

---

## 9. 优先级（MoSCoW）

| 优先级 | 需求 |
|---|---|
| **Must** | FR-A1–A10、FR-B1–B6、FR-C1–C6（R1/R2/R3 全部为分支核心交付） |
| **Should** | NFR-1–NFR-5（随架构改造一并达成） |
| **Won't (now)** | 范围外四项（§3.2）：瀑布事件类型化、`StreamTableApi`、契约版本化、生成式清单 |

---

## 10. 风险与开放问题（来源：设计文档 §9）

| 编号 | 风险/问题 | 缓解/对策 | 当前状态 |
|---|---|---|---|
| R-1 | 瀑布事件未类型化，`agent/pre-step` 等仍靠显式 Value | 已有 typed helper 思路（payload 类型化 + 保留 next），列入后续版本 | 开放 |
| R-2 | `llmStreams` 以具体 `StreamTable` 暴露，是"跨插件传输流句柄"的实现通道 | 可评估抽 `StreamTableApi` | 开放 |
| R-3 | 手写 manifest 与实际 `inject`/`provides` 漂移 | 运行时校验是对冲手段；build-time 生成式清单为长期方案 | 开放 |
| R-4 | **验证缺口**：`cargo test --workspace` 与 `dsh dump-config` 最终现场验证未完成（曾受执行环境故障影响） | 补跑验证 | **待补** |

---

## 11. 需求追溯矩阵

| 需求 | 设计文档章节 | 主要实现位置（crate） |
|---|---|---|
| FR-A1–A3 | §1、§2 R1、§3.1、§3.3(1) | `dsh-api`（新）、各实现 crate |
| FR-A4–A5 | §3.2 服务清单、§2 R1 | `dsh-api::services` |
| FR-A6 | §3.3(2) | `dsh-api::services`（SessionStoreApi）、dsh-session |
| FR-A7 | §3.3(3) | dsh-core（Agent/when_idle） |
| FR-A8 | §3.3(4)、§6 | `dsh-plugin-contract`、dsh-bundle、dsh-tools |
| FR-A9 | §7 依赖变化要点 | dsh-tools（todo 内置）、dsh-session |
| FR-A10 | §6 接入指南 | 各消费方 |
| FR-B1–B3 | §4 | `dsh-api::events` |
| FR-B4–B5 | §4 注册表 | dsh-session、dsh-core、TUI |
| FR-B6 | §4 界限说明 | dsh-core（waterfall 保持显式 Value） |
| FR-C1–C2 | §5 | `dsh-api::manifest` |
| FR-C3–C5 | §5 | dsh-bundle（`register_manifests`、ManifestService） |
| FR-C6 | §5、§7 | dsh-cli（`dump-config`） |