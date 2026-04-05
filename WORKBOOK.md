简单一点，我有一个全新的思路：
1. 直接不要/new了，这样用户在Telegram创建的每个窗口共享的东西只有：skills、skill_memory、USER.md、IDENTITY.md。
2. 然后针对每个Conversation的内部的上下文管理方式，我们可以学习codex的管理方法还有上下文压缩的算法。
3. 每个用户说话的时候（不是打断），的时候，因为共享内容会更新，我们只需要提示一个[System Message] 作为提示就行了。
这里一共有三种大类型：Skills、User、Identity，这三个有修改。其中Skill更新只用提示Skill xxx is updated to version xxx / removed / created to version xxx, 然后把description重新放一下就行了。然后这里skill要实现一个版本管理服务。update和create的时候要返回一个版本号。（我忘了之前我们是怎么设计了，好像update和create是同一个，反正要返回一个版本号，当工具的result）。
另外两个USER.md或者IDENTITY.md更新了，就要直接全文注入[System Message: XXX Updated]。注意区分好user prompt和更新系统提示。
4. 上下文压缩的时候，自动用最新的共享信息版本重新组装System Prompt。
5. 打断对话的时候，如果正在上下文压缩，就回复一句，正在上下文压缩。顺便帮我确认一下，如果上下文压缩的API失败了，打断对话还能不能插入进去。

# WORKBOOK

This document tracks medium-term implementation goals, active design questions, and a practical execution plan for ClawParty.

## How To Use

- Treat this file as the single high-level planning workbook.
- Keep completed items, but mark them clearly so future work retains context.
- Prefer updating status and notes here instead of creating many scattered TODO docs.
- Use concise notes that help the next implementation session restart quickly.

## Status Legend

- `todo`: not started
- `in_progress`: actively being designed or implemented
- `blocked`: waiting on a decision, dependency, or validation
- `done`: completed and verified

---

## 1. Multimodal Capability Unification

### Goal

Make model capability handling consistent across providers so the system can reason clearly about:

- multimodal input
- multimodal generation
- native model abilities vs tool-based abilities
- end-to-end multimodal workflows

### Why This Matters

Right now model abstraction is improving, but capability handling is still uneven:

- some models support text + image input directly
- some models support native web search or native image generation
- some workflows still rely on tools even when the model can do the task natively
- output routing is not fully unified for text/image/file responses

### 1.1 Multimodal Input

Status: `todo`

#### Desired End State

- Each model declares supported input modalities in a structured way.
- Prompt construction can choose the best representation automatically.
- The runtime can decide whether to:
  - pass multimodal input directly to the model
  - transform content into tool calls
  - degrade gracefully when a model lacks a modality

#### TODO

- Define a structured capability model for chat models:
  - text input
  - image input
  - file/document input
  - mixed multimodal turns
- Audit current provider compatibility:
  - `openrouter`
  - `openrouter-resp`
  - `codex-subscription`
- Unify message conversion rules between:
  - chat completions payloads
  - responses payloads
  - host-side attachment formatting
- Decide how non-image attachments should map:
  - plain text prompt injection
  - file metadata prompt injection
  - future native file input
- Add explicit downgrade behavior when a conversation switches to a model with weaker input modality support.

#### Open Questions

- Should file attachments remain prompt-expanded by default, or should they become first-class multimodal inputs when a provider supports them?
- Should image inputs always be passed directly when possible, or still be normalized through a common attachment layer first?

### 1.2 Multimodal Generation

Status: `todo`

#### Desired End State

- The system can generate text, images, and files through one consistent output contract.
- The runtime can decide whether to use:
  - native model generation
  - tools
  - hybrid flows

#### Core Design Tradeoff

There is a real tradeoff between:

- native model multimodal generation
  - better end-to-end coherence
  - fewer tool roundtrips
  - more provider-specific behavior
- tool-driven generation
  - easier orchestration
  - more explicit control
  - more predictable attachment handling

#### TODO

- Define output capability flags per model:
  - text
  - image generation
  - structured/native multimodal output
- Decide the routing policy:
  - when to prefer native image generation
  - when to force image tools
  - when to produce attachments from model output directly
- Design how multimodal outputs merge with prior context:
  - do generated images become persistent attachments in message history
  - how should compaction summarize multimodal outputs
  - what should `/continue` preserve after partial multimodal generation
- Add explicit telemetry for:
  - native image generation usage
  - tool fallback usage
  - output attachment persistence

#### Proposed Phases

- Phase A: capability schema only
- Phase B: multimodal input unification
- Phase C: native output routing for selected providers
- Phase D: compaction and resume semantics for multimodal turns

---

## 2. Service Management Ability

Status: `todo`

### Goal

Enable the system to safely perform controlled host-side service management tasks, such as:

- installing software to `/opt`
- managing external services
- potentially creating or updating `systemd` units

### Why This Matters

There are tasks where the agent needs to affect the machine outside the workspace:

- deploy software
- run durable background services
- manage integration processes
- install tools for future sessions

Current sandboxed execution is intentionally constrained, so this capability needs a dedicated control-plane design instead of relying on ad hoc `sudo`.

### Constraints

- Bubblewrap sandbox should not gain arbitrary root powers.
- Host changes must be explicit, auditable, and narrowly scoped.
- The model should not get blanket unrestricted shell access on the host.

### TODO

- Define a host-management capability boundary:
  - install package into `/opt`
  - manage service unit files
  - restart/enable/disable named services
  - inspect service status/logs
- Decide execution architecture:
  - host-side privileged helper
  - explicit reviewed action queue
  - signed or policy-checked operations
- Define the minimal safe operation set for v1.
- Decide whether these actions belong to:
  - dedicated tools
  - a separate management agent
  - a privileged control daemon
- Add durable logs for all external host modifications.

### Suggested v1 Scope

- read-only service inspection
- install into `/opt`
- create/update named `systemd` units from templates
- restart/status for explicitly managed units only

### Open Questions

- Should privileged operations require human confirmation every time?
- Should the system maintain an allowlist of managed service names?

---

## 3. Web Admin Page

Status: `todo`

### Goal

Provide a web-based management UI for:

- conversation history
- session state
- logs
- failure diagnostics
- operational controls

### Why This Matters

Current debugging is too dependent on:

- raw session JSON
- server logs
- channel logs
- manual grep/jq usage

A web console would make the system easier to operate, debug, and inspect.

### MVP Features

- conversation list
- session detail page
- recent turn history
- pending continue state
- error summary display
- model / sandbox / reasoning settings display
- server log viewer
- channel delivery log viewer

### TODO

- Decide whether admin UI lives:
  - inside `agent_host`
  - as a separate small web service
- Design read models for:
  - conversations
  - sessions
  - checkpoints
  - pending continue
  - subagents/background agents
- Add API endpoints for:
  - listing conversations
  - reading session snapshots
  - tailing logs
  - searching error events
- Decide auth model for local/private deployment.
- Design pages for:
  - overview
  - conversation detail
  - log search
  - workspace/service status

### Nice To Have

- replay a turn timeline
- compaction visualization
- tool execution timeline
- per-conversation health indicators

---

## 4. Programming Capability Enhancement

Status: `todo`

### Goal

Improve coding performance by expanding tools and sharpening existing workflows, informed by Codex and Claude Code style ergonomics.

### Current Opportunity Areas

- richer codebase navigation
- stronger edit primitives
- better long-running command workflows
- improved diff/test/diagnostic loops
- more ergonomic parallel work/delegation

### TODO

- Audit current programming workflow against common coding-agent tasks:
  - repo exploration
  - symbol search
  - batch edits
  - refactors
  - test loops
  - diagnostics
- Identify missing or weak tools:
  - symbol-aware search
  - better patch application feedback
  - richer command progress observation
  - structured test result parsing
  - better workspace diff summaries
- Improve current tools where cheaper than adding new ones:
  - `exec_*`
  - `read_file`
  - `edit`
  - `apply_patch`
  - file download / image workflows
- Evaluate adding higher-level coding tools:
  - repo grep with richer metadata
  - symbol index or tree-sitter based navigation
  - test runner wrapper
  - lint/fix wrapper
  - git-aware review helpers
- Improve tool guidance in prompt/tool descriptions without wasting tokens.

### Reference Directions

- Codex-style structured coding loop
- Claude Code-style repo exploration ergonomics
- better interruption-safe long-running coding tasks
- stronger subagent use for parallel implementation/verification

---

## 5. Cross-Cutting Product / Platform Concerns

### 5.1 Authentication

Status: `todo`

#### Goal

Unify authentication handling across providers, channels, and privileged/control-plane features.

#### Why This Matters

- provider auth is becoming more diverse
- some auth is host-resolved and injected into sandboxes
- future admin and service-management features will need stronger access control

#### TODO

- audit current auth flows:
  - API key env-based auth
  - codex subscription auth
  - refresh / persistence behavior
- define a cleaner auth abstraction for providers
- decide what secrets are allowed inside child processes vs only on host
- design admin/auth model for future web UI
- define operator authentication requirements for privileged host actions

### 5.2 Telegram Rendering

Status: `todo`

#### Goal

Improve Telegram-specific rendering quality and predictability for rich outputs.

#### Why This Matters

- Telegram has stricter formatting/rendering constraints than generic markdown
- attachment/image delivery behavior affects user trust and usability
- multi-part rendering needs to remain readable across long messages and grouped media

#### TODO

- review current markdown-to-Telegram HTML translation behavior
- improve rendering for:
  - code blocks
  - long lists
  - mixed text + attachments
  - multi-image replies
- define clearer output conventions for:
  - captions
  - attachment tags
  - chunked replies
- add regression coverage for Telegram rendering edge cases

### 5.3 Telegram Channel Rate Limiting

Status: `todo`

#### Goal

Add explicit rate limiting to actual Telegram sends so bursts of outgoing traffic do not cause avoidable failures or degraded UX.

#### Why This Matters

- message bursts can happen during:
  - typing updates
  - `user_tell`
  - chunked long replies
  - multi-image sends
  - recovery / resend flows
- Telegram has practical API limits and burst sensitivity

#### TODO

- define per-chat and global send rate policy
- distinguish rate policy for:
  - text messages
  - typing actions
  - media groups
  - attachment/file sends
- add queueing or token-bucket style throttling in the Telegram channel implementation
- record rate-limit hits in logs and expose them later in admin tooling
- ensure retry behavior is compatible with ordered delivery

### 5.4 In-Agent Updates To `USER.md` / `IDENTITY.md`

Status: `todo`

#### Goal

Let the agent update `USER.md` and `IDENTITY.md` from inside normal work loops in a controlled, intentional way.

#### Why This Matters

- durable user and agent profile knowledge is currently important but awkward to maintain
- profile updates should be part of the product workflow, not just a manual operator task
- changes to these files affect future prompting behavior and therefore need explicit policy

#### TODO

- define when the agent is allowed to propose or apply updates to:
  - `USER.md`
  - `IDENTITY.md`
- decide whether updates should be:
  - fully automatic
  - tool-mediated
  - confirmation-gated
- define write rules for:
  - stable user preferences
  - durable project/operator constraints
  - agent self-behavior notes
- add change logging so profile edits are auditable
- decide how these edits interact with:
  - session history
  - compaction summaries
  - multi-conversation consistency

---

## Cross-Cutting Technical Themes

### A. Provider Abstraction

Status: `in_progress`

- Recent work split upstream model handling into provider-specific modules.
- Continue extending that design so modality, auth, request shape, and output shape remain provider-local.

### B. Compaction and Continuation

Status: `in_progress`

- Any new multimodal/service/programming features must preserve:
  - stable checkpoints
  - `/continue`
  - pending continue recovery
  - compaction summaries

### C. Attachment Lifecycle

Status: `in_progress`

- Attachments are now part of user-facing delivery semantics.
- Future multimodal output and admin UI work should reuse the same attachment contract.

---

## Proposed Priority Order

### Near Term

1. Finish provider abstraction cleanup around modality-related behavior
2. Define multimodal capability schema
3. Improve programming-tool ergonomics with low-risk upgrades
4. Add read-only web admin visibility for logs and sessions

### Mid Term

1. Introduce controlled host/service management capability
2. Add native multimodal output routing where provider support is strong
3. Expand web admin from observability to operations

### Later

1. Advanced service automation
2. richer multimodal end-to-end flows
3. stronger symbol-aware coding subsystem

---

## Immediate Next Steps

Status: `todo`

- Define a model capability schema that covers multimodal input and output.
- Decide the first safe slice of host/service management.
- Sketch minimal backend API shape for the future web admin page.
- Audit current coding-tool pain points and rank by impact vs implementation cost.

---

## Session Notes

### 2026-04-05

- Created this workbook to consolidate medium-term product and architecture planning.
- Seeded four major workstreams:
  - multimodal capability unification
  - host/service management
  - web admin UI
  - programming capability enhancement
