# Optimization Plan

本文件记录当前 `main` 分支后续优化计划。已经完成的内容不再作为待办项保留，只在“已完成基线”里简短说明，避免后续继续围绕旧问题打转。

注意：仓库里仍有一个历史误拼文件 `OPTMIZATION_PLAN.md`，当前有效文件是本文件 `OPTIMIZATION_PLAN.md`。误拼文件不应再作为计划来源。

## 当前基线

这些事情已经完成，后续优化应以它们为前提：

- ZGent 后端已从 `main` 移除，并保留在独立 `zgent` 分支。
- `main` 只保留 `agent_frame` 后端，旧配置中的 `zgent` 会被升级/兼容到 `agent_frame`。
- `server.rs` 已完成第一轮拆分：
  - command routing 已下沉到 `agent_host/src/server/command_routing.rs`
  - command handlers 已下沉到 `agent_host/src/server/commands.rs`
  - incoming dispatch 已下沉到 `agent_host/src/server/incoming.rs`
  - foreground turn execution 已下沉到 `agent_host/src/server/foreground.rs`
  - frame config 构建已下沉到 `agent_host/src/server/frame_config.rs`
  - extra tool schema/handler 构建已下沉到 `agent_host/src/server/extra_tools.rs`
- slash command 已有独立控制通道，未知 `/command[@bot]` 不应再漏进用户上下文。
- conversation 级 remote workpath 已真正传入 `agent_frame`：
  - 每个 remote host 只允许一个 workpath
  - `remote="<host>"` 默认使用该 host 的 workpath 作为远端 cwd
  - 没有 workpath 时，remote file tools 只接受远端绝对路径
  - `exec_wait/exec_observe/exec_kill` 继续通过 `exec_id` 继承 remote
- interactive progress 已从 SessionState 低频更新改为 host/frame 间 progress API。
- token estimation 已支持模板、tokenizer、HuggingFace 缓存、bubblewrap cache mount，不应退回粗估。
- system prompt 已使用组件 hash 控制刷新，避免动态内容频繁破坏 prompt cache。
- `agent_frame` remote/SSH/workpath/cwd helper 已下沉到 `agent_frame/src/tooling/remote.rs`。
- `agent_frame` 通用 JSON 参数读取 helper 已下沉到 `agent_frame/src/tooling/args.rs`。
- `agent_frame` file tools 已下沉到 `agent_frame/src/tooling/fs.rs`。

## 保护性约束

后续重构必须保护这些近期重要行为：

- 不要把 remote 执行重新变成模型手写 `ssh host '...'` 的主路径。
- 不要让 local workspace path 被当成 remote cwd。
- 不要把 `exec_wait/observe/kill` 的 remote 参数重新暴露给模型；它们应通过 `exec_id` 推断。
- 不要让 slash command 进入 LLM 用户上下文。
- 不要把 progress 重新塞回 SessionState 作为唯一反馈机制。
- 不要删除 prompt hash 层；它是 prompt cache 成本控制的一部分。
- 不要删除 config/workdir upgrade 链；runtime 可以假设升级后的最新形态，但旧数据入口仍应由 loader/upgrade 处理。

## 下一阶段优先级

| 优先级 | 主题 | 目标 | 风险 |
| --- | --- | --- | --- |
| P0 | 收敛 `server.rs` facade | 让 `server.rs` 只保留装配、启动、对外入口 | 涉及引用多，需小步验证 |
| P0 | 继续拆分 `agent_frame/src/tooling.rs` | 下沉 exec/download/media/skill/runtime_state | 大文件移动，易引入 import 漏项 |
| P1 | Session transcript 命名和结构 | 区分用户可见 history 与 LLM transcript | 涉及持久化 schema，需要 upgrade |
| P1 | Config runtime 形态收敛 | loader 负责兼容，runtime 少背 legacy 分支 | 需要确认所有旧 loader 覆盖完整 |
| P2 | Prompt component 强类型化 | 用 enum/struct 替代字符串 key | 低风险但收益偏维护性 |
| P2 | Progress event 模型整理 | 合并 phase/progress/checkpoint 的概念边界 | 易影响 Telegram 体验 |

## 目标架构

```mermaid
flowchart TB
    Channel[Channel\nTelegram / Dingtalk / CLI]
    Incoming[IncomingDispatcher\ncommand lane + user lane]
    Commands[CommandRouter + CommandHandlers]
    Server[Server facade\nstartup + wiring]
    Runtime[RuntimeContext\nshared managers/config]
    Turn[TurnCoordinator\nforeground/background turn]
    Conversation[ConversationManager]
    Session[SessionManager]
    Workspace[WorkspaceManager]
    Frame[AgentFrame]
    Tools[Tool Registry]
    Workers[Tool Workers]

    Channel --> Incoming
    Incoming --> Commands
    Incoming --> Turn
    Server --> Runtime
    Runtime --> Conversation
    Runtime --> Session
    Runtime --> Workspace
    Turn --> Frame
    Frame --> Tools
    Tools --> Workers
```

目标不是减少所有类型，而是让每个类型只有一个清楚责任：

- `Server`: 服务装配、channel 生命周期、manager wiring。
- `IncomingDispatcher`: 把消息分成 command lane、user turn lane、ignored lane。
- `CommandHandlers`: 只处理 out-of-band control command，不写入 LLM transcript。
- `TurnCoordinator`: 负责构建 prompt/config、调用 backend、落盘 SessionState、发送最终消息。
- `RuntimeContext`: 提供共享依赖，避免函数参数到处传十几个 manager/config。
- `agent_frame::tooling::*`: 按工具族拆分，remote/exec/file 逻辑不再挤在一个文件里。

## P0. 收敛 `server.rs`

### 现状

第一轮拆分已经把 command routing、command handlers、incoming dispatch、foreground turn、frame config、extra tools 移出去了。剩余问题是 `ServerRuntime` 仍然像万能对象一样被各模块捕获和调用：

- 管理 conversation/workspace/session 快捷访问
- command helper
- prompt/model/sandbox/runtime 配置选择
- status/admin 辅助逻辑
- 若干测试仍直接依赖 `server.rs` 内部函数

### 下一步

1. 引入 `RuntimeContext` 或 `ServerState`

```rust
pub(crate) struct RuntimeContext {
    pub workdir: PathBuf,
    pub agent_workspace: AgentWorkspace,
    pub models: BTreeMap<String, ModelConfig>,
    pub main_agent: MainAgentConfig,
    pub command_catalog: CommandCatalog,
    pub conversations: Arc<Mutex<ConversationManager>>,
    pub sessions: Arc<Mutex<SessionManager>>,
    pub workspaces: WorkspaceManager,
    pub snapshots: Arc<Mutex<SnapshotManager>>,
    pub registry: AgentRegistry,
}
```

目标：

- `Server` 持有 `Arc<RuntimeContext>`
- command/turn/subagent/background 模块都拿 context，而不是继续让 `Server` 当万能对象
- 先不改变外部行为，只做参数和字段搬迁

2. 清理测试入口

把测试所需的小 helper 移到对应模块，避免所有测试都依赖 `server.rs` 私有函数。

### 完成标准

- `server.rs` 行数明显下降，只保留 server lifecycle 和 module wiring。
- 新增 feature 时不需要在 `server.rs` 同时改 5 个区域。
- `cargo test --manifest-path agent_host/Cargo.toml --lib` 通过。

## P0. 拆分 `agent_frame/src/tooling.rs`

### 现状

`remote.rs`、`args.rs`、`fs.rs` 已经拆出，但 `tooling.rs` 仍然混合了：

- `Tool` 类型和 schema 渲染
- exec metadata/runtime
- media tools
- download tools
- skill tools
- runtime cleanup/status summary
- 大量单测

继续留在一个文件会让 exec、media、download、skill 的改动互相干扰。

### 目标拆分

```text
agent_frame/src/tooling/
  mod.rs              # Tool, ToolExecutionMode, registry assembly
  exec.rs             # exec_start/wait/observe/kill, ProcessMetadata
  download.rs         # file_download_start/progress/wait/cancel
  media.rs            # image/audio/pdf/image generation tools
  skills.rs           # load_skill/skill_create/skill_update
  runtime_state.rs    # active_runtime_state_summary, terminate_runtime_state_tasks
```

### 顺序

1. 抽 `exec.rs`

包括：

- `ProcessMetadata`
- `spawn_managed_process`
- `read_process_snapshot`
- `wait_for_managed_process`
- exec follow-up tools
- `max_output_chars_arg`
- `ExecTimeoutAction`

2. 抽 `download.rs`

包括：

- download metadata
- download worker start/progress/wait/cancel
- active download cleanup

3. 抽 `media.rs`

包括：

- image/audio/pdf load
- image generation start/progress/wait/cancel
- multimodal marker payload helper

4. 抽 `skills.rs`

包括：

- skill load
- skill create/update/persist
- skill markdown validation 的工具入口 glue

5. 抽 `runtime_state.rs`

包括：

- `active_runtime_state_summary`
- `terminate_runtime_state_tasks`
- 统一 runtime 退出时对 exec/download/image worker 的清理

### 完成标准

- `tooling.rs` 或 `tooling/mod.rs` 只负责 registry assembly。
- `cargo test --manifest-path agent_frame/Cargo.toml --lib tooling::` 通过。

## P1. Session transcript 结构收敛

### 现状

Session 里有两套历史：

- `history: Vec<SessionMessage>`：用户可见/渠道语义历史。
- `session_state.messages: Vec<ChatMessage>`：LLM transcript。

它们都必要，但命名接近，且 `SessionCheckpointData` 仍有历史兼容痕迹，容易误用。

### 计划

1. 改名并升级 workdir schema：

```rust
struct Session {
    visible_history: Vec<SessionMessage>,
    runtime_state: DurableSessionState,
}

struct DurableSessionState {
    transcript: TranscriptState,
    turn: TurnState,
    prompt: PromptState,
    progress: Option<ProgressMessageState>,
}

struct TranscriptState {
    stable: Vec<ChatMessage>,
    pending: Vec<ChatMessage>,
}
```

2. 增加 upgrade：

- `history` -> `visible_history`
- `session_state.messages` -> `session_state.transcript.stable`
- `session_state.pending_messages` -> `session_state.transcript.pending`
- 清理旧 alias，如 `agent_messages`

3. 更新 snapshot/export/import。

### 完成标准

- 代码里看到 `history` 时不会再猜它是 UI 历史还是 LLM transcript。
- workdir upgrade 测试覆盖旧字段迁移。
- `VERSION` 按 workdir schema policy bump patch。

## P1. Config runtime 形态收敛

### 现状

项目已有 versioned config loaders，但 runtime 仍有一些兼容/legacy 概念残留。ZGent 移出主分支后，这块可以继续瘦身。

### 计划

1. 检查 runtime 中所有 `legacy`, `alias`, `deprecated`, `zgent` 相关分支。
2. 能放进 config loader 的兼容逻辑，迁移到 `agent_host/src/config/v0_x.rs`。
3. latest runtime struct 只表达当前支持形态。
4. TUI config editor 同步只显示当前支持字段。

### 完成标准

- `ModelConfig` 不再出现已删除 backend 的 runtime 分支。
- 旧配置仍可 load 并写回 latest。
- config tests 覆盖升级入口。

## P2. Prompt component 强类型化

### 现状

prompt component hash 已经解决 prompt cache 频繁失效的问题，但 component key 还是字符串。

### 计划

```rust
enum PromptComponentKind {
    StaticPolicy,
    ModelCatalog,
    UserProfile,
    IdentityProfile,
    WorkspaceSummary,
    RuntimeContext,
    RemoteWorkpaths,
    PartclawMemory,
}

struct PromptComponent {
    kind: PromptComponentKind,
    content: String,
    notice: Option<String>,
    refresh_policy: RefreshPolicy,
}
```

好处：

- 避免 key typo。
- 可以在测试中枚举所有 component。
- 更容易表达哪些变化只发 notice，哪些必须重组 system prompt。

## P2. Progress event 整理

### 现状

progress 已经独立于 SessionState，但 AgentFrame event、Host progress renderer、Telegram draft/edit message 三者概念还可以更清楚。

### 计划

统一为：

```rust
enum RuntimeProgressEvent {
    Thinking,
    Compacting,
    ToolBatchStarted(Vec<ToolSummary>),
    ToolBatchFinished,
    Completed,
    Failed(String),
}

struct ToolSummary {
    name: String,
    short_args: String,
}
```

原则：

- 不实时刷每个工具的 completed/running 状态。
- 只在 phase 变化或 tool batch 变化时更新。
- Telegram progress message 绑定 session，重启后可清理。

## 不做事项

- 不把 managers 合并成一个巨大 manager。
- 不把 `ConversationManager` 和 `SessionManager` 合并。
- 不把 `Tool` 和 `ToolWorkerJob` 合并。
- 不恢复 ZGent 到 `main`。
- 不让 remote execution 回退成 prompt 里鼓励手写 ssh。
- 不为了减少类型数删除 prompt hash、token estimation、progress API 这些成本/体验护栏。

## 推荐执行顺序

1. `agent_frame/src/tooling/exec.rs`: 抽 exec runtime/process metadata/tools。
2. `agent_frame/src/tooling/download.rs`: 抽 download worker tools。
3. `agent_frame/src/tooling/media.rs`: 抽 image/audio/pdf/image generation tools。
4. `agent_frame/src/tooling/skills.rs`: 抽 skill tools。
5. `agent_frame/src/tooling/runtime_state.rs`: 抽 runtime cleanup/status summary。
6. `server` 引入 `RuntimeContext` 或 `ServerState`，继续收敛万能 `ServerRuntime`。
7. Session transcript 命名和结构升级。

每一步都应该单独 commit，且至少运行：

```bash
cargo test --manifest-path agent_frame/Cargo.toml --lib
cargo test --manifest-path agent_host/Cargo.toml --lib
cargo check --manifest-path agent_frame/Cargo.toml
cargo check --manifest-path agent_host/Cargo.toml
git diff --check
```
