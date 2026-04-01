# Sandbox RPC Design

## Goal

Define a backend-neutral parent-child RPC bridge so that:

- `agent_host` remains the control plane
- sandboxed child processes run `agent_frame` or `zgent`
- host-owned tools still work after backend execution moves into a subprocess

This document is the implementation plan for the RPC half of the sandbox redesign.

## Why This Comes First

The current codebase is still fundamentally in-process:

- `agent_host` builds `Tool` closures in [server.rs](/Users/jeremyguo/Projects/ClawParty2.0/agent_host/src/server.rs)
- `agent_frame` exposes `Tool { name, description, parameters, handler }` in [tooling.rs](/Users/jeremyguo/Projects/ClawParty2.0/agent_frame/src/tooling.rs)
- `agent_host` switches backend in-process in [backend.rs](/Users/jeremyguo/Projects/ClawParty2.0/agent_host/src/backend.rs)

That means:

- tools are closures
- closures cannot cross a process boundary
- moving to `bubblewrap` without first introducing RPC will break all host-owned tools

So RPC is the real prerequisite.

## Current Ownership Model

### Parent-owned today

- session lifecycle
- workspace registry
- background and subagent spawning
- cron management
- channel sending
- agent status and token accounting
- workspace history tools

### Child-owned today

Conceptually these belong inside the sandbox:

- local file tools
- local process tools
- local patch tools
- local downloads and web fetches
- local image inspection on already-mounted files

## Design Summary

Introduce a child-runner protocol.

The parent launches a child runner process with:

- backend kind
- resolved config
- visible tool schemas
- socket FD for RPC

The child:

- instantiates the selected backend
- exposes a serializable tool list to the backend
- locally executes child-local tools
- forwards host-owned tool invocations back to the parent
- streams checkpoints and events to the parent

The parent:

- executes host-owned tool requests
- persists events/checkpoints
- handles cancellation and timeouts

## Process Model

### Parent process

Long-lived:

- `agent_host`

### Child process

Short-lived:

- one child per turn

Subcommands to add:

- `agent_host run-child --backend agent_frame`
- `agent_host run-child --backend zgent`

This is preferred over a second binary because:

- config loading stays centralized
- code reuse is easier
- deployment stays simpler

## Transport

Long-term target: use a Unix domain socket pair.

Current implementation status:

- the first version uses `stdin/stdout`
- messages are newline-delimited JSON
- this is intentionally a bring-up step, not the final transport choice

Recommendation:

- parent creates `UnixStream::pair()`
- one end stays in parent
- the other end is inherited by the child via FD passing
- child receives the FD number through env var or CLI arg

### Why this transport

- works locally
- supports duplex messaging
- supports FD passing later for real mounts
- keeps logs separate from RPC

## Message Framing

Use newline-delimited JSON for v1.

Each frame is a single JSON object followed by `\n`.

Reasons:

- easy to debug with `socat` or dumps
- low complexity
- enough for the current scale

If overhead matters later, migrate to MessagePack with the same message schema.

## Core Message Types

### Parent -> Child

- `init`
- `tool_response`
- `cancel`
- `soft_timeout`

### Child -> Parent

- `started`
- `session_event`
- `checkpoint`
- `tool_request`
- `completed`
- `failed`

## Suggested Schemas

### `init`

```json
{
  "type": "init",
  "payload": {
    "backend": "agent_frame",
    "previous_messages": [],
    "prompt": "",
    "config": {},
    "tools": []
  }
}
```

### `tool_request`

```json
{
  "type": "tool_request",
  "payload": {
    "id": "req-123",
    "tool_name": "workspace_mount",
    "arguments": {
      "workspace_id": "abc",
      "mount_name": "reference"
    }
  }
}
```

### `tool_response`

```json
{
  "type": "tool_response",
  "payload": {
    "id": "req-123",
    "ok": true,
    "result": {
      "mount_path": "/workspace/mounts/reference"
    }
  }
}
```

### `session_event`

```json
{
  "type": "session_event",
  "payload": {
    "kind": "agent_frame_tool_call_started",
    "round_index": 2,
    "tool_name": "read_file"
  }
}
```

### `checkpoint`

```json
{
  "type": "checkpoint",
  "payload": {
    "messages": [],
    "usage": {}
  }
}
```

### `completed`

```json
{
  "type": "completed",
  "payload": {
    "report": {}
  }
}
```

## Tool Compatibility Strategy

This is the key design choice.

### Keep one logical tool catalog

Do not create:

- one tool system for in-process mode
- and another unrelated one for subprocess mode

Instead split tool representation into:

### 1. Serializable definition

`RemoteToolDefinition`

Fields:

- `name`
- `description`
- `parameters`
- `execution_scope`

`execution_scope` values:

- `child_local`
- `parent_rpc`

### 2. Executable implementation

Only the side that owns the tool implementation keeps the actual handler.

This lets:

- the backend see one tool list
- while execution is dispatched to the right side

## Execution Scope Mapping

### `child_local`

These execute entirely in the child:

- `read_file`
- `write_file`
- `edit`
- `apply_patch`
- `exec_start`
- `exec_wait`
- `exec_observe`
- `exec_kill`
- `download_file`
- `web_fetch`
- `web_search` if implemented as direct upstream HTTP from child
- `image`
- `skill_load`
- `skill_create`
- `skill_update`

### `parent_rpc`

These remain in `agent_host`:

- `workspaces_list`
- `workspace_content_list`
- `workspace_mount`
- `workspace_content_move`
- `run_subagent`
- `start_background_agent`
- cron tools
- agent stats tools

## How `agent_frame` integrates

Current `agent_frame` expects `Tool` closures and calls `tool.invoke(...)` directly.

Migration path:

### Phase 1

Keep `agent_frame::Tool` unchanged.

### Phase 2

In the child, build a synthetic tool registry:

- child-local tools use normal local closures
- parent-owned tools use RPC proxy closures

The RPC proxy closure:

1. serializes the call
2. sends `tool_request`
3. waits for `tool_response`
4. returns the JSON result

This means `agent_frame` itself needs minimal change.

## How `zgent` integrates

The current `zgent` compatibility path in [backend.rs](/Users/jeremyguo/Projects/ClawParty2.0/agent_host/src/backend.rs) already uses a backend-neutral tool definition mapping.

That means the child-side `zgent` implementation should also:

- consume the same `RemoteToolDefinition` list
- use the same RPC proxy mechanism for parent-owned tools

So `agent_frame` and `zgent` should share:

- one child tool registry builder
- one RPC client implementation

## Cancellation

### Parent behavior

- on soft timeout: send `soft_timeout`
- on hard timeout: send `cancel`
- after grace: kill child process

### Child behavior

- soft timeout should be surfaced to backend control flow
- hard cancellation should interrupt any waiting local tool
- parent-RPC tool calls should be failed locally if cancellation arrives mid-flight

## Error Handling

### Tool errors

Parent tool failures should return ordinary tool error payloads, not transport failures.

That means:

- `tool_response.ok = false`
- include structured error text

### Transport failures

If RPC transport itself breaks:

- child should fail the turn
- parent should record the failure as infrastructure failure, not a tool error

## Logging

The child should not write durable logs directly.

Instead:

- child emits `session_event` and `checkpoint`
- parent converts them to existing agent/session log entries

This preserves current observability layout.

## Implementation Steps

### Step 1

Add serializable RPC message enums in a new module:

- `agent_host/src/child_rpc.rs`

### Step 2

Add `RemoteToolDefinition` plus `execution_scope`

### Step 3

Refactor `build_extra_tools()` in [server.rs](/Users/jeremyguo/Projects/ClawParty2.0/agent_host/src/server.rs) into:

- host executable handlers
- serializable tool definitions

### Step 4

Add child runner entrypoint to `agent_host`

### Step 5

Implement parent RPC dispatcher

### Step 6

Implement child-side RPC proxy tools

### Step 7

Run `agent_frame` in the child without `bubblewrap` yet

### Step 8

Run `zgent` in the child using the same bridge

### Step 9

Only after this is stable, wrap child launch with `bubblewrap`

## Key Design Decision

The compatibility layer should be:

- backend-neutral
- transport-neutral at the message schema level
- authority-preserving

The parent stays authoritative.
The child is a disposable executor.
