# Bubblewrap Sandbox Plan

## Goal

Define the concrete `bubblewrap` plan for the new sandboxed child runner.

This document covers:

- child process visibility
- mount layout
- real workspace mounts
- `.skill_memory`
- cleanup and platform boundaries

It assumes the RPC bridge described in [SANDBOX_RPC_DESIGN.md](/Users/jeremyguo/Projects/ClawParty2.0/SANDBOX_RPC_DESIGN.md) already exists.

## Target Platform

Primary target:

- Linux with `bubblewrap` installed

This should be explicit in config and startup diagnostics.

If `bubblewrap` is missing:

- sandbox mode should fail closed when enabled
- not silently fall back to unsandboxed subprocess mode

## Non-Goals

- macOS support for `bubblewrap`
- rootful container replacement
- namespace tricks beyond what is needed for file visibility

## Sandbox Launch Strategy

Every turn subprocess is launched by the parent with:

```bash
bwrap \
  --die-with-parent \
  --new-session \
  ...
  /path/to/agent_host run-child ...
```

### Required properties

- child dies when parent dies
- child gets a clean mount namespace
- cwd is `/workspace`
- host absolute paths are not exposed directly inside the sandbox

## Filesystem Visibility Model

### Visible

The child should see a minimal Linux runtime plus explicit mounts.

Recommended mounts:

- `--ro-bind /usr /usr`
- `--ro-bind /bin /bin`
- `--ro-bind /sbin /sbin`
- `--ro-bind /lib /lib`
- `--ro-bind /lib64 /lib64` when present
- `--ro-bind /etc /etc`
- `--dev /dev`
- `--proc /proc`
- `--tmpfs /tmp`
- `--tmpfs /var/tmp`

### Not visible

Do not expose:

- `/home`
- `/Users`
- arbitrary repo paths
- host workdir root
- host metadata directories

unless explicitly mounted.

## Internal Mount Layout

Inside the child use stable virtual paths:

```text
/workspace
/workspace/upload
/workspace/.skill_memory
/workspace/mounts/<name>
```

This keeps prompts and tool output stable and host-path agnostic.

## Main Workspace Mount

The current workspace root should be mounted as:

```text
/workspace
```

Recommended initial implementation:

- host path: `<workdir>/workspaces/<workspace_id>/files`
- child path: `/workspace`
- mount mode: `rw`

Uploads should exist inside the same workspace tree and be visible as:

```text
/workspace/upload
```

## Skill Memory Mount

### Source

```text
<workdir>/rundir/skill_memory
```

### Destination

```text
/workspace/.skill_memory
```

### Mode

- `rw`

### Design note

This is intentionally shared mutable state.
It is not hidden from the child.
The restriction is semantic:

- skills may use it
- agents should not use it proactively unless instructed by a skill

This should be communicated in system prompt and skill guidance, not via filesystem ACL.

## Workspace Mounts

The old copied snapshot model should be replaced.

## Required behavior

When the agent uses `workspace_mount`:

- the parent authorizes the request
- the requested workspace becomes visible inside the current child at:
  - `/workspace/mounts/<name>`
- mode is either:
  - `ro`
  - or `rw`

### Consistency goal

This must be a real mounted view, not a copied snapshot.

So if host-authorized changes happen to the source workspace:

- the mounted view updates immediately

## Security requirement for mounts

Do not solve this by prebinding all workspaces and hiding them by convention.

That would leak discoverability to shell commands.

Instead use one of these approaches.

### Preferred approach: FD passing + child-side bind mount

1. Parent opens the source workspace directory.
2. Parent sends the directory FD to the child over the Unix socket.
3. Child creates `/workspace/mounts/<name>`.
4. Child performs the bind mount from the received FD-backed path.
5. Child remounts it read-only if needed.

This is the cleanest authority model.

### Simpler fallback: relaunch child with updated mount plan

If live FD-based mounting is too complex for v1 of this redesign:

- parent can treat a mount request as a turn boundary
- persist the new mount grant
- end the current turn
- relaunch the next child with the updated mount table

This is operationally simpler but less fluid.

Recommendation:

- long-term target: live FD-based real mounts
- short-term fallback if necessary: mount-plan-on-next-turn

## Real RW Mount Policy

### Rules

- many readers allowed
- one writer allowed
- parent-owned registry is authoritative

Parent registry should track:

- source workspace id
- target workspace id
- mount name
- mode
- granted_at
- active_session_id or child id

### Child cannot self-upgrade

If a mount is granted `ro`, the child must not be able to make it `rw` itself.

That means remount flags must be controlled entirely by the parent-issued plan.

## Host Metadata Isolation

These should not be mounted into the child:

- `sandbox/workspaces.json`
- `sandbox/mounts.json`
- `sandbox/workspace_meta/*`

The child should only observe host metadata through tools or RPC responses.

This keeps the control plane parent-owned.

## Temporary Runtime State

Turn-local runtime state should live under:

```text
<workdir>/runtime/<workspace_id>/<turn_id>/
```

Only the pieces needed by the child should be mounted in.

For example:

- local process state for `exec_*`
- transient request artifacts

This replaces any accidental runtime state inside repository directories.

## Network Access

By default the child needs outbound network access for:

- LLM requests
- `web_fetch`
- `download_file`

So the initial `bubblewrap` plan should not disable networking.

This can become configurable later.

## Package Installation Clarification

The child runs as the same uid/gid as the parent launcher.

So:

- it can only write where that user can write
- it cannot magically become root

That means:

- `apt install ...` usually still fails
- local environment bootstrapping within writable mounted paths still works
- user-level package managers and downloads still work

This should be documented clearly so sandbox expectations stay realistic.

## Suggested Launch Builder

Add a dedicated module:

- `agent_host/src/sandbox.rs`

Responsibilities:

- detect `bubblewrap`
- build command line args
- create per-turn runtime dirs
- compute mount plan
- launch child

Suggested types:

- `SandboxLaunchPlan`
- `SandboxMount`
- `SandboxMode`

`SandboxMount` fields:

- `host_path`
- `child_path`
- `mode`
- `required`

Modes:

- `ro_bind`
- `rw_bind`
- `tmpfs`
- `proc`
- `dev`

## Platform Guardrails

Startup checks should fail if:

- sandbox mode is enabled
- platform is unsupported
- or `bubblewrap` binary is missing

Recommended error:

```text
sandbox mode requires Linux with bubblewrap installed
```

## Failure Handling

### Launch failure

If `bubblewrap` child launch fails:

- fail the turn
- log the launch command summary
- do not fall back to in-process execution

### Mount failure

If a required mount fails:

- fail the turn
- include which mount failed

### Child crash

If the child dies unexpectedly:

- parent marks the turn failed
- no partial mount state should survive beyond the process

## Recommended Implementation Order

### Stage 1

Wrap child runner with `bubblewrap` using only:

- main workspace
- upload
- skill memory
- minimal runtime dirs

No live extra mounts yet.

### Stage 2

Add read-only real workspace mounts.

### Stage 3

Add RW mounts with parent authorization.

### Stage 4

Remove old snapshot mount logic entirely.

## Open Questions

These should be resolved during implementation:

- live FD-based mount now or relaunch-on-next-turn first
- whether `skill_memory` should be mounted as a subpath or symlink target
- whether `exec_*` runtime state should live per workspace or per turn
- whether background agents get the same mount plan semantics as foreground by default

## Recommended Decision

For the first secure upgrade, do this:

- parent-child RPC first
- `bubblewrap` child runner second
- workspace root and `.skill_memory` mounts immediately
- real extra workspace mounts after child runner is stable

This keeps the critical path defensible and avoids mixing process-boundary and mount-complexity changes in one shot.
