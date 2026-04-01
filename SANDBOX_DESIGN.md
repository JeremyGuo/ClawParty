# Sandbox Design

## Status

This document replaces the previous `project_*`-centric sandbox design.
The old design should be considered abandoned.

The new direction is:

- one sandboxed subprocess per agent turn
- `bubblewrap` as the isolation primitive
- real mounts instead of copied workspace snapshots
- a small RPC bridge between `agent_host` and the child runtime

## Goals

- Run every foreground/background/sub-agent turn in an isolated child process.
- Prevent the child from seeing arbitrary files under the host user's home directory.
- Keep the child's effective uid/gid the same as the launching user.
- Preserve access to the agent's own workspace and explicitly granted mounts.
- Support real read-only and read-write workspace mounts inside the sandbox.
- Keep `agent_frame` and `zgent` selectable as backends without duplicating tool logic.
- Keep host-owned tools available even after agent execution moves into a subprocess.

## Non-Goals

- This is not a root sandbox.
- This does not allow unprivileged code to install system packages that require root.
- This does not try to make prompt rules a hard security boundary.
- This does not solve hostile code execution against the current user account.

## Important Clarification

The sandboxed child keeps the same user identity as the launcher.
That means:

- if the host user can write somewhere, the sandbox may be allowed to write there only if that path is mounted into the sandbox
- if the host user cannot write somewhere, `bubblewrap` does not magically grant that permission

So a command like `apt install XXX` still requires root on the host and will fail unless the launcher itself is root.

What this design does guarantee is:

- the child cannot see arbitrary paths from the host home directory
- the child only sees the filesystem subtree that `agent_host` explicitly mounts

## High-Level Model

### Parent process

`agent_host` remains the long-lived control plane:

- channels
- session state
- workspace registry
- mount policy
- background scheduling
- token accounting
- user-visible logs

### Child process

Every agent turn runs in a fresh child process:

- one child for each foreground turn
- one child for each background turn
- one child for each sub-agent turn

This child is started through `bubblewrap`.

Inside the child we run exactly one backend runtime:

- `agent_frame`
- or `zgent`

The child is disposable.
It should hold no authority that the parent cannot revoke by simply killing it.

## Filesystem Layout

```text
<workdir>/
  rundir/
    AGENTS.md
    .skills/
    skill_memory/
  sandbox/
    state/
    workspace_meta/
    mounts.json
    workspaces.json
  workspaces/
    <workspace_id>/
      files/
      upload/
```

### Meaning

- `rundir/`
  - template and shared control-plane content
- `rundir/skill_memory/`
  - shared RW memory for skills
- `workspaces/<id>/files/`
  - the agent's durable workspace root
- `workspaces/<id>/upload/`
  - uploaded files associated with that workspace
- `sandbox/`
  - host-owned metadata and registries, not visible for ordinary direct agent editing

## Bubblewrap Sandbox Shape

Each turn subprocess is launched with a restricted mount namespace.

### Visible inside the child

The child should see:

- a private `/tmp`
- a private `/var/tmp`
- read-only system runtime paths needed for execution:
  - `/usr`
  - `/bin`
  - `/sbin`
  - `/lib`
  - `/lib64`
  - `/etc`
  - `/dev`
- the selected workspace root mounted as the child working directory
- the workspace upload directory
- explicit workspace mounts granted by policy
- `.skill_memory` mounted RW into the workspace

### Not visible inside the child

The child should not see:

- the rest of the host home directory
- sibling workspaces unless explicitly mounted
- host metadata registries such as `sandbox/workspaces.json`
- arbitrary repository paths outside the mounted workspace subtree

### Recommended internal sandbox paths

Inside the child, use a stable path model:

```text
/workspace
/workspace/upload
/workspace/.skill_memory
/workspace/mounts/<mount_name>
```

The agent's effective cwd should be:

```text
/workspace
```

This avoids leaking host absolute paths into prompts and model outputs.

## Skill Memory

### Goal

Introduce a shared skill-scoped memory directory:

```text
rundir/skill_memory
```

This should be mounted RW into every sandboxed workspace as:

```text
/workspace/.skill_memory
```

### Policy

This is not a hard filesystem ACL.
It is a policy rule enforced primarily through prompt and skill design:

- ordinary agents should not proactively use `.skill_memory`
- skills may instruct the agent to use `.skill_memory`
- if a skill requires persistent cross-workspace memory, it should use this directory explicitly

### Why this is acceptable

The child already has the same uid as the launcher.
So the strict boundary here is not OS permission, but mount visibility and policy.
`skill_memory` is intentionally shared state, so prompt-level restriction is enough for v2.

## Workspace Mounts

The previous snapshot-copy mount model should be replaced by real mounts.

### Supported modes

- `ro`
- `rw`

### Policy

- many readers are allowed
- at most one writer is allowed
- parent-owned registry decides whether a mount request is granted
- the child cannot grant mounts to itself without parent approval

### Important behavior

Because mounts are real:

- if one workspace is mounted into another
- and host-managed operations modify the source workspace
- the mounted view updates immediately

This is the main reason to move away from copied snapshots.

## Turn Lifecycle

For each turn:

1. Parent resolves session and workspace state.
2. Parent computes the mount plan for this turn.
3. Parent launches a child with `bubblewrap`.
4. Child starts the requested backend runtime.
5. Child and parent communicate over a local RPC channel.
6. Parent kills the child after:
   - normal completion
   - cancellation
   - crash
   - hard timeout
7. No turn-local runtime state survives except files written to mounted locations.

This means:

- sandbox state is reset every turn
- hidden mount leakage across turns is avoided
- mount plans are recomputed from parent-owned state every time

## Why Per-Turn Subprocesses

This is better than keeping one long-lived agent runtime process because:

- mount plans are easy to rebuild
- crash cleanup is trivial
- leaked file descriptors and temp state die with the process
- backend switching between `agent_frame` and `zgent` becomes symmetric
- the parent remains the single source of truth

The tradeoff is startup overhead, but this is acceptable given the security and consistency benefits.

## Tool Compatibility Problem

This is the key architectural issue.

Today the system works like this:

- `agent_host` constructs `Tool` closures in-process
- `agent_frame` or `zgent` invokes them directly

That breaks once the backend moves into a child process.
Closures cannot be passed across the process boundary.

## Recommended Solution: Local RPC Bridge

Use a parent-child RPC protocol.

### Parent responsibilities

- launch the sandbox child
- own session state
- own workspace registry
- own mount policy
- execute host-only tools
- persist logs
- surface checkpoints and events

### Child responsibilities

- run the selected backend
- execute child-local builtin tools
- forward host-owned tool calls over RPC
- emit progress events and checkpoints back to parent

## Transport Choice

The long-term preferred transport is a Unix domain socket pair or an inherited Unix socket FD.

Current implementation status:

- the first working version uses `stdin/stdout`
- messages are newline-delimited JSON
- this keeps bring-up simple while the child runner contract stabilizes

Planned upgrade:

- move to one duplex Unix socket
- keep the same logical RPC schema
- optionally move from JSON to MessagePack later if overhead matters

### Why a socket is still the target

`stdin` and `stdout` work, but they mix badly with:

- backend debugging output
- shell subprocess output
- future streaming

So sockets remain the cleaner long-term transport even though the initial implementation uses stdio.

## RPC Message Types

Minimum set:

- `run_started`
- `session_event`
- `checkpoint`
- `tool_request`
- `tool_response`
- `cancel_requested`
- `soft_timeout_requested`
- `turn_completed`
- `turn_failed`

Suggested `tool_request` payload:

```json
{
  "id": "req-123",
  "tool_name": "workspace_mount",
  "arguments": {
    "workspace_id": "abc",
    "mount_name": "reference"
  }
}
```

Suggested `tool_response` payload:

```json
{
  "id": "req-123",
  "ok": true,
  "result": {
    "mount_path": "/workspace/mounts/reference",
    "mode": "ro"
  }
}
```

## Tool Ownership Split

### Child-local tools

These can run entirely inside the sandbox:

- file reads and writes within mounted paths
- local `exec_*`
- local `apply_patch`
- local image inspection of files already present in the sandbox
- local `web_fetch` and `download_file`

### Parent-owned tools

These must stay in `agent_host`:

- `run_subagent`
- `start_background_agent`
- cron management
- session status tools
- workspace registry queries
- workspace mount / move authorization
- agent stats and registry inspection

### Why this split is good

It keeps the parent authoritative for control-plane actions, while keeping data-plane file and process tools inside the sandbox where they belong.

## Real Mount Implementation

This is the hardest part.

### Recommendation

Do not expose every workspace path into the child pre-emptively.

Instead:

1. Parent authorizes the mount request.
2. Parent opens the source workspace root.
3. Parent passes a directory FD over the Unix socket using `SCM_RIGHTS`.
4. Child-side mount code attaches that FD under `/workspace/mounts/<mount_name>`.
5. Child applies `ro` or `rw` flags according to the grant.

This avoids making all workspaces globally visible inside the sandbox.

### Why this is better than prebinding all workspaces

If every workspace were prebound under a hidden internal path, an agent with shell access could still discover them.
FD-passing keeps visibility demand-driven.

## Workspace Content Move

`workspace_content_move` should remain parent-authorized.

Recommended behavior:

1. Parent validates permission.
2. Parent performs the move on host-visible workspace roots.
3. Parent updates workspace summaries and timestamps.
4. Any live mounted view inside the current child reflects the change automatically because the mount is real.

This keeps metadata consistency in one place.

## Backends: `agent_frame` and `zgent`

Both backends should run under the same child-runner protocol.

The parent should not know backend-specific tool semantics.

Instead, define one backend-neutral child runner contract:

- input:
  - backend kind
  - prompt
  - previous messages
  - config
  - visible tool schemas
- output:
  - session events
  - checkpoints
  - tool requests
  - final report

The child runner selects:

- `agent_frame`
- or `zgent`

internally.

This keeps the process isolation design from being coupled to one backend implementation.

## Interaction With Current Tool System

Today a tool is essentially:

- metadata
- JSON schema
- a Rust closure

That is fine in-process but not across a subprocess boundary.

The migration path should be:

### Step 1

Keep the current `Tool` abstraction in the parent.

### Step 2

Add a serializable `RemoteToolDefinition`:

- `name`
- `description`
- `parameters`

### Step 3

When launching the child:

- parent sends only serializable tool definitions
- child exposes them to the backend
- any host-owned tool invocation becomes an RPC request back to the parent

### Step 4

For child-local builtin tools:

- keep their implementation inside the child runtime

This gives one compatibility model for both `agent_frame` and `zgent`.

## Timeouts and Cancellation

The timeout model should remain parent-owned.

### Parent

- soft timeout
- hard timeout
- final kill

### Child

- obey soft timeout signals
- emit timeout observations back into backend flow
- stop local tool execution on hard cancellation

Because each turn is a separate subprocess, hard cancellation is simple:

- kill the child process tree

## Logging

The child should not own durable logs.

Instead:

- child emits structured events over RPC
- parent writes them into existing session and agent logs

This keeps all durable observability in one place even after process isolation.

## Design Choices Summary

### Chosen

- `bubblewrap`
- one child per turn
- same uid/gid as launcher
- hidden host home except explicitly mounted paths
- real mounts
- `skill_memory` as shared RW mount
- parent-child RPC over Unix socket
- parent-owned control plane
- child-owned local execution plane

### Rejected

- keeping the backend in-process
- copied workspace mount snapshots
- making prompt-only rules the main security mechanism
- duplicating a second tool implementation just for subprocess mode
- prebinding every workspace into the child

## Risks

- `bubblewrap` is Linux-specific
- real mount updates are significantly more complex than copied snapshots
- FD-passing mount setup requires careful low-level implementation
- shell access still means code runs as the host user, just with reduced visibility

## Recommended Implementation Order

1. Introduce a child runner process without `bubblewrap` yet.
2. Move backend execution and tool RPC onto that runner.
3. Once RPC is stable, wrap the child runner with `bubblewrap`.
4. Move child-local builtin tools into the sandboxed runner.
5. Replace copied workspace mounts with parent-authorized real mounts.
6. Add `skill_memory` mount and prompt rules.
7. Finally remove old snapshot mount code and legacy sandbox assumptions.

This order minimizes risk because process-boundary compatibility is the real prerequisite.
