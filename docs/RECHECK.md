# Conversation Backend Recheck

本文件只记录本次后端结构梳理与复查结论，不修改实现代码。检查依据以当前代码为准；`ROAD_MAP.md` / `docs/conversation_new.md` 作为方向和历史背景参考。

## 1. Conversation 之下的后端结构

### 1.1 Host / Kernel

- `stellaclaw/src/conversation_host.rs`
  - `ConversationHostRuntime`：Host 侧 conversation registry，负责启动、重启、停止 `ConversationKernel`。
  - `HostedConversation`：保存 kernel handle、`channel/main` ingress sender、channel event subscriber 列表。
  - `spawn_channel_event_fanout`：单独线程把 `ChannelService` 输出事件广播给订阅者。

- `stellaclaw/src/conversation_new.rs`
  - `ConversationKernel`：单个 conversation 的串行 owner，负责 service manifest、runtime config、metadata、动态路由表、service lifecycle。
  - `ServiceAddr` / `ServiceCall` / `ServiceOutput`：内部 service-router 协议。
  - `ServiceKind` / `ServiceManifest`：conversation 级 service 持久化清单。
  - kernel reserved call：`CreateAgentSession`、`StopService`、`Query/UpdateRuntimeConfig`、`Query/UpdateMetadata`、`ListServices`。

### 1.2 Service Protocols

- `stellaclaw/src/service_protos/channel.rs`
  - 平台入口 `ChannelIngress`，包括消息、前台 session 查询/控制、workspace、terminal、runtime config、metadata。
  - channel 输出 `ChannelEvent`，由 Host/Web/Telegram 投影为平台事件。

- `stellaclaw/src/service_protos/agent_session.rs`
  - `AgentSessionRequest` / `AgentSessionEvent` / `AgentSessionResponse`。
  - 负责 new service protocol 与 core `SessionRequest` / `SessionEvent` 的中间形态。

- `stellaclaw/src/service_protos/{kernel,cron,memory,skill,tool_binary,workspace,terminal}.rs`
  - 各 service 的 typed payload 与 call builder。

### 1.3 Services

- `stellaclaw/src/services/channel.rs`
  - `ChannelService`：唯一接收平台 ingress 的 service；把消息转发给 foreground AgentSession，把 workspace/terminal 请求转发给对应 service，把 AgentSession/kernel/service response 转成 `ChannelEvent`。
  - 当前只有 `channel/main` 被标准启动。

- `stellaclaw/src/services/agent_session.rs`
  - `AgentSessionService`：foreground/background/subagent service runtime。
  - 启动真实 `agent_server`；没有 agent server path 时走 skeleton。
  - 维护轻量 state：lifecycle、active turn、last message、child agents、cron id counter。
  - 转换 core session event，处理 host bridge：memory、skill、tool_binary、cron、subagent、background agent。

- `stellaclaw/src/services/workspace.rs`
  - Workspace view / attachment materialization / local overlay / fixed SSH remote workspace。
  - Incoming `data:` `FileItem` 会 materialize 到 `.stellaclaw/attachments/incoming`，失败时保留 `FileItem` 并标记 crashed。

- `stellaclaw/src/services/terminal.rs` 与 `terminal_runtime.rs`
  - Terminal protocol service 与 PTY manager。
  - 支持 list/get/create/terminate/input/resize/replay/attach/detach，以及 fixed SSH mode runtime reset。

- `stellaclaw/src/services/cron.rs`
  - Cron task registry、schedule timer、manual trigger、background AgentSession creation、run status persistence。
  - Cron-owned background session event sink 为 `cron`，完成后按 output policy 可回投 foreground session。

- `stellaclaw/src/services/memory.rs`
  - Memory v1 backend wrapper；支持 user/public/conversation scope 的 search/write/update/delete/prompt context/maintain。

- `stellaclaw/src/services/skill.rs`
  - Runtime skill store 与 workspace `.stellaclaw/skill` 同步；支持 persist/list/load。

- `stellaclaw/src/services/tool_binary.rs`
  - Managed tool binary ensure protocol；复用全局 `ToolBinaryManager`。

### 1.4 Web / Platform Projection

- `stellaclaw/src/main.rs`
  - `run_conversation_event_bridge`：订阅每个 conversation 的 `channel/main` event，投影成旧 `channels::types::ChannelEvent`。
  - `project_channel_event`：把 `AgentSessionEvent`、`UserMessageQueued`、runtime config/metadata event 投影给 Web/Telegram。

- `stellaclaw/src/channels/web/channel.rs`
  - HTTP API：conversation CRUD、foreground session CRUD、message history/detail/send。
  - 通过 `ConversationHostRuntime::send_main_channel_ingress[_subscribed]` 与 `ChannelService` 通信。

- `stellaclaw/src/channels/web/main.rs`
  - Web home/chat websocket state machine；消费 `OutgoingMessageAppended` / `OutgoingSessionStream`。

- `stellaclaw/src/channels/web/workspace.rs` / `terminal.rs`
  - Web HTTP/WebSocket 到 WorkspaceService / TerminalService 的薄投影。

### 1.5 Core / Agent Server

- `stellaclaw/src/session_client.rs`
  - Host 侧 JSON-RPC client，负责启动 `agent_server`、initialize、发送 session notification、读取 session events。

- `agent_server/src/main.rs`
  - 独立 session 执行进程，初始化 `SessionActor`、`SessionRpcThread`、provider、tool catalog、local tool executor。

- `core/src/session_actor/session_rpc.rs`
  - Core side conversation bridge：将 session request 放进 mailbox，将 host bridge request 发回 Host，并等待 `ResolveHostCoordination`。

- `core/src/session_actor/actor.rs`
  - Session 执行状态机：control/data mailbox、turn loop、provider/tool loop、history、compaction、session view。

## 2. 已确认 BUG

### BUG-001: Conversation 重启后全局 outgoing event bridge 不会重新订阅

**入口和触发链路**

1. `ConversationHostRuntime::send_main_channel_ingress` / `send_main_channel_ingress_subscribed` 在旧 channel ingress closed 时会调用 `restart_conversation`，重新启动 kernel 和新的 channel event fanout：`stellaclaw/src/conversation_host.rs:153-180`、`231-257`。
2. `run_conversation_event_bridge` 用 `HashMap<conversation_id, Receiver<_>>` 缓存订阅，只在 `!subscriptions.contains_key(&conversation_id)` 时订阅：`stellaclaw/src/main.rs:397-427`。
3. 旧 kernel/fanout 停止后，旧 receiver 不再产生事件；但 map 里仍有这个 conversation id，所以 bridge 不会订阅新 kernel 的 event receiver。

**为什么是真的 BUG**

`project_channel_event` 是 Web chat websocket/home event 和 Telegram outgoing 的主投影路径：`stellaclaw/src/main.rs:454-684`。重启后 HTTP 的一次性 subscribed request 仍可能成功，但全局 outgoing loop 不再收到该 conversation 的 `MessageAppended` / stream / error 事件，表现为 WebSocket 和 Telegram 输出静默丢失。

### BUG-002: 发往不存在 foreground AgentSession 的无 request_id 调用会让 kernel 失败

**入口和触发链路**

1. Web 可以对 URL 中的任意 `foreground_session_id` 发消息、控制命令或删除请求：
   - `POST /foreground_sessions/{id}/messages`：`stellaclaw/src/channels/web/channel.rs:580-642`
   - `DELETE /foreground_sessions/{id}`：`stellaclaw/src/channels/web/channel.rs:375-408`
2. `ChannelService` 将这些 ingress 转成 `agent_session::*_call`，目标为 `agent/foreground/<id>`，但这些 command/notification call 没有 `request_id`：
   - incoming message：`stellaclaw/src/services/channel.rs:565-621`
   - status/context query、cancel/continue/compact/delete/host-coordination resolve 等 foreground control：`stellaclaw/src/services/channel.rs:623-734`
   - 对应 call builder 中 cancel/continue/compact/shutdown/resolve 等没有设置 `ServiceCall.request_id`：`stellaclaw/src/service_protos/agent_session.rs:404-485`
3. `ConversationKernel::dispatch_call` 只有在 `request_id` 存在且 source 还活着时才返回 `service_not_found` response；没有 `request_id` 时直接 `Err(unknown service target ...)`：`stellaclaw/src/conversation_new.rs:735-753`。
4. `ConversationKernel::handle_output` 收到这个 Err 会让 kernel 进入失败路径并 `stop_all`。

**为什么是真的 BUG**

该问题不依赖内部不可能发生的状态。Web API 路由本身接受任意 `{foreground_session_id}`；已删除、创建失败、并发 stale UI、手写 API 请求都能构造不存在的 foreground id。因为投递 call 没有 request_id，后续协议不会把错误转为 channel error，而是杀掉整个 conversation kernel。

同类次生问题：

- 对不存在 foreground 的 message history/detail query 有 request_id，kernel 会回 `KernelResponse::Error`，但 Web `wait_for_event` 只等待 `MessageHistory` / `MessageDetail`，最终返回 504 timeout，而不是明确的 404/400。
- Cron active run 完成后也可能向已删除 foreground 回投结果：`stellaclaw/src/services/cron.rs:601-610`。如果目标已不存在，同样会走无 request_id unknown target 路径。

### BUG-003: foreground session 创建失败会被 Web API 当作创建成功

**入口和触发链路**

1. Web `create_foreground_session` 发送 `ChannelIngress::CreateForegroundSession` 并等待 `AgentSessionCreated`：`stellaclaw/src/channels/web/channel.rs:311-327`。
2. `kernel::create_agent_session_call` 没有设置 `request_id`：`stellaclaw/src/service_protos/kernel.rs:115-129`。
3. 如果创建失败，例如 requested id 已存在，kernel 会通过 `reply_kernel_error` 返回 `KernelResponse::Error`：`stellaclaw/src/conversation_new.rs:1038-1048`。
4. Web 的 `wait_agent_session_created` 只匹配 `KernelChannelEvent::AgentSessionCreated`，忽略 error：`stellaclaw/src/channels/web/channel.rs:1084-1090`。
5. 等待超时后 Web fallback 到 requested/default id，并继续写 metadata、发布 home upsert、返回 HTTP 201：`stellaclaw/src/channels/web/channel.rs:325-345`。

**为什么是真的 BUG**

重复创建同一个 foreground session 或创建被 kernel policy 拒绝时，服务端实际没有 mount 新 AgentSession，但 HTTP API 会返回成功并更新 UI/metadata。后续对该 session 发消息又会触发 BUG-002 的 missing target 路径。

同一缺陷还会影响并发创建：多个 HTTP caller 订阅同一个 channel event fanout，但 create call 没有 request_id，`wait_agent_session_created` 无法确认收到的是自己这次创建的 response。

### BUG-004: 非 main foreground session 无法注册 cron task

**入口和触发链路**

1. Web 支持创建多个 foreground sessions，但它们都通过 `channel/main` 管理；创建 foreground 时 source 是 `channel/main`，session id 可以是 `scratch` 等非 main：`stellaclaw/src/channels/web/channel.rs:311-345`、`stellaclaw/src/services/channel.rs:674-680`。
2. `AgentSessionService::cron_bridge_call` 注册 cron task 时：
   - `registered_by = ctx.addr`，例如 `agent/foreground/scratch`
   - `channel_addr = state.binding.event_sink`，当前 Web 多 foreground 下是 `channel/main`
   - `foreground_session_addr = Some(ctx.addr)`：`stellaclaw/src/services/agent_session.rs:2035-2063`、`2455-2464`
3. `CronService::validate_task_registration` 要求 `foreground_session_addr` 的 id 必须等于 `channel_addr` 的 id，并要求 source foreground id 也等于 channel id：`stellaclaw/src/services/cron.rs:801-832`。

**为什么是真的 BUG**

对 `agent/foreground/scratch` 来说，foreground id 是 `scratch`，channel id 是 `main`，所以注册必然被拒绝。这个不是理论分支：Web 当前确实只启动 `channel/main`，同时又暴露多 foreground session API。因此非 main foreground session 内调用 cron 工具无法成功。

### BUG-005: Bridge 工具参数解析错误会杀掉 AgentSessionService / ConversationKernel

**入口和触发链路**

1. Core tool catalog 的 `execute_bridge_tool` 只要求 tool arguments 是 JSON object，然后把整个 object 作为 `ConversationBridgeRequest.payload` 发给 conversation bridge；没有在 core 侧做字段级 schema 校验：`core/src/session_actor/tool_catalog/mod.rs:249-273`。
2. Session RPC thread 会把该 request 作为 `SessionEvent::HostCoordinationRequested` 发送给 Host：`core/src/session_actor/session_rpc.rs:469-479`。
3. `AgentSessionService::run` 收到 core event 后调用 `handle_core_session_event(...)?`；该错误会直接从 service run loop 返回：`stellaclaw/src/services/agent_session.rs:146-171`。
4. `handle_core_session_event` 对多个 bridge action 调用解析函数时使用 `?` 直接传播错误，包括 memory、skill、tool_binary、cron、child agent、subagent control：`stellaclaw/src/services/agent_session.rs:967-1095`。
5. 这些解析函数会对 provider/model 给出的 payload 做 `serde_json::from_value(...)?`，并对空 task/description、非法 schedule/scope 等业务输入直接 `Err`：
   - memory：`stellaclaw/src/services/agent_session.rs:1911-1965`
   - skill：`stellaclaw/src/services/agent_session.rs:1968-1997`
   - tool_binary：`stellaclaw/src/services/agent_session.rs:2000-2015`
   - cron：`stellaclaw/src/services/agent_session.rs:2018-2085`
   - child agent start / subagent control：`stellaclaw/src/services/agent_session.rs:2104-2245`
6. service thread 中 `service.run(ctx)` 返回 Err 后会发出 `ServiceOutput::Failed`：`stellaclaw/src/conversation_new.rs:656-664`；kernel 收到任意 service failure 后返回 Err：`stellaclaw/src/conversation_new.rs:1008-1019`。

**为什么是真的 BUG**

Provider/model 生成格式错误或缺字段的 tool arguments 是真实可发生输入；core 当前只过滤“不是 object”的情况，不保证 `skill_name`、`task`、`id`、`schedule` 等字段一定存在且合法。因此一次坏的 bridge tool call 本应变成该 tool 的结构化失败结果，却会让 `AgentSessionService` 失败，并进一步把整个 conversation kernel 带入失败路径。

主要修复边界应在工具执行层：每个 bridge tool 在发出 `ConversationBridgeRequest` 前应完成字段级合法性检查，缺字段、类型错误、空字符串、枚举不合法、成组字段不完整等都应立即返回该工具的错误结果，而不是把坏 payload 发送给 Host。Host bridge payload 属于内部协议；如果内部坏 payload 仍然到达 Host，应该记录协议异常日志，但不把 Host 解析层作为用户输入合法性防线。

**修复记录**

已在 `core/src/session_actor/tool_catalog/mod.rs` 的 bridge tool 执行入口增加字段级校验，覆盖 required、type、enum、unknown argument、关键字符串为空和 `cron_task_update` 时间字段成组校验；校验失败会返回 `LocalToolError::InvalidArguments`，不会发出 `ConversationBridgeRequest`。Host 侧 `stellaclaw/src/services/agent_session.rs` 保持内部协议失败路径，但对仍然到达的坏 bridge payload 写入 `agent_session_bad_bridge_payload` warn log。

### BUG-006: Web / Telegram `/model` 可选择非 chat 模型并导致 AgentSessionService / ConversationKernel 失败

**入口和触发链路**

1. 全局配置允许 `models` 中同时存在 chat 模型和非 chat 工具模型；校验只要求 `available_agent_models` 中的 alias 是 chat-capable，并要求至少有一个 chat-capable model：`stellaclaw/src/config/mod.rs:300-330`。
2. Web control command `/model <alias>` 只检查 `config.models.contains_key(argument)`，没有检查目标 model 是否支持 `ModelCapability::Chat`：`stellaclaw/src/channels/web/control.rs:36-50`。Telegram `/model <alias>` 先解析成 `ConversationControl::SwitchModel`，再走同一个 `control_to_channel_ingress`，也只检查 alias 是否存在：`stellaclaw/src/channels/telegram.rs:979-999`，`stellaclaw/src/main.rs:342-356`。
3. Web `post_message` 对 control command 走 fire-and-forget：发送 `ChannelIngress::UpdateRuntimeConfig` 后直接返回 HTTP 202；Telegram incoming control 也会被转换成同样的 runtime config patch：`stellaclaw/src/channels/web/channel.rs:579-599`，`stellaclaw/src/main.rs:318-356`。
4. `ChannelService` 将 runtime config patch 转成 kernel `UpdateRuntimeConfig` call：`stellaclaw/src/services/channel.rs:736-748`。
5. kernel 持久化 runtime config 后广播到所有 AgentSession / Workspace / Terminal：`stellaclaw/src/conversation_new.rs:1089-1101`、`1213-1257`。
6. `AgentSessionService::restart_runner_for_launch` 会调用 `start_real_session`；后者通过 `resolve_session_model` 检查 main model 必须 chat-capable。若 `/model` 选中的是 web_search / image 等非 chat alias，`resolve_session_model` 返回 Err：`stellaclaw/src/services/agent_session.rs:501-535`、`582-590`、`687-707`。
7. 这个 Err 会从 service run loop 传播；service thread 发 `ServiceOutput::Failed`，kernel 收到后进入失败路径：`stellaclaw/src/conversation_new.rs:656-664`、`1008-1019`。

**为什么是真的 BUG**

非 chat 模型 alias 在配置中是合法存在的，例如 search/image/tooling model。Web / Telegram `/model` 当前没有限定到 `available_agent_models` 或 chat-capable 集合，所以用户只要输入一个合法但非 chat 的 alias，就会把 conversation runtime config 写成不可启动状态，并在 AgentSession 空闲立即重启或当前 turn 结束应用 pending launch 时触发 service/kernel failure。后续协议没有把该错误转为用户可见的 control command 失败。

### BUG-007: Cron run 完成/失败后不会停止 cron-owned background AgentSession，导致服务泄漏

**入口和触发链路**

1. Cron 触发任务时为每次 run 创建一个新的 `agent/background/cron_<task>_<run_id>`，binding 的 `event_sink` 是 `cron`：`stellaclaw/src/services/cron.rs:491-538`。
2. kernel 创建 AgentSession 后会 mount service 并写入 manifest：`stellaclaw/src/conversation_new.rs:1260-1293`、`620-685`。
3. Cron-owned background AgentSession 的事件会通过 `emit_session_event` 回到 CronService，而不是回到某个父 AgentSession：`stellaclaw/src/services/agent_session.rs:3200-3212`。
4. CronService 在 `TurnCompleted` 时从 `active_runs` 移除 run、更新 task 状态、按策略把结果回投 foreground，并持久化 cron state：`stellaclaw/src/services/cron.rs:572-614`。
5. CronService 在失败事件时也只是从 `active_runs` 移除 run 并记录失败：`stellaclaw/src/services/cron.rs:615-687`、`690-727`。
6. 上述完成/失败路径都没有对该 background AgentSession 发送 `Shutdown` / `StopService`，所以 service 仍留在 kernel service map 和 manifest 中。

**为什么是真的 BUG**

定时任务的 run 是一次性执行语义；`TurnCompleted` / `TurnFailed` 后 Cron 已经认为 run 结束并清掉 `active_runs`，但对应 background AgentSession 没有停止。下一次 cron run 会用新的 run id 创建新的 background service，旧 service 继续 idle 并保留在 manifest，长期运行会导致 conversation 下 background services 不断累积。这个不依赖模型必须调用 `terminate`；一次普通的完成 turn 就会触发。

### BUG-008: AgentSession 真实 runtime 启动/重启失败会让整个 ConversationKernel 失败

**入口和触发链路**

1. `AgentSessionService::run` 顶层直接调用 `start_real_session(&launch, &self.kind)?`；如果配置了 `agent_server_path`，spawn、model resolve、tool model resolve、`client.initialize` 任何一步失败都会返回 Err：`stellaclaw/src/services/agent_session.rs:87-98`、`582-666`。
2. runtime config 更新时，`restart_runner_for_launch` 也会先 shutdown 当前 runner，然后用 `start_real_session(launch, kind)?` 启动新 runner：`stellaclaw/src/services/agent_session.rs:501-535`。
3. 上述 Err 会直接从 service run loop 返回；service wrapper 将其转为 `ServiceOutput::Failed`：`stellaclaw/src/conversation_new.rs:656-664`。
4. `ConversationKernel::handle_output` 收到任意 `ServiceOutput::Failed` 都返回 Err，kernel run loop 记录错误并 `stop_all`：`stellaclaw/src/conversation_new.rs:588-595`、`1008-1019`。
5. `AgentSessionEvent::RuntimeCrashed` 只覆盖 agent_server 内部 `SessionActor` 运行中出错的路径：`agent_server/src/main.rs:225-253`；它没有覆盖 agent_server spawn/initialize 或 runtime config restart 阶段的失败。

**为什么是真的 BUG**

`agent_server_path` 配错、二进制不可执行、provider 初始化失败、模型/tool model 配置不兼容、Web `/model` 选择非 chat 模型等都是真实运行时输入。单个 AgentSession 的 runtime 启动失败应该投影为该 session 的 `RuntimeCrashed` / `TurnFailed` 或 channel-visible error；当前会升级成 conversation 级 service failure，导致所有 service 停止。BUG-006 是该问题的一个具体 Web 触发入口。

同类次生问题：

- 如果 agent_server 在返回 initialize response 前写出 `server_error` 后退出，`AgentServerClient` reader 会忽略 `server_error` notification，EOF 时也不会 drain pending requests；`initialize` 调用可能等到 30s timeout 才返回：`stellaclaw/src/session_client.rs:127-168`、`226-267`，`agent_server/src/main.rs:23-34`。

### BUG-009: 非 main foreground RuntimeCrashed 错误会被 Web 投影到 main session

**入口和触发链路**

1. `ChannelEvent::SessionEvent` 中的 `AgentSessionEvent::RuntimeCrashed` 携带 `session_addr`，可以来自 `agent/foreground/scratch` 等非 main foreground session：`stellaclaw/src/main.rs:459-462`。
2. `project_channel_event` 在处理 `RuntimeCrashed` 时只生成 `ChannelEvent::Error(OutgoingError)`，没有像 `TurnFailed` 一样同时生成带 `session_id` 的 `OutgoingSessionStream`：`stellaclaw/src/main.rs:540-597`。
3. `OutgoingError` 类型不携带 foreground/session id；Web 的 `send_error` 固定把错误 publish 到 `WebSessionKey::new(&error.conversation_id, "main")`：`stellaclaw/src/channels/web/main.rs:348-361`。
4. Web chat session stream 正常按 `session_id` / foreground id 分发：`stellaclaw/src/channels/web/main.rs:456-554`，但 RuntimeCrashed 没有走这条带 session id 的投影路径。

**为什么是真的 BUG**

Web 当前支持多 foreground session。非 main foreground session 运行中发生 `RuntimeCrashed` 时，事件本身有正确 `session_addr`，但投影层丢失该信息，最终错误被推送到 main chat。当前正在查看 scratch 等 session 的用户不会收到对应 chat error，main session 用户反而会看到不属于 main 的 runtime error。

### BUG-010: fixed SSH remote workspace archive 下载绕过 50MB 响应上限

**入口和触发链路**

1. WorkspaceService 明确定义了 archive 下载上限 `MAX_DOWNLOAD_BYTES = 50 * 1024 * 1024`：`stellaclaw/src/services/workspace.rs:32-33`。
2. `download_workspace_archive` 对 local overlay / local workspace 会先构造 `archive_data`，随后检查 `archive_data.len() > MAX_DOWNLOAD_BYTES` 并返回错误：`stellaclaw/src/services/workspace.rs:771-803`。
3. 但 fixed SSH remote 分支在收到 remote helper payload 后直接 `return serde_json::from_value(payload)`，提前绕过同一个大小检查：`stellaclaw/src/services/workspace.rs:776-785`。
4. remote helper 的 `download_archive` 会在远端内存中用 `tarfile` 构造完整 gzip archive，再把完整 archive base64 放进 JSON response，没有大小检查：`stellaclaw/src/services/workspace.rs:1402-1422`。
5. Web `/workspace/download` 会等待 `WorkspaceResponse::ArchiveDownloaded`，再把 base64 解码成完整 HTTP `application/gzip` body 返回：`stellaclaw/src/channels/web/workspace.rs:105-129`。

**为什么是真的 BUG**

该链路不依赖不可能发生的状态：只要 conversation runtime config 使用 fixed SSH remote workspace，用户或前端请求下载一个足够大的 remote path，就会让远端 helper、WorkspaceService response JSON、Web 解码 body 都处理超出本地 50MB 限制的完整 archive。local 路径会被 `MAX_DOWNLOAD_BYTES` 拦住，remote 路径不会，因此这是真实的协议不一致和资源耗尽风险。

### BUG-011: 带 `data:` 附件的消息会被后续普通消息插队，导致历史顺序反转

**入口和触发链路**

1. `ChannelService` 收到 `IncomingMessage` 后会先投影 `UserMessageQueued`，然后检查 `message_needs_materialization`：`stellaclaw/src/services/channel.rs:565-583`。
2. 如果消息里有 `data:` `FileItem`，ChannelService 发起 `WorkspaceRequest::MaterializeMessage`，把该消息放入 `pending_workspace`，随后立即 `return Ok(())`，没有阻塞后续 ingress：`stellaclaw/src/services/channel.rs:583-599`。
3. 后续不需要 materialization 的普通文本消息会直接走 `agent_session::enqueue_message_call` 进入 AgentSession：`stellaclaw/src/services/channel.rs:601-621`。
4. 当前一个附件消息 materialize 完成后，`handle_workspace_response` 才把它 enqueue 到同一个 foreground AgentSession：`stellaclaw/src/services/channel.rs:900-938`。
5. AgentSession 收到 enqueue 后按收到顺序发 `UserMessageStarted`，core append 后再发 `MessageAppended` / `UserMessageCommitted`：`stellaclaw/src/services/agent_session.rs:202-232`、`1118-1131`。

**为什么是真的 BUG**

用户连续发送“附件消息 A”再发送“普通文本消息 B”时，A 会等待 WorkspaceService materialize，B 会直接进入 AgentSession。因此 B 可以先写入 `all_messages`，A 后写入，最终持久化历史、Web message index、provider 上下文顺序都变成 B 在 A 前。这个不依赖异常状态，只依赖一个常见的 `data:` 附件消息和紧随其后的普通消息。

### BUG-012: `data:` 附件写入失败会让 WorkspaceService / ConversationKernel 失败

**入口和触发链路**

1. `WorkspaceService` 处理 `MaterializeMessage` / `MaterializeFiles` 时直接对 `materialize_message` / `materialize_files` 使用 `?`：`stellaclaw/src/services/workspace.rs:78-88`。
2. `materialize_file` 对 `data:` URI 解码失败会返回带 `FileState::Crashed` 的 `FileItem`，但创建附件目录或写入附件文件失败会通过 `?` 传播 Err：`stellaclaw/src/services/workspace.rs:301-339`。
3. service wrapper 会把 `service.run(ctx)` 的 Err 转成 `ServiceOutput::Failed`：`stellaclaw/src/conversation_new.rs:656-664`。
4. `ConversationKernel::handle_output` 收到任意 `ServiceOutput::Failed` 都返回 Err，并进入 kernel fatal 停止路径：`stellaclaw/src/conversation_new.rs:1008-1019`。

**为什么是真的 BUG**

用户上传 `data:` 附件时，附件目录不可写、磁盘满、路径权限异常等都是真实运行失败路径。按照 `FileItem` 约定，文件不可用时应保留 `FileItem` 并标记 `FileState::Crashed`；当前只有 data URI 解析错误被降级为 crashed file，文件系统写入失败会升级成 WorkspaceService failure，并进一步停止整个 conversation kernel。

### BUG-013: background agent 调用 `terminate` 可能把 response 投递给已停止的自己并触发 kernel failure

**入口和触发链路**

1. `terminate` 是 main background agent 可用的 host bridge 工具：`core/src/session_actor/tool_catalog/host_tools.rs:126-132`。
2. `terminate_bridge_response` 在 background agent 内收到 `terminate` 后，先向 parent 发送 `Terminated` child event，然后发送一个 `agent_session::shutdown_call`，source 和 target 都是当前 background AgentSession 自己：`stellaclaw/src/services/agent_session.rs:1306-1348`。
3. AgentSession 的 `Shutdown` handler 会先向 `call.source` 回复 `AgentSessionResponse::Stopped`，再发送 `ServiceOutput::Stopped` 并退出 service run loop：`stellaclaw/src/services/agent_session.rs:445-460`。
4. 对 self-shutdown 来说，`call.source == ctx.addr`。因此这个 response 的 target 也是即将停止的 background AgentSession 自己。
5. kernel 处理该 response 时会调用 `dispatch_call`，向 target service inbox 发送 call；如果该 AgentSession 已经退出，`inbox_tx.send` 返回 `service inbox closed`，该 Err 会从 `handle_output` 传播为 kernel failure：`stellaclaw/src/conversation_new.rs:735-758`、`1008-1019`。

**为什么是真的 BUG**

这不是纯竞态推测。background agent 调用公开的 `terminate` 工具时必然构造 source=target=self 的 shutdown call；shutdown handler 又必然构造一个发回 self 的 response。服务随后立即停止，kernel 后续处理这个 self-response 时要么把 response 投递到无人消费的 inbox，要么在 receiver 已关闭时直接 `service inbox closed` 并停止整个 kernel。无论哪种结果，terminate 的协议语义都不应产生投递给已停止自身的 response。

### BUG-014: 不同 AgentSession 创建 cron task 会使用同一 `cron_0001` id 并互相覆盖

**入口和触发链路**

1. cron host tools 会暴露给 main foreground 和 main background agent：`core/src/session_actor/tool_catalog/host_tools.rs:17-29`、`136-163`。
2. `AgentSessionService::cron_bridge_call` 创建 cron task 时，用当前 AgentSession 自己的 `state.next_cron_index` 生成 `task_id = cron_0001`、`cron_0002` 等：`stellaclaw/src/services/agent_session.rs:2035-2056`。
3. `next_cron_index` 是每个 AgentSession runtime state 独立维护的字段，新 session 从 1 开始：`stellaclaw/src/services/agent_session.rs:3280-3308`。
4. `CronService` 收到 `RegisterTask` 后只校验 task_id 非空、schedule、channel/foreground owner 形态，没有检查同名 task 是否已存在或是否属于同一 owner：`stellaclaw/src/services/cron.rs:70-93`、`836-873`。
5. 注册成功时直接 `tasks.insert(task.task_id.clone(), ScheduledCronTask::new(task, now))`，`HashMap` 会覆盖同 key 的旧任务：`stellaclaw/src/services/cron.rs:94-106`。

**为什么是真的 BUG**

一个 foreground agent 创建第一条 cron task 会得到 `cron_0001`；之后它启动的 background agent 也有 cron tools，且自己的 `next_cron_index` 同样从 1 开始，创建第一条 cron task 时也会注册 `cron_0001`。CronService 的 task namespace 是 conversation 级全局 `HashMap<String, ScheduledCronTask>`，所以第二个任务会静默覆盖第一个任务，而不是拒绝冲突或按 owner 分 namespace。后续 foreground agent 按 owner list/get/update/remove 时会发现自己的任务消失，且旧任务 schedule/payload 被另一个 agent 替换。

### BUG-015: 非创建类 Web API 会隐式创建未知 conversation

**入口和触发链路**

1. Web 已有显式创建 conversation 的入口，`create_conversation` 会通过 `ConversationMetadataStore::load_or_create` 生成 metadata，并带上 `channel_id` / `platform_chat_id`：`stellaclaw/src/channels/web/channel.rs:236-266`。
2. 但多个非创建接口也直接调用 `conversation_runtime.ensure_conversation_started(conversation_id)`，例如 list messages、post message/control、runtime config query：`stellaclaw/src/channels/web/channel.rs:414-430`、`575-632`、`744-760`。
3. `ConversationHostRuntime::ensure_conversation_started` 不检查 metadata 是否已存在，会调用 `ConversationKernel::open_or_bootstrap`：`stellaclaw/src/conversation_host.rs:81-145`。
4. `open_or_bootstrap` 在 metadata 不存在时会 `persist_metadata()`，而 `ConversationKernel::new` 生成的 metadata 使用当前 conversation id，但 `channel_id` / `platform_chat_id` 是空字符串：`stellaclaw/src/conversation_new.rs:477-478`、`518-545`。
5. `persist_metadata` 会创建 `services/<conversation_id>/conversation_metadata.json`：`stellaclaw/src/conversation_new.rs:889-897`、`935-941`。后续 `start_existing` / conversation list 会把这个 metadata 当成真实已有 conversation。

**为什么是真的 BUG**

对一个不存在或已删除的 conversation id 发 `GET /messages`、`POST /messages`、`GET /runtime_config` 等非创建请求，本应返回 404 或明确错误；当前会创建一个 metadata 不完整的新 conversation，并 mount 标准 services。这个不依赖异常内部状态，手写 URL、旧 UI stale id、删除后的重试请求都能触发。因为 metadata 里 channel/platform id 为空，后续 home 列表、event projection 和平台路由都会看到一个并非通过创建协议产生的 conversation。

### BUG-016: AgentSession RuntimeCrashed 后仍接收新消息并把消息静默丢给已退出 actor

**入口和触发链路**

1. agent_server 的 actor loop 在 `actor.recv_step()` 返回 Err 时会发送 `SessionEvent::RuntimeCrashed`，随后 `break` 退出 actor thread：`agent_server/src/main.rs:229-247`。
2. 这个路径不会关闭 agent_server 的 JSON-RPC 主循环，也不会关闭 `SessionRpcThread`；`AgentRuntime` 仍保留 `rpc_thread`，agent_server 继续接受 `session_request` notification：`agent_server/src/main.rs:71-86`、`251-267`。
3. `AgentSessionService` 收到 core `RuntimeCrashed` 只在 `apply_core_session_event` 中把轻量 state 标成 `Crashed`，但没有 `runner.take()`、shutdown runner，也没有让后续 ingress 被拒绝或重启：`stellaclaw/src/services/agent_session.rs:2947-2958`。
4. 后续 Web/Channel 仍可向同一 foreground session 发送新消息；`AgentSessionService` 的 `EnqueueMessage` 分支只判断 `runner.as_mut()`，不检查 `state.state == Crashed`，因此会先投影 `UserMessageStarted`，再调用 `runner.send(...)`：`stellaclaw/src/services/agent_session.rs:202-232`。
5. `runner.send` 通过 JSON-RPC notification 把 `SessionRequest` 发给 agent_server；agent_server 的 `runtime.send` 只要求请求能进入 `SessionRpcThread`，而 `SessionRpcThread::handle_conversation_request` 对已经关闭的 actor mailbox append 结果直接 `let _ = mailbox.append(...)` 忽略：`agent_server/src/main.rs:71-86`、`core/src/session_actor/session_rpc.rs:493-506`。
6. 如果不是 actor 内部 crash，而是 agent_server stdout/event stream 断开，AgentSessionService 也只是把 state 标成 `Crashed` 并发 `RuntimeCrashed`，同样没有 `runner.take()`；后续 `runner.send(...)` 对关闭 stdin 写入失败时会通过 `?` 传播出 service run loop：`stellaclaw/src/services/agent_session.rs:176-198`、`564-568`。

**为什么是真的 BUG**

`RuntimeCrashed` 是当前协议明确存在的 session-level failure 事件，不是“不可能发生”的状态。发生后 actor thread 已经退出，后续消息不会被 `SessionActor` 处理；但 AgentSessionService 仍向前端发出用户消息已开始的事件，并且 JSON-RPC/SessionRpcThread 会把投递失败吞掉。若 agent_server 进程已经退出，后续写入还可能从 `runner.send` 返回 Err，进而升级成 AgentSessionService failure / kernel failure。结果是用户在 crashed session 继续发送消息时，要么消息没有 `MessageAppended` / `TurnFailed` 的后续闭环，要么把 session crash 二次放大成 conversation 级失败。

### BUG-017: Web conversation id 未校验，`DELETE /api/conversations/..` 可删除整个 workdir

**入口和触发链路**

1. Web HTTP 路由用 `split_path` 直接按 `/` 分段，不对 path segment 做 conversation id 形态校验；`..` 是一个合法 segment，会匹配 `["api","conversations",conversation_id]`：`stellaclaw/src/channels/web/http.rs:167-172`，`stellaclaw/src/channels/web/channel.rs:104-113`。
2. `delete_conversation` 对该 `conversation_id` 先尝试 stop host registry，再调用 `ConversationMetadataStore::remove(conversation_id)`：`stellaclaw/src/channels/web/channel.rs:285-293`。
3. `WorkdirLayout::conversation_root(conversation_id)` 是 `workdir/conversations/<conversation_id>`，没有拒绝 `..`；当 conversation_id 是 `..` 时，路径解析为 `workdir/conversations/..`，也就是整个 workdir：`stellaclaw/src/conversation_metadata.rs:17-33`。
4. `ConversationMetadataStore::remove` 看到 `conversation_root.exists()` 后直接 `fs::remove_dir_all(&conversation_root)`：`stellaclaw/src/conversation_metadata.rs:161-173`。

**为什么是真的 BUG**

该入口不依赖 URL percent decode 或内部异常状态：HTTP path `/api/conversations/..` 的第三段就是字符串 `..`，会正常命中 delete conversation 路由。后续协议没有任何 id validation 或 canonical boundary check，`remove_dir_all(workdir/conversations/..)` 会删除 workdir 本身，包含 `conversations/`、`services/`、`rundir/`、日志和持久化状态。其它以 conversation id 拼路径的入口也会受到同类 id 形态风险影响，delete 是最直接、破坏性最大的可触发链路。

### BUG-018: foreground session `session_id` 未校验，可通过 `/` / `..` 创建越界 service storage 目录

**入口和触发链路**

1. Web `POST /api/conversations/{conversation_id}/foreground_sessions` 从 JSON body 读取 `session_id`，没有做字符集或路径分隔符校验：`stellaclaw/src/channels/web/channel.rs:312-327`、`1050-1054`。
2. ChannelService 把该值作为 `ChannelIngress::CreateForegroundSession { requested_id }` 转给 kernel：`stellaclaw/src/services/channel.rs:674-681`。
3. `ConversationKernel::create_agent_session` 直接调用 `ServiceAddr::agent_foreground_id(id)`，把用户传入的 id 放进 `ServiceAddr.path`：`stellaclaw/src/conversation_new.rs:1263-1294`。
4. `ServiceAddr::storage_component()` 用 `self.path.join("__")` 生成 storage component，但不会转义 path segment 内部的 `/` 或 `..`：`stellaclaw/src/conversation_new.rs:145-154`。
5. `mount_service_instance` 会在启动 service 前创建 `self.service_storage(&addr)`；`service_storage` 直接 `.join(addr.storage_component())`：`stellaclaw/src/conversation_new.rs:637-645`、`948-955`。

**为什么是真的 BUG**

该链路不依赖 URL path 对 `/` 的限制，因为入口是 JSON body。请求体中的 `{"session_id":"../../../../tmp/pwn"}` 会进入 storage component `local__agent__foreground__../../../../tmp/pwn`；`PathBuf::join` 会把其中的 `/..` 当作真实路径组件解析，导致 service storage 目录越过 `services/<conversation_id>/` 边界。core 的 `SessionStateStore` 和 `SessionActorLogger` 会再次 sanitize `session_id`，但那只保护 core log/state 路径；kernel 在启动 core 之前已经用未校验 id 创建了 service storage，并把该地址写入 manifest/metadata。

### BUG-019: Workspace `ReadFile` 未设置默认/最大读取上限，可一次性读完整大文件进响应

**入口和触发链路**

1. Web `GET /api/conversations/{id}/workspace/file?path=...` 把 query 中的 `limit_bytes` 原样解析成 `Option<usize>`；如果调用方不传该参数就是 `None`：`stellaclaw/src/channels/web/workspace.rs:40-53`。
2. `WorkspaceService::read_workspace_file` 把这个 `None` 传给本地或 remote 读取实现：`stellaclaw/src/services/workspace.rs:489-511`。
3. 本地 `read_local_workspace_file` 在 `limit_bytes == None` 时调用 `file.read_to_end(&mut bytes)`，没有使用 `MAX_DOWNLOAD_BYTES` 或其它响应上限：`stellaclaw/src/services/workspace.rs:970-1018`。
4. fixed SSH remote helper 在没有 `limit_bytes` 时执行 `handle.read()`，把完整文件读入内存，再 UTF-8 或 base64 放入 JSON response：`stellaclaw/src/services/workspace.rs:1320-1349`。

**为什么是真的 BUG**

这是 Web 暴露的普通读取接口，不依赖异常状态。请求者只要省略 `limit_bytes` 并读取一个很大的 local/remote workspace 文件，WorkspaceService 就会把完整文件读入内存，remote 模式还会额外经过 SSH stdout JSON 和 base64 膨胀；Web 再把整个 `WorkspaceResponse::File` JSON 返回给客户端。协议里已有 `offset`、`limit_bytes`、`returned_bytes`、`truncated` 字段，说明分块读取是预期能力；但当前没有默认 limit 或最大 limit，导致一次请求可绕过 archive 下载已有的 50MB 响应边界。

### BUG-020: Cron 重启后丢失 active run 关联，恢复的 background run 结果会被忽略并可重复触发

**入口和触发链路**

1. Cron 触发任务后会创建 background AgentSession，并把 run 关联记录在内存态 `pending_runs` / `active_runs`，同时把 task 的 `last_run_status` 持久化为 `Running`：`stellaclaw/src/services/cron.rs:430-512`、`560-578`。
2. `pending_runs` / `active_runs` 没有写入 `tasks.json`；持久化文件只保存 task registration、失败计数、last status/result 和 `next_run_id`：`stellaclaw/src/services/cron.rs:462-489`、`956-1027`。
3. Conversation 重启时，kernel 会从 manifest 重新 mount 所有 service，包括上次 run 创建但尚未停止的 background AgentSession：`stellaclaw/src/conversation_new.rs:518-544`。
4. CronService 重启时只调用 `load_tasks`，新建的 `pending_runs` / `active_runs` 为空：`stellaclaw/src/services/cron.rs:31-36`、`989-1006`。
5. 恢复出来的 background AgentSession 后续如果发出 `TurnCompleted` / `TurnFailed` / `RuntimeCrashed`，`handle_agent_session_event` 会先查 `active_runs.remove(&session_addr)`；查不到就直接 `return Ok(())`，不会更新 task 状态、不会 forward 结果，也不会清理 run：`stellaclaw/src/services/cron.rs:572-666`。
6. 下一次 timer 触发时，`task_run_in_progress` 只看当前内存态 pending/active map；由于重启后为空，它会认为该 task 没有运行中实例，并再次创建新的 background run：`stellaclaw/src/services/cron.rs:356-390`、`560-570`。

**为什么是真的 BUG**

这是服务重启/进程恢复的真实路径。active run 的 background service 已经写入 manifest，会随 conversation 恢复；但 CronService 没有恢复 run ownership map，导致旧 run 的结果丢失，`last_run_status` 可能长期停在 `Running`，同时下一次调度仍能启动新 run。该问题和 BUG-007 不完全相同：BUG-007 是 run 正常完成后不停止已知 background service；这里是重启后 Cron 已经不认识仍在 manifest 中的 run，因此完成/失败事件和重复触发保护都会失效。

### BUG-021: Skill persist 会跟随 staged skill 内的文件 symlink，复制 workspace 外内容到 runtime skill store / 同步目标

**入口和触发链路**

1. Core 暴露 `skill_create` / `skill_update` bridge tool，AgentSession 只从 tool payload 解析 `skill_name` 并转成 `SkillRequest::Persist`：`stellaclaw/src/services/agent_session.rs:1968-1998`。
2. SkillService 会先调用 `validate_skill_name`，该校验能拒绝 `../`、`/`、空白等名称穿越；随后把 staged skill 路径固定为当前 workspace 的 `.stellaclaw/skill/<skill_name>`：`stellaclaw/src/services/skill.rs:149-187`，`stellaclaw/src/services/skill_sync.rs:410-429`。
3. `validate_skill_directory` 只校验 staged 目录存在、`SKILL.md` frontmatter name/description 匹配，不遍历检查目录内 symlink：`stellaclaw/src/services/skill_sync.rs:431-455`。
4. `copy_skill_atomically` 使用 `copy_directory_recursive_local` 复制 staged skill 到 runtime skill store；递归复制时用 `entry.file_type().is_dir()` 分支，非目录统一调用 `fs::copy(&source_path, &destination_path)`：`stellaclaw/src/services/skill_sync.rs:530-550`、`620-664`。
5. Rust `fs::copy` 对文件 symlink 会复制其目标文件内容，而不是把 symlink 作为安全边界拒绝。之后 `sync_skill_to_conversation_workspaces` 还会把同一 staged skill 同步到所有已有 `.stellaclaw/skill` workspace：`stellaclaw/src/services/skill_sync.rs:576-617`。

**为什么是真的 BUG**

该链路不依赖非法 `skill_name`，所以不会被当前名称校验挡住。只要 staged skill 目录内含有一个指向 workspace 外文件的 symlink，例如 `.stellaclaw/skill/demo/secret.txt -> /path/outside/workspace/secret.txt`，`skill_create` / `skill_update` 就会在复制 skill 时把目标文件内容 materialize 到 runtime skill store，并可能进一步同步到其它 conversation workspace 或配置的 skill sync 目标。后续协议没有再检查 copied entry 是否仍在 workspace 边界内，因此这是可触发的复制边界 BUG。

### BUG-022: Web / Telegram `/remote` 未校验 host，Terminal fixed SSH 创建会把 host 当作 `ssh` option 解析

**入口和触发链路**

1. Web message 中的 `/remote <host> <path>` 只用 `split_whitespace` 拆出两个 token，不调用 `validate_remote_binding` / `validate_remote_host`；随后直接把 host 写入 `ToolRemoteMode::FixedSsh` patch：`stellaclaw/src/channels/web/control.rs:72-88`。
2. Telegram `/remote <host> <path>` 也只按空白拆分 host/path，不校验 host；通用 control 转换同样直接生成 `KernelRuntimeConfigPatch { tool_remote_mode: FixedSsh { host, cwd } }`：`stellaclaw/src/channels/telegram.rs:979-1023`，`stellaclaw/src/main.rs:364-372`。
3. Web `post_message` 对 control command 只 `send_main_channel_ingress` 并立即返回 202，不等待 runtime config 是否能被后续服务验证：`stellaclaw/src/channels/web/channel.rs:572-599`。
4. TerminalService 收到 `UpdateRuntimeConfig` 后只替换本地 config 并 reset stale terminals，不验证 fixed SSH host：`stellaclaw/src/services/terminal.rs:93-105`。
5. 后续 Web `POST /api/conversations/{id}/terminals` 会触发 `TerminalRequest::Create`：`stellaclaw/src/channels/web/terminal.rs:42-45`。
6. Terminal runtime 在 FixedSsh 分支直接构造 `ssh -tt <host> -- <remote_command>`；`host` 被放在 `--` 之前，若 host 以 `-` 开头会被 OpenSSH 当作 option 解析：`stellaclaw/src/services/terminal_runtime.rs:724-731`。

**为什么是真的 BUG**

Workspace 和 core tool runtime 确实有 host 校验：Workspace 只允许 ASCII 字母、数字、`.`、`_`、`-`，core 还额外拒绝 `-`、shell metachar、slash 等：`stellaclaw/src/workspace.rs:182-199`，`core/src/session_actor/tool_runtime.rs:332-356`。但 Terminal create 不经过这两处校验，Web / Telegram `/remote` 的 unsafe host 会直接进入 TerminalRuntime。攻击者可以先发送 `/remote -oProxyCommand=... /tmp` 这类无空白的 host token，再创建 terminal；因为参数位于 `ssh` 的 option 区，后续 `--` 不能保护已经被解析的 host token。这是 control -> kernel config -> Terminal create 的完整可触发链路。

### BUG-023: Workspace archive upload 只限制压缩包大小，不限制解压后总写入量

**入口和触发链路**

1. HTTP request body 有 32MB 上限；`/workspace/upload` 会把整个 body base64 包进 `WorkspaceRequest::UploadArchive`：`stellaclaw/src/channels/web/http.rs:11`、`125-132`，`stellaclaw/src/channels/web/workspace.rs:87-103`。
2. WorkspaceService 解码 archive 后，本地路径调用 `upload_local_workspace_archive`；remote 路径把同一个压缩包再发给 fixed SSH helper：`stellaclaw/src/services/workspace.rs:682-703`。
3. 本地 `upload_local_workspace_archive` 用 `GzDecoder` + `tar::Archive` 遍历 entry，只跳过绝对路径、`..`、symlink、hardlink，然后直接 `entry.unpack_in(&target_dir)`，没有统计 entry size 或总解压字节数：`stellaclaw/src/services/workspace.rs:707-743`。
4. remote helper 同样只跳过绝对路径、parent traversal、symlink、hardlink，然后调用 `archive.extract(member, target)`，也没有解压后字节上限：`stellaclaw/src/services/workspace.rs:1388-1401`。

**为什么是真的 BUG**

这是普通 Web upload API，调用方只需要提交一个小于 32MB 的 gzip tar。gzip 压缩比可以让压缩包远小于展开后的文件总量；后续 local/remote unpack 都没有把 `MAX_UPLOAD_BYTES` / `MAX_DOWNLOAD_BYTES` 或任何新上限应用到解压后的普通文件内容，因此一次请求可以写出远超 HTTP body limit 的数据，造成 workspace / remote 磁盘耗尽或长时间解压占用。已有 path/symlink/hardlink 检查不能阻止该资源消耗链路。

### BUG-024: `subagent_join.timeout_seconds` 极大值会触发 AgentSessionService panic

**入口和触发链路**

1. Core tool catalog 暴露 `subagent_join`，`timeout_seconds` schema 只是 `{"type":"number"}`，没有 maximum 或 finite 约束：`core/src/session_actor/tool_catalog/host_tools.rs:102-112`。
2. AgentSessionService 从 provider bridge payload 反序列化 `LegacySubagentJoinPayload { timeout_seconds: Option<f64> }`：`stellaclaw/src/services/agent_session.rs:2212-2215`、`2669-2674`。
3. 如果目标 subagent 正在 running，且 `timeout_seconds > 0.0`，代码直接执行 `Instant::now() + Duration::from_secs_f64(timeout_seconds)`，没有检查 `is_finite()` 或上限：`stellaclaw/src/services/agent_session.rs:2216-2225`。
4. `Duration::from_secs_f64` 对超出 `Duration` 范围的 finite f64 会 panic。service thread panic 后不会走 `service.run(ctx)` 的 `Err` 分支；kernel 仍保留该 service sender，后续 dispatch 到它会得到 `service inbox closed` 并升级为 kernel error：`stellaclaw/src/conversation_new.rs:656-663`、`748-760`、`1376-1382`。

**为什么是真的 BUG**

该问题依赖“目标 subagent 正在 running”，但这不是不可能前置条件：同一工具集先调用 `subagent_start` 即可创建 running subagent，再调用 `subagent_join` 并传入类似 `1e308` 的 `timeout_seconds`。这不是普通 join timeout 失败，而是服务线程 panic；后续消息、join/cancel 或其它发给该 AgentSessionService 的 call 会遇到 closed inbox，可能进一步拉垮 conversation kernel。

### BUG-025: Web `FileItem.file://` 未做边界校验，后续 provider media normalizer 可读取任意本地文件

**入口和触发链路**

1. Web `post_message` 直接把请求体里的 `files: Vec<FileItem>` 放入 `ChatMessageItem::File`，没有限制 URI scheme 或校验 `file://` 路径是否属于 conversation workspace / attachments：`stellaclaw/src/channels/web/channel.rs:602-623`。
2. ChannelService 只在 `message_needs_materialization` 检测到 `data:` URI 时才走 WorkspaceService materialization；非 `data:` 的 `file://` 会直接 enqueue 到 AgentSession：`stellaclaw/src/services/channel.rs:565-621`、`1019-1026`。
3. 即使走到 WorkspaceService，`materialize_file` 对非 `data:` URI 也是 `return Ok(file)`，不会做 boundary check 或将外部 file URI materialize 到附件目录：`stellaclaw/src/services/workspace.rs:301-304`。
4. core 在给 provider 构造请求前会调用 `normalize_messages_for_model`。对用户侧 image / pdf / audio `FileItem`，如果模型支持对应输入，它会进入 `read_file_bytes`，对 `file://` 直接 `fs::read(local_file_path(file))`：`core/src/session_actor/media_normalizer.rs:17-63`、`104-148`、`198-218`、`254-270`。

**为什么是真的 BUG**

该链路不依赖 provider 自行伪造内容，而是 Web API 请求者可以直接提交例如 `{"uri":"file:///Users/.../secret.pdf","media_type":"application/pdf"}` 或任意本地图片路径。只要当前模型支持对应 media input，normalizer 会读取该绝对路径并把内容内联到 provider 请求；路径不需要位于 conversation workspace，也不需要经过 attachment materialization。读取失败只会变成 crashed-file prompt，但对存在且类型签名有效的文件，内容会被发送给外部 provider。这违反了 FileItem 约定里“Web 直接发送的 FileItem 应该已经是 durable/retrievable file”的信任边界：服务端没有验证这个前提。

### BUG-026: Provider 返回的 assistant image `file://` 可在下一轮被 Codex image history replay 读取本地图片

**入口和触发链路**

1. Codex subscription provider 解析 Responses 输出时，对 assistant `message.content[]` 中的 `image_url` / `output_image` / `input_image` 调用 `append_image_reference`：`core/src/providers/codex_subscription.rs:2315-2350`、`2725-2742`。
2. 如果 reference 不是 `data:`，也不像裸 base64，代码直接持久化为 `ChatMessageItem::File(FileItem { uri: reference, media_type: Some("image/*"), state: None })`，没有限制 scheme 或拒绝 `file://`：`core/src/providers/codex_subscription.rs:2744-2767`。
3. 后续构造下一轮 Codex subscription 请求时，assistant 历史里的 image File 会进入 `append_assistant_response_items`。只要模型支持 `ImageIn`，代码不会只把 URI 当文本输出，而是调用 `append_assistant_image_visual_context`：`core/src/providers/codex_subscription.rs:2488-2504`。
4. `append_assistant_image_visual_context` 会构造一个临时 `ChatRole::User` fake message，把该 assistant `FileItem` 放进去，再调用 `normalize_messages_for_model`：`core/src/providers/codex_subscription.rs:2548-2568`。
5. 对 user-side image 且 inline transport 的 normalizer 会调用 `normalize_image_inline -> read_file_bytes`；`read_file_bytes` 对 `file://` 直接 `fs::read(local_file_path(file))`，没有 conversation/workspace boundary check：`core/src/session_actor/media_normalizer.rs:147-188`、`254-270`。

**为什么是真的 BUG**

这条链路的前置不是“用户上传了 file URI”，而是 provider 响应中包含了一个 assistant image reference，例如 `file:///Users/.../secret.png`。当前解析层会把它作为普通 assistant-generated image 历史保存；下一轮请求为了保留视觉上下文，会把 assistant image replay 成临时 user image，并由共享 media normalizer 读取本地路径。如果该路径存在且是可解码图片，内容会被转成 `data:image/...;base64,...` 发回 provider。provider 返回非 data URL 本可以是远端 URL 或 provider 文件引用，但 `file://` 不应跨过 provider-response 信任边界并触发本地文件读取。

### BUG-027: Workspace List 的 `limit` 只限制返回结果，仍会完整枚举和排序超大目录

**入口和触发链路**

1. Web `/workspace/list` 直接把 query string 中的 `limit` 解析为 `WorkspaceRequest::List.limit`；未传时 service 默认使用 200：`stellaclaw/src/channels/web/workspace.rs:16-31`，`stellaclaw/src/services/workspace.rs:104-115`。
2. `list_workspace` 对 local workspace / local overlay 调用 `list_local_workspace`，对 fixed SSH remote workspace 把 `limit.max(1)` 传给 remote helper：`stellaclaw/src/services/workspace.rs:458-489`。
3. 本地 `list_local_workspace` 会先 `fs::read_dir` 遍历目录下所有 entry，对每个 entry 调 `fs::symlink_metadata` 并 push 到 `entries`，之后对完整 `entries` 排序，最后才计算 `effective_limit` 并 `entries.truncate(effective_limit)`：`stellaclaw/src/services/workspace.rs:908-957`。
4. fixed SSH remote helper 也会先 `entries = [entry(child, ...) for child in target.iterdir()]` 构造完整列表、排序、计算 total，然后才执行 `entries = entries[:limit]`：`stellaclaw/src/services/workspace.rs:1296-1317`。

**为什么是真的 BUG**

这不是“客户端传了很大的 limit 才会变大”的问题；即使默认 `limit=200`，service 也会为了计算 `total_entries` 和排序而 stat 并缓存目录下全部条目。Web 调用方只要对 workspace 或 fixed SSH remote 中的超大目录发起 list，请求就能让 WorkspaceService 长时间阻塞、分配大量内存，并在 remote 模式下让 SSH helper 构造巨大 JSON。`limit` 在后续协议里没有形成真正的资源上限，因此当前 API 给调用方展示的是 bounded list，实际执行却是 unbounded directory scan。

### BUG-028: Telegram 附件下载无大小上限，会把完整响应读入内存并写入 conversation 附件目录

**入口和触发链路**

1. Telegram incoming message 会把 photo/document/audio/voice/video/animation 转成附件描述，并在 `collect_incoming_files` 中为每个附件调用 `download_attachment`：`stellaclaw/src/channels/telegram.rs:397-420`、`840-858`。
2. 附件目标路径位于 `workdir/conversations/<conversation_id>/.stellaclaw/attachments/incoming`，下载失败才会降级成 crashed `FileItem`；正常路径会写入稳定 `file://` URI：`stellaclaw/src/channels/telegram.rs:407-438`。
3. `download_attachment` 调用 Telegram `getFile` 后只读取 `file_path`；`TelegramFile` 结构没有解析 `file_size`，也没有在下载前做大小判断：`stellaclaw/src/channels/telegram.rs:515-523`、`896-900`。
4. 实际下载使用 `reqwest` 的 `.bytes()`，会把整个响应体读进内存，然后 `fs::write(target, &bytes)` 一次性写盘；没有 content-length 检查、streaming limit、磁盘写入上限或附件总量上限：`stellaclaw/src/channels/telegram.rs:524-536`。
5. Telegram channel 的 `api_base_url` 是配置项，可以指向默认 `https://api.telegram.org` 以外的 Bot API endpoint；后端代码不能把外部服务的默认文件限制当作内部资源边界：`stellaclaw/src/config/mod.rs:264-274`。

**为什么是真的 BUG**

这是平台入口到 conversation 附件持久化的直接链路。只要 Telegram update 指向一个足够大的可下载文件，当前代码会先把完整文件加载到内存，再写入 conversation 附件目录；失败只在已经超时、OOM、连接中断或写盘错误之后才变成 crashed `FileItem`。由于 `api_base_url` 可配置，部署可以使用自建 Bot API 或兼容 endpoint，不能依赖官方服务当前可能存在的文件大小限制。该问题会造成 channel 线程内存峰值、磁盘占用和后续 provider media normalization 的资源消耗失控。

### BUG-029: Provider-backed media job 结果转换只保留第一个文件，多个生成/返回文件会从 ToolResultContent 丢失

**入口和触发链路**

1. provider-backed `image_generation` / media analysis 工具通过 `start_provider_job` 启动 provider 请求，等待完成后把 provider 返回的 `ChatMessage` 交给 `provider_message_to_tool_result` 转成工具结果：`core/src/session_actor/tool_catalog/media_tools.rs:345-365`、`390-446`。
2. Codex subscription Responses 解析 assistant `message.content[]` 时会遍历 content array；每个 `image_url` / `output_image` / `input_image` 都会调用 `append_image_reference`，并向同一个 `ChatMessage.data` 追加一个 `ChatMessageItem::File`：`core/src/providers/codex_subscription.rs:2331-2338`、`2738-2768`。
3. `provider_message_to_tool_result` 只维护 `let mut file = None`。遇到 `ChatMessageItem::File(item)` 时仅在 `file.is_none()` 时保留；遇到嵌套 `ToolResult` 也只取 `result.result.files.into_iter().next()`：`core/src/session_actor/tool_catalog/media_tools.rs:597-612`。
4. 最终结果只执行一次 `result.with_file(file)`，因此 `ToolResultContent.files` 最多包含一个文件：`core/src/session_actor/tool_catalog/media_tools.rs:616-624`。

**为什么是真的 BUG**

`ToolResultContent.files[]` 的数据模型允许多个文件，provider parser 也已经能把单个 assistant message 中的多个 image content items 表示为多个 `ChatMessageItem::File`。但 media job 的转换层把这个多文件结果压成一个文件，后续 history、Web stream、provider replay 都只能看到第一张图/第一个文件。这个问题不依赖不存在的 provider 行为：Responses content array 本身就是多 item 协议，当前解析代码也显式支持多次追加 image File；丢失发生在后续的统一工具结果转换阶段。

### BUG-030: 重命名不存在的 foreground session 会创建 ghost session metadata，并引导后续请求打到不存在 service

**入口和触发链路**

1. Web 路由允许对 URL 中任意 `foreground_session_id` 调用 `PATCH /api/conversations/{conversation_id}/foreground_sessions/{foreground_session_id}`：`stellaclaw/src/channels/web/channel.rs:121-128`。
2. `rename_foreground_session` 不查询 service 是否存在，只调用 `set_session_nickname` 写入 metadata，然后返回 200 并发布 home update：`stellaclaw/src/channels/web/channel.rs:350-370`。
3. `set_session_nickname` 只把 route id 转成 storage id，并通过 `KernelMetadataPatch.session_nicknames` 写入 conversation metadata；kernel `apply_metadata_patch` 同样只更新 map，不验证对应 `agent/foreground/<id>` service 是否存在：`stellaclaw/src/channels/web/channel.rs:656-668`，`stellaclaw/src/conversation_new.rs:1186-1208`。
4. Web conversation summary 会把 `metadata.session_nicknames.keys()` 全部转换成 foreground session summary；`query_message_summary` 失败会被 `unwrap_or_default()` 吞掉，因此不存在的 session 也会作为空会话出现在 UI/home snapshot：`stellaclaw/src/channels/web/channel.rs:817-842`。
5. 用户随后对这个 ghost session 发送消息时，ChannelService 会把消息投递到 `agent/foreground/<id>`；该 target 不存在且 enqueue call 没有 request id，后续进入已确认的 missing target fatal 路径：`stellaclaw/src/services/channel.rs:565-621`。

**为什么是真的 BUG**

这个问题不是单纯“未知 foreground id 会触发 BUG-002”，而是另一个合法 Web API 会把未知 id 写进持久化 metadata，并让前端和 home summary 把它当作真实 foreground session 展示。也就是说后续用户从 UI 点击/发送到该 ghost session 是服务端自己制造出来的状态，而不是只能靠手写恶意 URL。由于该 ghost session 没有对应 AgentSession service，下一条消息会走不存在 target 的无 request_id call，造成 conversation kernel failure 或至少消息不可达。

### BUG-031: Codex assistant image replay 会把 provider 返回的 `https://` 图片退化成错误文本，丢失视觉上下文

**入口和触发链路**

1. Codex subscription provider 解析 Responses assistant `message.content[]`，对 `image_url` / `output_image` / `input_image` 调用 `append_image_reference`；非 `data:`、非裸 base64 的 reference 会直接保存为 `FileItem { uri: reference, media_type: Some("image/*") }`：`core/src/providers/codex_subscription.rs:2315-2350`、`2738-2768`。
2. 因此 provider 返回普通远端图片 URL，例如 `https://.../image.png`，会作为 assistant image `FileItem` 进入持久化历史。这与 `responses_file_item` 对 image file 直接输出 `{"type":"input_image","image_url": file.uri}` 的能力并不矛盾：用户侧 image item 可以携带 URL：`core/src/providers/codex_subscription.rs:2624-2631`。
3. 但下一轮构造 assistant history 时，`append_assistant_response_items` 对 assistant image 不直接按 `image_url` replay，而是构造临时 user message 并调用 `append_assistant_image_visual_context`：`core/src/providers/codex_subscription.rs:2478-2506`、`2548-2572`。
4. 该 fake user message 进入共享 `normalize_messages_for_model`；当模型的 image input transport 不是 `FileReference` 时，会走 `normalize_image_inline -> read_file_bytes -> local_file_path`，而 `local_file_path` 只接受 `file://`，对 `https://` 返回错误：`core/src/session_actor/media_normalizer.rs:118-147`、`254-270`。
5. normalizer 的错误不会让请求失败，而是把 file item 替换成 `crashed_file_prompt` 文本；随后 `user_responses_content` 只能发送这段错误/文件引用文本，图片本身不再进入 provider 请求：`core/src/session_actor/media_normalizer.rs:45-62`，`core/src/providers/codex_subscription.rs:2565-2571`。

**为什么是真的 BUG**

这条链路与 BUG-026 的安全问题不同：`https://` 是 provider 返回图片引用的正常形态，也是当前 Codex translator 在用户侧 image input 中可以表达的形态。当前代码先把远端 URL 作为合法 assistant image 保存，下一轮却因为 assistant visual replay 复用 user-side inline normalizer，把它当成本地文件读取失败处理。结果不是显式不支持，而是 silently degrade 成文本错误，导致多轮图像生成/分析场景丢失上一轮生成图片的视觉上下文。

## 3. 检查过但暂不判定为 BUG

- Incoming `data:` attachment 的解码失败不会导致消息丢失或 kernel crash。`WorkspaceService::materialize_file` 会保留原 `FileItem` 并设置 `FileState::Crashed`。但附件目录创建/文件写入失败会传播为 service failure，已单独记录为 BUG-012。

- `subagent_join` pending 时没有在 `AgentSessionService` 的 `EnqueueMessage` 分支直接清空 pending join，但 core tool cancel path 会在 cancel token 触发时发送 `subagent_join_cancel` bridge request。因此“新用户消息 interrupt join”需要通过 core cancel 语义成立，不能单看 service 层 pending queue 就判定 BUG。

- Subagent/background child id 的路径边界本轮未新增 BUG。`subagent_start` / `background_agent_start` 的 service id 由 AgentSessionService 自增生成 `subagent_0001` / `background_0001`，provider 只提供 description/task，不直接提供 child service id：`stellaclaw/src/services/agent_session.rs:2109-2159`。已确认的 subagent 问题是 join timeout 数值未校验导致 panic，见 BUG-024。

- Workspace local symlink 边界存在安全风险迹象：local read/list/download 使用 `fs::metadata` / open 会跟随 symlink，而 conversation workspace 本身也有 `.stellaclaw/shared` 等 intentional symlink。由于 shared symlink 是产品能力的一部分，本次不把“跟随 symlink”直接判定为 BUG；需要先明确 Workspace API 的安全边界是“conversation root 物理目录”还是“允许已存在的共享 runtime symlink”。

- Terminal runtime config reset 路径检查后没有判定为 BUG。`TerminalManager::reset_stale_terminals` 会在 fixed SSH / local runtime target 变化后终止 stale terminals，后续 list/get/create 会基于当前 target 重新构造 terminal state；没有发现会把旧 SSH runtime 误复用到新 runtime config 的后续协议链路。

- Web metadata/runtime query、Workspace、Terminal 请求的 request correlation 暂未发现串线。Web 侧生成 request id，`ChannelIngress` 携带 request id，ChannelService 转发到目标 service 时设置 `ServiceCall.request_id`，响应投影回 `KernelChannelEvent` 后 Web 按同一个 id 过滤：`stellaclaw/src/services/channel.rs:750-813`、`875-1017`，`stellaclaw/src/channels/web/channel.rs:670-771`，`stellaclaw/src/channels/web/workspace.rs:150-175`，`stellaclaw/src/channels/web/terminal.rs:72-97`。

- Workspace / Memory / Skill / ToolBinary / Cron service 对“无法 decode 的内部 payload”有些路径会发 `ServiceOutput::Failed`，但这些 payload 正常来自同进程 typed `encode_request` / call builder，不是当前 Web/provider 用户输入能直接构造的协议面。本次只把 provider 可真实生成坏参数并穿透到 AgentSession bridge parser 的 BUG-005 记为已确认 BUG。

- Memory bridge 的正常后端失败本轮不新增 BUG。`MemoryService` 会把 storage/backend error 映射为 `MemoryResponse::Failure`，`memory_tool_payload` 再把它转成 bridge tool 的结构化 `{"status":"failure","reason":...}`：`stellaclaw/src/services/memory.rs:44-65`、`79-183`，`stellaclaw/src/services/agent_session.rs:1519-1549`、`2322-2362`。会导致 service failure 的仍是 bad internal payload decode；provider 侧坏参数在 AgentSession bridge parser 已归入 BUG-005。

- Memory store 的普通数据边界本轮未新增 BUG。`memory_id` 只通过 `u_` / `p_` / `c_` 前缀映射到固定 scope，不参与文件路径拼接；写入/更新文本限制为 1KB，active entry 每 scope 512 条，`entries.jsonl` 写入上限 2MB，search 结果也有数量和字节上限：`stellaclaw/src/memory.rs:26-60`、`1182-1189`、`1202-1268`。外部手工篡改 memory 文件导致 load/parse 失败不属于当前 Web/provider 协议可直接触发链路。

- Skill bridge 的 persist/load 业务失败本轮不新增 BUG。`SkillService` 对 `persist_skill` / `load_skill` 的校验、目录不存在、复制/删除失败等会返回 `SkillResponse::Failure`，AgentSession 会把该 Failure 回写成 host coordination tool result：`stellaclaw/src/services/skill.rs:50-67`、`78-201`，`stellaclaw/src/services/agent_session.rs:1551-1581`、`2364-2409`。启动 reconcile 失败会发 `ServiceOutput::Failed`，但通过当前 bridge 协议创建/更新 skill 时都会先 validate staged/runtime skill directory，未找到普通 provider 输入能把 runtime skill store 写成启动时必然 invalid 的后续链路。

- ToolBinary bridge 的 ensure 失败本轮不新增 BUG。unsupported tool、下载/安装失败、remote ensure 失败会被 `ToolBinaryService::ensure_tool` 捕获并编码成 `ToolBinaryResponse::Failure`，随后转换成 bridge tool 的结构化失败结果：`stellaclaw/src/services/tool_binary.rs:42-64`、`72-91`，`stellaclaw/src/services/agent_session.rs:1583-1614`、`2411-2434`。因此“managed binary 不可用”不会升级成 kernel failure；仍需修的是 BUG-005 中 tool_binary request payload parse error 直接 `?` 传播的问题。

- ToolBinary remote host 来源本轮未新增 BUG。`tool_binary_ensure` 不是直接暴露给 provider 的普通工具，而是 core 文件/进程工具在需要 managed binary 时发出的 bridge request；remote host 来自 core `ToolExecutionContext` 的 execution target，并会先经过 `validate_remote_host`：`core/src/session_actor/tool_binary.rs:34-67`，`core/src/session_actor/tool_runtime.rs:121-150`、`332-356`，`core/src/session_actor/tool_catalog/process_tools.rs:1934-1944`。Terminal fixed SSH host 没有复用这套校验的问题已记录为 BUG-022。

- Provider-backed media tool 的 `wait_timeout_seconds` 极大值本轮不单独判定为后端 BUG。`wait_media_job` 确实也把未 clamp 的 f64 传给 `Duration::from_secs_f64`：`core/src/session_actor/tool_catalog/media_tools.rs:149-196`、`475-486`，但该调用发生在 core local tool executor 的 operation thread 内；tool executor 会把 operation panic 转成 `"tool panicked"` 的工具错误结果，不会像 BUG-024 那样发生在 AgentSessionService 主循环并留下 closed service inbox：`core/src/session_actor/tool_executor.rs:299-306`、`462-479`、`502-516`。

- Cron interval schedule 的 `Duration::from_secs_f64` 本轮不新增 BUG。`CronSchedule::IntervalSeconds` 在 `next_wakeup_for_task` 内确实只校验 finite / positive 后就转换 Duration，理论上极大 finite 秒数仍可能 panic；但当前 provider 暴露的 `cron_task_create` / `cron_task_update` 只接受 cron field 字符串并构造 `CronExpression`，没有入口让 provider 生成 `IntervalSeconds`：`core/src/session_actor/tool_catalog/host_tools.rs:150-163`、`246-315`，`stellaclaw/src/services/agent_session.rs:2035-2075`、`2685-2788`。现有 `IntervalSeconds` 只来自内部测试/手工状态文件，不满足当前协议可触发条件。

- Workspace path / archive traversal 基础边界检查暂未新增 BUG。普通 path 入口会拒绝绝对路径和 `..`：`stellaclaw/src/services/workspace.rs:1079-1099`；archive upload 会跳过绝对路径、parent traversal、symlink 和 hardlink entry：`stellaclaw/src/services/workspace.rs:707-743`。保留的 symlink 跟随问题仍归入上面的产品边界待确认项；archive upload 解压后大小无上限已单独记录为 BUG-023。

- Workspace archive upload 通过“既有 symlink 目录”写出 conversation root 的风险本轮不单独判定为 BUG。`upload_local_workspace_archive` 的确只做 lexical/path-entry 检查，`entry.unpack_in(&target_dir)` 会受目标目录中既有 symlink 影响；但默认 workspace 明确创建 `.stellaclaw/shared` / `.stellaclaw/skill_memory` 指向 workdir runtime shared area，这是产品层共享能力，不等同于任意越界写。仅凭 Web upload 也不能创建 symlink entry，因为 archive symlink/hardlink 被跳过：`stellaclaw/src/workspace.rs:43-87`，`stellaclaw/src/services/workspace.rs:707-743`。是否禁止写入 shared symlink 需要先明确 Workspace API 对 `.stellaclaw/shared` 的产品边界。

- fixed SSH remote archive upload 的 symlink / hardlink 入口本轮未新增 BUG。remote helper 同样跳过绝对路径、parent traversal、`member.issym()` 和 `member.islnk()`：`stellaclaw/src/services/workspace.rs:1394-1400`。remote archive download 的大小上限绕过已单独记录为 BUG-010。

- Terminal HTTP/WebSocket 控制路径除 fixed SSH host 校验外暂未新增 BUG。create/input/resize/replay/attach/detach 的运行时错误会转成 `TerminalResponse::Error`：`stellaclaw/src/services/terminal.rs:83-221`、`253-273`；Web attach 会按 request id 等待 `Attached` 或显式 error，stream output 通过 terminal id + subscriber id 过滤：`stellaclaw/src/channels/web/terminal.rs:100-152`、`238-289`。Web / Telegram fixed SSH host 未统一校验已单独记录为 BUG-022。

- Terminal resource boundary 本轮未新增 BUG。TerminalManager 有每 conversation 8 个、全局 128 个 terminal 上限，输出历史通过 `MAX_OUTPUT_BUFFER_BYTES = 2MB` 截断，stream replay 按 chunk 发送；订阅 sender 被 terminal 退出或 detach 清理后，转发线程会从 receiver 断开并退出：`stellaclaw/src/services/terminal_runtime.rs:25-31`、`275-301`、`632-683`、`783-834`，`stellaclaw/src/services/terminal.rs:292-314`。

- Skill `skill_name` 本身的路径穿越本轮不单独判定为 BUG。`validate_skill_name` 只允许 ASCII 字母、数字、`_` 和 `-`，`skill_load` / `skill_create` / `skill_update` / `skill_delete` 都会进入 SkillService 后再校验：`stellaclaw/src/services/skill_sync.rs:410-429`，`stellaclaw/src/services/skill.rs:149-187`、`239-250`。已确认的问题不是名称穿越，而是 staged skill 内容复制时跟随 symlink，见 BUG-021。

- ChatMessage / FileItem 的 provider-neutral 持久化形态本轮未发现直接违背约定的写入。Web POST 中的 `FileItem` 会先进入 `ChannelIngress::IncomingMessage`，若 URI 是 `data:`，ChannelService 会转发给 WorkspaceService materialize 后再 enqueue 到 AgentSession；这个异步转发带来的顺序问题已记录为 BUG-011，写入失败升级问题已记录为 BUG-012。Telegram 附件会下载成稳定 `file://`，失败也保留 crashed `FileItem`：`stellaclaw/src/channels/telegram.rs:400-448`、`510-550`。Tool result helpers `from_text` / `from_json` 写入 `structured`，legacy context 也会在反序列化时迁移为 structured：`core/src/session_actor/chat_message.rs:492-546`。

- SelectionReference 的 `file_path` 本轮未新增 BUG。Web 可以直接提交 `SelectionReferenceItem`，但后续 provider / compressor / token estimator 都只调用 `selection.to_prompt_text()`，把 `file_path`、locator、selected text 和 context 当文本渲染；没有像 `FileItem.file://` 那样进入 media normalizer 读取本地路径：`stellaclaw/src/channels/web/channel.rs:602-623`，`core/src/session_actor/chat_message.rs:291-425`，`core/src/providers/openrouter_responses.rs:417-420`，`core/src/providers/codex_subscription.rs:2447-2450`。因此“selection file_path 指向任意本地文件会被读取”这条后续协议不成立。

- Telegram incoming attachment path 本轮未新增 BUG。Telegram conversation id 由 `ConversationIdManager` 生成，附件目录使用该 id；附件文件名会替换 `/`、`\`、`:` 等危险字符，下载失败时保留 crashed `FileItem` 而不是静默丢弃：`stellaclaw/src/conversation_id_manager.rs:31-50`，`stellaclaw/src/channels/telegram.rs:399-448`、`510-550`、`789-804`、`2631-2643`。Telegram `/remote` host 校验缺失已并入 BUG-022。

- `AgentServerClient::shutdown` 的阻塞风险检查后暂不判定为 BUG。虽然 host 侧 shutdown request 有 30s timeout 后还会 `child.wait()`：`stellaclaw/src/session_client.rs:99-109`，但 agent_server 收到 shutdown 后会给 actor mailbox 发送 `SessionRequest::Shutdown`，同时发送 `stop_tx` 并 join actor：`agent_server/src/main.rs:107-112`、`225-276`；actor loop 在 `recv_step` 外层检查 `stop_rx`，并且 `ProviderSession` active request 不会被 shutdown join 等待。因此没有找到“正常 provider 长请求必然卡死 shutdown”的后续协议链路。

- subagent/background child event 的 terminal shutdown 路径大体能闭合。parent 收到 child `TurnCompleted` / `TurnFailed` / `RuntimeCrashed` 后会更新 child record，并对 child 发送 shutdown；该 shutdown 的 source 是 parent、target 是 child，所以 `Stopped` response 回 parent，不会像 BUG-013 的 self-shutdown 那样回到已停止的 child 自己：`stellaclaw/src/services/agent_session.rs:1752-1905`。

- Cron trigger 的 pending create error correlation 本轮暂不单独判定为 BUG。`create_agent_session_with_binding_call` 没有 request id，`KernelResponse::Error` 只能用 `pending_order` FIFO 关联，但这些 create calls 都由 CronService 自己串行发出，kernel 也串行处理 service outputs；没有找到其它来源能向 CronService 混入同形态的 kernel create error 并打乱 FIFO 的实际链路。

- ConversationHost 的 per-request subscribed ingress 在 closed ingress 后会重新订阅新 fanout，本轮不判定为 BUG。`send_main_channel_ingress_subscribed` 在 send 失败后调用 `restart_conversation`，再重新取 `(sender, rx)` 并 retry：`stellaclaw/src/conversation_host.rs:231-255`。全局 bridge 不重新订阅的问题已单独记录为 BUG-001。

- Kernel service stop lifecycle 本轮未新增 BUG。主动 stop 会先从 `services` map 移除目标、发送 stop、join service，并在需要时持久化 manifest；service 自发 `ServiceOutput::Stopped` 也会触发 map 清理和 manifest 更新：`stellaclaw/src/conversation_new.rs:769-803`、`991-1006`。kernel stop agent 前给 Cron 发送 `DisableTasksForOwner`，Cron 对 kernel source 特意不发送 response，避免把 CronResponse 回投给 kernel 并被当作 bad kernel request：`stellaclaw/src/conversation_new.rs:769-779`、`stellaclaw/src/services/cron.rs:156-174`。

- 显式 `POST /api/conversations` 创建路径本轮未新增 BUG。该入口不接受客户端自带 conversation id，而是通过 `ConversationIdManager` 生成 `{channel_id}-000001` 形式，再持久化 metadata：`stellaclaw/src/channels/web/channel.rs:235-266`、`stellaclaw/src/conversation_id_manager.rs:31-50`。当前确认的 id 风险来自 URL path segment 和 foreground `session_id` body，分别记录为 BUG-017 / BUG-018。

- Web chat websocket 本轮未新增“删除后通过 socket 复活 conversation”的 BUG。chat websocket 只订阅并发送服务端事件，`websocket_event_loop` 不读取客户端消息；用户输入仍走 HTTP `POST /messages` 或 terminal websocket：`stellaclaw/src/channels/web/channel.rs:895-925`、`stellaclaw/src/channels/web/websocket.rs:36-52`。删除后旧 HTTP 非创建接口会隐式重建 conversation 的问题已记录为 BUG-015。

- ChannelService 多协议 response decode 顺序本轮未发现串线。`AgentSessionResponse`、`KernelResponse`、`WorkspaceResponse`、`TerminalResponse` 都使用 `type` tag，当前响应枚举的 tag 不重叠；ChannelService 先解 AgentSession，再解 Kernel/Workspace/Terminal，不会把正常 kernel error 当成 agent response：`stellaclaw/src/services/channel.rs:330-530`，`stellaclaw/src/service_protos/agent_session.rs:260-283`，`stellaclaw/src/service_protos/kernel.rs:90-111`。

- Workspace move/delete 普通路径边界本轮未新增 BUG。local/remote 都先走 lexical `normalize_workspace_path` / remote `norm`，会拒绝绝对路径和 `..`；本地 delete 使用 `symlink_metadata`，删除 symlink 本身而不是跟随 symlink 删除目标：`stellaclaw/src/services/workspace.rs:560-651`、`1079-1099`、`1238-1263`、`1357-1381`。已有 symlink 写入/读取边界仍归入前面记录的产品边界待确认项。

- runtime config broadcast 覆盖面本轮暂不新增 BUG。Web 当前暴露的 `/model`、`/reasoning`、`/remote` 分别影响 AgentSession launch、Workspace/Terminal remote mode，kernel 会把 update 广播给 AgentSession、Workspace、Terminal：`stellaclaw/src/channels/web/control.rs:31-88`、`stellaclaw/src/conversation_new.rs:1213-1257`。这里说的是 broadcast 覆盖面；`/model` chat-capable 校验问题和 `/remote` host 校验问题已分别记录为 BUG-006 / BUG-022。ToolBinaryService 持有 sandbox 副本且不在 broadcast 列表里，但 Web `/sandbox` 当前直接拒绝，未找到当前用户入口能更新 sandbox 后立刻触发 ToolBinary 使用旧 sandbox 的真实链路：`stellaclaw/src/channels/web/control.rs:89-95`、`stellaclaw/src/services/tool_binary.rs:20-31`。

## 4. 后续建议检查点

- 给所有跨 service command 区分 notification 与 request；对用户入口的 foreground-targeting call，缺失 target 应回 channel-visible error，不能让 kernel 失败。
- 给 `CreateForegroundSession` / `UpdateRuntimeConfig` / metadata update 等 kernel calls 增加 request correlation，Web 等待具体 response/error。
- `run_conversation_event_bridge` 需要识别 disconnected receiver 或 conversation generation 变化，重启后重新订阅。
- Cron owner/channel 校验需要适配当前 `channel/main` 承载多 foreground session 的事实，或者真正引入 `channel/<foreground_id>`。
- Bridge tool 应在 core 工具执行层完成字段级合法性检查；不符合 schema/业务前置条件时应立即返回 tool error，不应发出 `ConversationBridgeRequest`。
- Host bridge payload 属于内部协议；若内部坏 payload 到达 Host，应记录协议异常日志，优先修正上游工具校验，而不是把 Host 解析层作为主要输入防线。
- Web `/model` 应限定到 chat-capable/available agent models，或者 runtime config broadcast/apply 失败时回滚 patch 并投影明确错误。
- Cron run terminal event 处理后应主动 shutdown 对应 background AgentSession，避免一次性 run 变成长期 manifest service。
- AgentSession runtime 启动/重启失败应留在 session 级错误域，不能通过 `ServiceOutput::Failed` 升级为 conversation kernel fatal。
- Web error projection 需要携带 foreground/session id；至少 RuntimeCrashed 应和 TurnFailed 一样投影到对应 session stream。
- fixed SSH remote archive download 应和 local download 共用大小上限，最好让 remote helper 在构造/返回前也能提前中止。
- ChannelService 对需要 materialization 的 incoming message 应保持每个 foreground session 的入队顺序，例如同一 session 内暂停后续 message enqueue，直到前序 materialize 完成或降级。
- Workspace materialize 的所有失败都应落回 `FileState::Crashed` 或 `WorkspaceResponse::Error`，不能让用户附件失败升级为 service/kernel fatal。
- AgentSession shutdown response 需要区分 request/notification；self-shutdown 不应生成发给自己的 response，或者 kernel 应在服务停止流程中安全丢弃这类 response。
- Cron task id 应改为 conversation-global 唯一，或把 key 改成 `(registered_by, task_id)`；注册同名任务时至少不能静默覆盖其它 owner 的任务。
- Web API 应区分“确保已启动已存在 conversation”和“创建/引导新 conversation”；除显式 create 路径外，未知 conversation id 应返回 404。
- AgentSession 收到 `RuntimeCrashed` 后应进入明确的 terminal/restartable 状态：停止或丢弃 runner，后续消息要么拒绝并返回 session-visible error，要么显式重启 runtime；core `SessionRpcThread` 也不应吞掉 mailbox append 失败。
- 对所有来自 Web path segment 的 `conversation_id` / `foreground_session_id` / `terminal_id` 等 id 做统一校验，禁止 `.`、`..`、空白、路径分隔符和不符合 storage id 约定的字符；所有 workdir 拼路径处也需要 canonical boundary check。
- `ServiceAddr::storage_component()` 不能直接拼未转义 path segment；如果 ServiceAddr 允许任意字符串 segment，storage component 必须使用固定安全编码，或在构造 `ServiceAddr` 时统一校验 segment。
- Workspace file read 应强制默认分块大小和最大 `limit_bytes`，local/remote 统一使用响应字节上限，避免 `ReadFile` 比 archive download 更容易触发大响应。
- Cron active run 需要持久化或在启动时从 manifest/background id 重建；否则重启恢复出的 cron background session 不能正确回填 task status、forward result 或参与重复运行保护。
- Skill persist / sync 的递归复制需要拒绝 symlink，或只复制经过 canonical boundary check 的普通文件；同步到其它 workspace 和 upstream repo 前也应复用同一套 entry 校验。
- `/remote` 写入 runtime config 前应复用统一 remote host/path 校验；Terminal fixed SSH 创建也应在 service/runtime 层校验 host，并把 `--` 放在 host 之前只作为 remote command 分隔不能替代 host validation。
- Workspace archive upload 需要对解压后单文件大小、总写入字节数和 entry 数量设置统一上限；local 和 fixed SSH remote helper 都要在写入前/写入中累计并中止。
- `subagent_join.timeout_seconds` 应校验 finite、正数和最大值，和其它 tool timeout 一样 clamp 到合理范围；所有 `Duration::from_secs_f64` 前都应避免未校验 provider 数字直接进入 panic API。
- Web 入口接收 `FileItem` 时只允许 `data:` 或服务端已登记的 attachment/output URI；所有 `file://` 必须 materialize 到 conversation-owned attachment 目录并做 canonical boundary check，provider media normalizer 也应拒绝越界本地路径。
- provider 响应解析层不应把外部返回的 `file://` 当作可信 assistant-generated artifact；assistant image history replay 前也应对 `FileItem` 来源和本地路径做 boundary check，或只 replay 服务端 output persistor 生成的文件。
- Workspace list 应把 `limit` 变成执行上限而不是仅返回上限；本地和 remote helper 都应边枚举边截断，并避免为了精确 `total_entries` 扫完整个目录。
- Telegram 附件下载应解析并校验 `file_size`，同时用 streaming reader 累计字节数；超过上限时写入 crashed `FileItem`，不要先把完整响应读入内存。
- Provider-backed media job 应把 provider message 中的所有 `FileItem` 收集进 `ToolResultContent.files[]`，不要只保留第一个文件。
- foreground session 的 rename/delete/mark seen 等 Web API 应先确认 session 存在；metadata 里的 `session_nicknames` 不应成为“创建 session”的隐式来源。
- Codex assistant image history replay 应按 URI/transport 分支处理：`https://` 可以直接作为 `image_url` replay，只有 conversation-owned local files 才进入 inline base64 normalization。
