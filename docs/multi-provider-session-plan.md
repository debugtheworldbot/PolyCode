# Multi-Provider Session Support Plan

## 1. 目标

在现有 CodexMonitor 代码库基础上，新增对以下会话提供方的支持：

- Codex（保持现状兼容）
- Claude Code
- Gemini CLI

目标是让三种 provider 在同一套 UI/工作区管理体系下可用，优先实现“可用会话闭环”，再逐步补齐高级能力。

## 2. 结论与实施策略

采用 **增量重构 + provider 适配**，不做全量重写。

原因：

- 现有工作区、Git、设置、daemon、事件总线、前端状态管理可复用。
- 主要改造集中在会话协议层和 provider 差异适配。
- 全量重写会显著增加回归风险和交付周期。

## 3. 范围定义

### 3.1 MVP（第一阶段交付）

每个 provider 至少具备：

- 启动会话（start）
- 发送消息（send）
- 接收流式增量（delta）
- 完成/失败状态（completed/error）
- 中断（interrupt，若 provider 支持）
- 会话列表与恢复（list/resume；若 provider 无原生能力则本地模拟）

### 3.2 后续能力（第二阶段）

按 provider capability 渐进支持：

- model list
- review
- skills / apps
- account/login
- collaboration modes

## 4. 当前架构约束（必须处理）

当前代码中存在强 Codex 耦合：

- 后端进程启动与握手固定为 `codex app-server` + `initialize/initialized`。
- `shared/codex_core.rs` 固定发送 Codex RPC 方法。
- 全局 session 类型是 `crate::codex::WorkspaceSession`。
- 数据模型字段为 `codex_bin/codex_args/codexHome`。
- 前端事件路由包含 `codex/connected`、`codex/backgroundThread` 等特定方法。
- prompts/local usage 依赖 `CODEX_HOME` 与 Codex 目录结构。

## 5. 目标架构

### 5.1 核心原则

- UI 只消费“规范化事件”，不感知 provider 原始协议细节。
- provider 差异收敛在后端 adapter 层。
- app 与 daemon 继续通过 shared core 复用业务逻辑。

### 5.2 新增抽象

- `ProviderKind`：`codex | claude | gemini`
- `ProviderCapabilities`：描述 provider 是否支持 model/review/login/skills/apps/collab/interrupt/list/resume。
- `SessionBackend`（trait 或等价抽象）：统一会话操作接口：
  - `start_thread`
  - `resume_thread`
  - `list_threads`
  - `send_user_message`
  - `interrupt_turn`
  - `archive/compact/set_name`（按 capability 支持）
- `NormalizedEvent`：统一事件语义（thread/turn/item/account/rateLimits 等）。

### 5.3 会话持久化策略

对无原生 `thread/list` 或 `thread/resume` 的 provider，新增本地会话索引存储：

- `provider_sessions.json`
- 保存 workspace_id / provider / thread_id / title / updated_at / provider-native metadata
- 消息增量回放用于“恢复展示”

## 6. 数据模型改造计划

## 6.1 Rust 类型（`src-tauri/src/types.rs`）

- 新增：
  - `ProviderKind` enum
  - `WorkspaceEntry.provider`（默认 `codex`）
  - `AppSettings.default_provider`（默认 `codex`）
- 兼容保留：
  - `codex_bin/codex_args/codex_home` 先保留
- 新增 provider 配置字段（建议）：
  - `provider_bins: { codex?: string, claude?: string, gemini?: string }`
  - `provider_args: { codex?: string, claude?: string, gemini?: string }`
  - `provider_homes: { codex?: string, claude?: string, gemini?: string }`（如适用）

### 6.2 TS 类型（`src/types.ts`）

- 增加同构字段与类型：
  - `ProviderKind`
  - `WorkspaceInfo.provider`
  - `AppSettings.defaultProvider`
  - provider-level bin/args config

## 7. 后端实施分阶段

### Phase 0: 预备与防回归

- 建立基线测试与快照：
  - 当前 Codex 行为不变
  - 关键命令链可回归（start/list/resume/send/interrupt）
- 引入 capability 结构但默认全部沿用 Codex。

交付标准：

- 不引入行为变化。
- 现有测试通过。

### Phase 1: Provider 骨架落地

主要改动：

- 新建 `src-tauri/src/providers/*`（建议）：
  - `mod.rs`
  - `types.rs`（ProviderKind/Capabilities）
  - `session.rs`（trait + 通用 session handle）
  - `registry.rs`（按 workspace provider 分发）
- 将 `AppState.sessions` 从 Codex 专用类型迁移到 provider-agnostic session 容器。

交付标准：

- 代码可编译。
- Codex 路径仍可工作（通过 adapter 包装）。

### Phase 2: Codex adapter 抽离

主要改动：

- 将现有 `backend/app_server.rs` + `shared/codex_core.rs` 能力封装到 `providers/codex_adapter.rs`。
- `workspaces_core` 与 `connect_workspace` 改为通过 provider registry spawn session。
- app 与 daemon 两端都走同一 provider 分发逻辑。

交付标准：

- Codex 行为与当前一致。
- 无前端功能回退。

### Phase 3: Claude Code adapter（MVP）

主要改动：

- 新建 `providers/claude_adapter.rs`：
  - 进程启动参数与 IO 协议适配
  - 原生事件映射为 `NormalizedEvent`
  - 无原生 list/resume 时接入本地会话索引
- 能力声明：
  - 按实际支持填充 capabilities。

交付标准：

- Claude provider 可完成完整消息闭环。
- UI 不崩溃，未支持功能自动降级隐藏/禁用。

### Phase 4: Gemini CLI adapter（MVP）

主要改动：

- 新建 `providers/gemini_adapter.rs`，步骤同 Phase 3。
- 建立 Gemini 的输出解析与标准化映射。

交付标准：

- Gemini provider 可完成完整消息闭环。
- provider 间可切换，线程列表可用。

### Phase 5: 前端 provider 化

主要改动：

- 设置页新增 provider 选择与 provider-specific 配置项。
- `useAppServerEvents` / `appServerEvents.ts` 从“Codex方法名集合”改为“规范化事件集合”。
- 对 capabilities 做 UI gating（model/review/skills/apps/login/collab）。
- 登录文案与入口 provider-aware（不再固定 “Sign in to Codex”）。

交付标准：

- 前端可在三个 provider 下稳定工作。
- 不支持能力有明确提示与降级行为。

### Phase 6: Daemon 与远程模式对齐

主要改动：

- `src-tauri/src/bin/codex_monitor_daemon.rs` 同步 provider 抽象与路由。
- remote RPC 保持兼容，新增 provider 相关参数与返回字段。

交付标准：

- local/remote 两种 backend mode 行为一致。

### Phase 7: 硬化与发布

主要改动：

- 错误分级与用户提示完善。
- 性能优化（流式事件高频更新、线程列表分页、缓存策略）。
- 文档与迁移说明。

交付标准：

- lint/test/typecheck/cargo check 全部通过。
- 回归用例稳定。

## 8. 文件级改动清单（第一批）

后端核心：

- `src-tauri/src/types.rs`
- `src-tauri/src/state.rs`
- `src-tauri/src/shared/workspaces_core.rs`
- `src-tauri/src/workspaces/commands.rs`
- `src-tauri/src/lib.rs`
- `src-tauri/src/bin/codex_monitor_daemon.rs`
- `src-tauri/src/backend/*`（会话实现下沉到 providers）
- `src-tauri/src/shared/codex_core.rs`（拆分/重命名为通用 session core）
- 新增 `src-tauri/src/providers/*`

前端核心：

- `src/types.ts`
- `src/services/tauri.ts`
- `src/utils/appServerEvents.ts`
- `src/features/app/hooks/useAppServerEvents.ts`
- `src/features/settings/components/SettingsView.tsx`
- `src/features/app/components/Sidebar.tsx`
- `src/features/workspaces/hooks/useWorkspaces.ts`
- `src/features/threads/hooks/*`（按 capability 降级）

配套：

- `README.md`
- `docs/app-server-events.md`
- 新增 provider 迁移文档

## 9. 兼容性与迁移

### 9.1 配置迁移

- 读取旧字段时自动补默认 `provider=codex`。
- 写回时保留旧字段，避免老版本客户端读失败。

### 9.2 数据迁移

- `workspaces.json` 增量迁移，不破坏 existing workspace。
- 新增 provider session 索引文件，按 provider namespace 分离。

## 10. 测试计划

### 10.1 Rust

- 单元测试：
  - provider registry 分发
  - capability gating
  - session index 读写与恢复
  - 各 adapter 的事件解析
- 集成测试：
  - connect/start/send/interrupt/list/resume 基本流程
  - app 与 daemon 一致性

### 10.2 Frontend

- hooks 测试：
  - `useAppServerEvents` 规范化事件路由
  - settings provider 切换与配置保存
  - threads/actions 在不同 capability 下行为
- 关键组件测试：
  - SettingsView provider 区域
  - Sidebar 登录与状态展示

### 10.3 手工回归

- 三 provider 各跑一次完整闭环：
  - 新建 workspace -> connect -> start thread -> send -> interrupt -> resume -> list
- Local/Remote 双模式回归。

## 11. 风险与缓解

1. provider 协议能力不对齐  
- 缓解：capability-first 设计 + 本地会话索引兜底。

2. 事件语义差异导致 UI 状态错乱  
- 缓解：统一 `NormalizedEvent`，adapter 内做强校验与回退。

3. app/daemon 双实现漂移  
- 缓解：shared core only，daemon 仅 transport/wiring。

4. 旧配置兼容性问题  
- 缓解：读旧写新、保留旧字段、迁移测试覆盖。

## 12. 里程碑与工期建议

按 1 名熟悉代码的工程师估算：

1. Phase 0-2（骨架 + Codex 抽离）：5-8 天  
2. Phase 3（Claude MVP）：4-7 天  
3. Phase 4（Gemini MVP）：4-7 天  
4. Phase 5-6（前端 provider 化 + daemon 对齐）：4-6 天  
5. Phase 7（硬化与发布）：3-5 天  

总计：20-33 天（取决于 Claude/Gemini CLI 的协议稳定性与可用能力）。

## 13. 验收标准

满足以下即视为“多 provider 会话支持”达标：

- 三 provider 可在同一版本中创建并使用会话。
- 每个 provider 至少完成消息闭环（start/send/stream/complete/list/resume）。
- 不支持能力在 UI 中有明确降级，不出现不可恢复错误。
- 现有 Codex 体验无明显回退。
- local + remote 模式均可用。

## 14. 下一步执行顺序（建议）

1. 先完成 Phase 0-2（确保 Codex 零回归）。  
2. 先接 Claude，再接 Gemini（减少并行复杂度）。  
3. MVP 完成后再补 review/skills/apps/login 等高阶能力。
