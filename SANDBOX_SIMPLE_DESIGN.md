# Sandbox Simple Design

## Goal

做一个比当前共享 `rundir` 更清晰、但比完整 project/mount/lease 系统简单很多的方案。

这个方案的核心不是“强安全沙箱”，而是：

- 给每个 Main Agent 一个长期工作区
- 允许别的 Agent 搜索、查看、只读挂载旧工作区
- 允许把旧工作区中的部分内容搬运到当前工作区继续工作
- 在 Main Agent 销毁时自动生成工作区摘要，降低跨 Agent 复用成本
- 30 天未修改的工作区自动归档，通过 `/oldspace` 重新激活

## Non-Goals

- 这不是 hostile multi-tenant 安全边界
- v1 不做容器级隔离
- v1 不做真正的 OS bind mount / overlayfs / FUSE
- v1 不解决所有并发冲突，只做最小可控的一致性约束

## Why This Simpler Design

相比之前的 `project_*` 设计，这一版收敛了两个复杂点：

- 不再频繁判断“什么值得沉淀成 project”
- 不再引入单 writer / 多 reader / 抢占式写锁那一整套 project 控制面

我们先承认一个事实：

- Agent 大部分工作天然就是落在某个工作区里
- 跨 Agent 复用的第一需求，不是“长期知识建模”
- 而是“找到别人的工作区，看一下内容，把一部分拿过来继续做”

所以 v1 直接围绕 `workspace` 做协作，而不是先做 `project`

## High-Level Model

### Workspace ownership

- 每个 Main Agent 绑定一个 durable workspace
- 这个 workspace 在 Main Agent 生命周期之外仍然存在
- Sub-Agent 和 Main Background Agent 默认运行在当前 Main Agent 的 workspace 中，不创建自己的 durable workspace

这意味着：

- 用户每次 `/new` 创建新的 Main Agent 时，会创建一个新的 workspace
- 老 workspace 不会消失，只是失去当前绑定
- 后续别的 Agent 可以通过工具搜索、查看、挂载、搬运它的内容

### Current workspace

当前 Main Agent 的 `cwd` 直接就是自己的 workspace 根目录。

它不再写共享 `rundir`，而是写：

- `workspaces/<workspace_id>/files/`

`agent_frame` 的 `workspace_root` 也直接指到这里。

### Cross-workspace reuse

跨工作区复用分三层：

- `workspaces_list`
  - 看工作区 id 和内容简介
- `workspace_content_list`
  - 看别人的工作区里有什么
- `workspace_mount`
  - 把别人的工作区只读挂到当前工作区下
- `workspace_content_move`
  - 把别人的一部分内容搬到自己工作区里

## Technology Choice

## v1 choice

v1 不用容器，不用 FUSE，不用 bind mount。

用：

- 普通目录
- JSON 元数据注册表
- 宿主工具控制访问
- 只读挂载使用“materialized read-only snapshot”

### Why not containers in v1

容器能提供更强隔离，但现在不适合先上：

- Telegram / CLI 本地开发调试更复杂
- macOS 下 bind mount / overlay 行为不一致
- 我们当前真正缺的是“协作和组织”，不是“强隔离”
- 容器会把问题从产品设计变成基础设施设计

所以 v1 明确选择：

- 协作优先
- 一致性优先
- 安全边界只做到 cooperative level

### Why JSON in v1

现在这个阶段更重要的是先把 workspace 生命周期和工具语义跑通。

所以 v1 直接用一个 JSON registry：

- `<workdir>/sandbox/workspaces.json`

原因：

- 实现更轻
- 更容易人工检查和调试
- 当前并发规模不大

如果后面并发修改真的变复杂，再升级 SQLite。

## Filesystem Layout

```text
<workdir>/
  sandbox/
    workspaces.json
    workspace_meta/
      <workspace_id>/
        summary.md
  workspaces/
    <workspace_id>/
      files/
      mounts/
```

说明：

- `files/`
  - 这个 workspace 的真实工作目录
- `mounts/`
  - 当前 workspace 下对其他 workspace 的只读挂载视图
- `sandbox/workspace_meta/<workspace_id>/summary.md`
  - 宿主生成的摘要文件
  - 不放在 workspace 内部，避免 agent 自己乱改

Agent 的真实工作目录是：

- `<workdir>/workspaces/<workspace_id>/files/`

## Workspace Registry Schema

建议表：`workspaces`

字段：

- `id`
- `title`
- `summary`
- `state`
  - `active`
  - `archived`
- `created_at`
- `updated_at`
- `last_content_modified_at`
- `last_summarized_at`
- `last_main_agent_id`
- `last_session_id`

建议表：`workspace_mounts`

字段：

- `id`
- `owner_workspace_id`
- `mounted_workspace_id`
- `mount_path`
- `created_at`
- `source_revision`

这里不做 rw mount，所以不需要复杂 writer lease。

## Tool Design

这些工具提供给 Main Agent 和 Sub-Agent。

Sub-Agent 可以使用，是因为它也可能需要复用别的工作区内容。

### 1. `workspaces_list`

作用：

- 查看其他工作区的 id 和内容简介

输入：

- 可选自然语言 query
- 可选 `include_archived`

输出：

- `workspace_id`
- `title`
- `summary`
- `state`
- `updated_at`
- `last_content_modified_at`

行为：

- 默认只返回 `active`
- `archived` 需要显式要求才返回

### 2. `workspace_content_list`

作用：

- 查看别的 workspace 里有哪些内容

输入：

- `workspace_id`
- 可选 `path`
- 可选 `depth`
- 可选 `limit`

输出：

- 文件树列表
- 文件大小
- 修改时间

注意：

- 这里只列目录内容，不直接读文件正文
- 真要读正文，要先 `workspace_mount`，再用普通文件工具读取

### 3. `workspace_mount`

作用：

- 将别人的 workspace 以只读方式挂载到当前 workspace

输入：

- `workspace_id`
- 可选 `mount_name`

输出：

- `mount_path`
- `source_workspace_id`
- `source_revision`

### v1 implementation choice

这里的 “mount” 不是真正的 OS mount。

v1 用：

- 从源 workspace 复制一个 snapshot 到当前 workspace 的：
  - `mounts/<mount_name>/`
- 然后把 writable bit 去掉

也就是：

- materialized snapshot
- read-only by convention + file permission

优点：

- 跨平台
- 不需要 root
- 不需要容器
- 语义稳定

缺点：

- 不是强安全
- 不是实时同步

但对当前场景是够的，因为我们需要的是“看别人做了什么”，不是“实时共享编辑”。

### 4. `workspace_content_move`

作用：

- 将别的 workspace 中的一部分内容移动到自己 workspace

输入：

- `source_workspace_id`
- `paths`
- 可选 `target_dir`
- 可选 `update_source_summary`
- 可选 `update_target_summary`

输出：

- 实际移动的文件列表
- 目标路径
- 两边 metadata 是否更新

### v1 consistency rule

`workspace_content_move` 是唯一一个会修改“别人的 workspace”的跨工作区工具。

因此 v1 做一个简单但硬的限制：

- 只允许从“当前没有绑定运行中 Main Agent 的 workspace”移动内容

如果 source workspace 当前仍然被活跃 Main Agent 使用：

- 立即失败
- 提示用户或 Agent 先等待旧 Agent 结束，或者只做 `workspace_mount`

这样可以避免：

- 正在工作的 workspace 被别的 Agent 把文件搬走
- 双方同时改同一批文件

### Summary update rule

`workspace_content_move` 完成后：

- source workspace 的 `updated_at` 更新
- target workspace 的 `updated_at` 更新
- 如果启用了 summary 更新，就把 source 和 target 标记为 `summary_dirty`

真正的 summary 重写可以异步做，不必阻塞这次移动。

## Main Agent Destruction Summary

这是这套方案的关键。

### Trigger

当 Main Agent 被销毁时触发，例如：

- `/new`
- session reset
- process shutdown 时的 graceful cleanup

### What happens

1. 先看当前 session 是否有新增 turn
2. 如果上下文太长，走一次现有的 message compaction
   - 不看 idle 时间
   - 只看“是否超过压缩阈值”
3. 收集当前 workspace 的摘要输入
   - 当前 summary
   - 最近修改文件列表
   - 文件树概览
   - agent messages 或其压缩版
4. 调用当前 Main Agent 自己的模型，让它总结：
   - 这个 workspace 是干什么的
   - 最近做了什么
   - 里面最重要的文件/目录是什么
5. 写回：
   - `workspaces.summary`
   - `.host/summary.md`
   - `last_summarized_at`

### Why use the dying Main Agent itself

因为它最了解刚才做了什么。

这样比单独起一个 Maintainer Agent 更便宜，也更少上下文损耗。

## 30-Day Archival

规则：

- 一个 workspace 如果 `last_content_modified_at` 超过 30 天
- 就标记成 `archived`

注意：

- 不是删除
- 只是默认不再出现在普通 `workspaces_list` 结果中
- 仍然保留全部文件

### `/oldspace`

新增 channel 指令：

- `/oldspace <workspace_id>`

行为：

- 把目标 workspace 从 `archived` 改回 `active`
- 创建一个新的 Main Agent session 并绑定到这个 workspace
- 后续用户就在这个旧 workspace 上继续工作

## Design Choices

### Choice 1: Main Agent 持久 workspace，Sub-Agent 不持久

原因：

- 用户真正关心的是主线工作区
- Sub-Agent 更像任务执行器
- 如果给每个 Sub-Agent 都做持久 workspace，管理成本会迅速膨胀

### Choice 2: mount 用 snapshot copy，不用真正只读 bind mount

原因：

- 不需要 root
- 跨平台
- 更容易调试
- 语义简单

代价：

- 不是实时视图
- 不是强安全

### Choice 3: 元数据外置，内容内置

内容在：

- `workspaces/<id>/files/`

控制面元数据在：

- `sandbox/workspaces.json`
- `sandbox/workspace_meta/<workspace_id>/`

这样 Agent 不能直接篡改 registry。

### Choice 4: `workspace_content_move` 只允许从 inactive source 移动

原因：

- 这是 v1 最简单的一致性边界
- 比复杂锁系统便宜很多

## Failure Handling

### If summary generation fails

- 不影响 workspace 保留
- 只记录 error
- 保留旧 summary
- 标记 `summary_dirty = true`

### If mount snapshot creation fails halfway

- 删除不完整 mount 目录
- 不写 mount registry 记录

### If move fails halfway

v1 不做跨文件事务。

策略：

- 逐文件 copy
- 全部成功后再删除源文件
- 所以实现上更接近 “move by copy-then-delete”

这样失败时最坏结果是：

- 目标有部分新副本
- 源还保留原文件

不会出现“文件直接丢了”。

## Implementation Plan

### Phase 1

- 引入 workspace registry JSON
- Main Agent 绑定 durable workspace
- 切换 `workspace_root` 到当前 workspace `files/`
- `/new` 创建新 workspace

### Phase 2

- 实现 `workspaces_list`
- 实现 `workspace_content_list`
- Main Agent 销毁时自动写 summary

### Phase 3

- 实现 `workspace_mount`
- mount 用 snapshot copy + chmod read-only

### Phase 4

- 实现 `workspace_content_move`
- 增加 inactive-source 限制
- 增加 summary dirty 标记

### Phase 5

- 30 天自动归档
- `/oldspace <id>` 重新激活

## Future Upgrade Path

如果后面这套真的证明有价值，再升级到：

- 每个 Agent 私有 scratch workspace
- 真正的 read-only bind mount
- 容器级隔离
- project 层抽象

但那应该是 v2，不是现在。
