# New Session State Spec

Status: Draft

This document describes a proposed replacement for the current foreground session persistence and recovery model.

The goal is to replace the current multi-track recovery design with a single durable session state plus a small amount of host-only runtime metadata.

## Goals

- Use one primary durable session structure as the source of truth.
- Make recovery depend on transcript state, not scattered checkpoint types.
- Keep message ordering deterministic under user interruption.
- Preserve interruptibility, tool continuation, compaction, and long-running task reuse.
- Make persistence consistent: update session state, then persist it immediately.

## Non-Goals

- This spec does not redesign the prompt format itself.
- This spec does not require changing tool worker storage formats on day one.
- This spec does not describe background agent or cron state in detail.

## Core Model

```rust
struct SessionState {
    messages: Vec<ChatMessage>,
    pending_messages: Vec<ChatMessage>,
    state: SessionPhase,
    errno: Option<SessionErrno>,
    errinfo: Option<String>,
    usage: SessionUsage,
}

enum SessionPhase {
    End,
    Yielded,
}
```

## Meaning Of Each Field

- `messages`
  Stable transcript that has already been accepted into session history.
  This is durable committed state.

- `pending_messages`
  Durable queue of messages that have not yet participated in the next upstream API request.
  This is not "new user messages only".
  It can contain:
  - user follow-up messages received while a micro-round was running
  - tool result messages that have been produced but not yet folded into the next API request
  - other runtime-generated messages that are semantically waiting for the next request

- `state`
  Persistent resumability marker.
  - `End`: the session is at a completed assistant boundary
  - `Yielded`: the session has a durable recovery point and can continue from there

- `errno` and `errinfo`
  Why the session most recently stopped in a non-clean way.
  These describe the stop reason but do not change transcript truth.

- `usage`
  Session-local stats and billing information.

## Host-Only Runtime State

The persistent state intentionally does not include an explicit `Running` state.

`Running` is host-only transient state maintained by `agent_host`, for example:

- whether a runtime process is currently executing this session
- which sandbox/runtime instance is bound to this session
- cancellation/yield handles for the currently running micro-round

This state is not durable and does not participate in recovery semantics.

## Fundamental Invariants

1. `SessionState` is the only durable foreground source of truth for session progress.
2. Every micro-round return updates `SessionState` first, then persists it.
3. `pending_messages` is ordered by semantic/causal order, not raw arrival timestamp.
4. A micro-round never returns in the middle of a partially settled tool batch.
5. If a tool batch has started, then when the micro-round returns, every tool call in that batch is already settled.
6. A tool result never disappears.
   It must exist in either `messages` or `pending_messages`.
7. Compression must only cut at legal boundaries.
   It must not cut across an unresolved tool-call/tool-result dependency.

## Clarification On Unresolved Tool Calls

This design allows `messages` to end with an assistant message that contains tool calls whose results have not yet been moved into `messages`, as long as:

- those tool calls are already a stable historical fact
- their corresponding tool results are preserved in `pending_messages`
- compression logic treats that suffix as a non-cuttable boundary until the dependency is resolved

This means `messages` is a stable transcript, but not necessarily a fully "closed" transcript.

The stability guarantee is:

- facts already accepted into `messages` stay true
- unresolved continuation-critical data must still be reachable from `pending_messages`

## Micro-Round Model

The runtime no longer behaves as "run an entire user turn and maybe checkpoint internally".

Instead, each execution step returns at a micro-round boundary.

A micro-round return is allowed only at one of these boundaries:

1. Terminal assistant answer produced with no pending tool batch.
   Return `state = End`.

2. A full tool batch has settled and the runtime wants the host to persist the new continuation point before the next model request.
   Return `state = Yielded`.

3. A recoverable failure occurred after preserving a valid continuation point.
   Return `state = Yielded` plus `errno/errinfo`.

`Compaction completed` is not a standalone return boundary.
Compaction is an internal step inside one of the three boundaries above.

## Entering A Micro-Round

When the runtime starts or resumes a session, it conceptually operates on:

```rust
next_request_messages = messages + pending_messages
```

Before the next upstream call, the runtime may:

- compact a legal prefix of `messages`
- compact a legal prefix of `messages + pending_messages` when needed
- reorder nothing
- consume some or all of `pending_messages` into the next request

After the runtime returns, `SessionState` is rewritten to the new committed state.

## Ordering Rules

`pending_messages` must preserve semantic order.

This is especially important when a user interrupt arrives during a tool batch.

Rule:

- tool results generated by the currently executing micro-round come before user follow-up messages received during that same micro-round, because the tool results causally close the already-issued assistant tool batch

Example:

1. assistant emits tool calls `A` and `B`
2. user sends follow-up `U` while `A` and `B` are still running
3. `A` and `B` finish

Then the correct `pending_messages` order is:

1. tool result for `A`
2. tool result for `B`
3. user follow-up `U`

This is causal order, not arrival order.

## Interruption Rules

If a user message arrives while no micro-round is running:

- if `state = End`, start a new micro-round directly
- if `state = Yielded`, resume from the stored session state and continue

If a user message arrives while a micro-round is running:

- the host requests interruption/cancellation for interruptible work as needed
- the incoming user message is appended to `pending_messages`
- the running micro-round is allowed to finish at its next legal return boundary
- when the micro-round returns successfully with `Yielded`, the host may annotate the queued user message as interrupted follow-up before the next request if prompt policy still requires that marker

The marker policy is prompt-layer behavior, not session-state structure.

## Error Model

Errors are represented as:

- `state = Yielded`
- `errno = Some(...)`
- `errinfo = Some(...)`

The session remains resumable unless the host decides the session is unrecoverable for external reasons.

### API Failure

If an upstream API call fails but a valid continuation point still exists:

- return `state = Yielded`
- preserve durable continuation in `messages` and `pending_messages`
- set `errno` and `errinfo`

### Threshold Compaction Failure

If threshold compaction fails before certain newly produced tool results are folded into `messages`:

- keep already committed transcript in `messages`
- preserve uncommitted tool results in `pending_messages`
- return `state = Yielded`
- set `errno` and `errinfo`

### Tool-Wait Timeout Observation

If a timeout-observation path fires during tool waiting:

- wait until the current tool batch has settled according to the micro-round invariant
- place the resulting tool outputs into `messages` or `pending_messages` as appropriate
- return `state = Yielded`
- set `errno` and `errinfo`

### Idle Compaction Failure

Idle compaction does not require a special retry state in persistent session structure.

If idle compaction fails while the session is otherwise complete:

- keep `state = End`
- keep session transcript unchanged except for any safe committed compaction result
- set `errno` and `errinfo`
- do not repeatedly nag the user
- at most one retry may be attempted by host policy before normal user traffic resumes

## Compression Rules

Compression is allowed only on legal cut boundaries.

A legal cut boundary must satisfy:

- it does not split an unresolved assistant tool-call message from continuation-critical state
- it does not lose any still-needed ids or references for active runtime tasks
- it preserves enough information to continue unfinished work safely

In practice:

- if the tail of `messages` ends in a continuation-critical assistant tool-call segment, compaction must not cut through that segment
- if necessary tool results are still in `pending_messages`, the compactor must respect that dependency and cut earlier

This is close to the current algorithmic intent, but the legality condition becomes an explicit invariant of the new design.

## Long-Running Runtime Tasks

Examples:

- `exec`
- file download
- image generation/edit tasks that survive across turns
- hosted subagents

These tasks already have authoritative runtime state outside the chat transcript, for example:

- worker metadata files under runtime state directories
- subagent state files

The new session model should not duplicate their full operational state inside `SessionState`.

### Rule

- the authoritative task state remains in runtime-state storage
- the transcript may contain ids and references as conversation facts
- before the next model request, the host/runtime may inject a derived summary of active runtime tasks so the model can continue using those ids safely

This means task ids do not need to become first-class top-level durable fields in `SessionState`.

They remain:

- part of conversation facts when they were mentioned in messages
- derivable from runtime state when still active

## Persistence Contract

For every micro-round return:

1. runtime returns the updated session state payload
2. host replaces the old durable `SessionState`
3. host persists it immediately
4. only after persistence succeeds does the host proceed with follow-up scheduling or user-visible messaging

This keeps durability and consistency aligned.

## Suggested Runtime/Host Exchange

Conceptually, `agent_frame` and `agent_host` only need to exchange one primary structure:

```rust
struct SessionStepResult {
    session_state: SessionState,
}
```

Additional process-local metadata may still exist for transport or control, but it is not part of durable recovery semantics.

## Migration Direction

The current system has multiple recovery tracks:

- persisted `agent_messages`
- `pending_continue`
- provider-facing `response_checkpoint`
- runtime-local stable report / checkpoint handling

The new direction should collapse the durable recovery surface onto `SessionState`.

A provider-specific continuation token may still exist as an optimization, but it should not be a separate durable recovery truth source.

## Open Questions

1. Should a provider continuation token still be kept as a non-authoritative optimization field alongside `SessionState`, or should the first implementation remove it entirely?
2. Should `pending_messages` be stored as generic `ChatMessage`, or should there be a stricter tagged enum for `UserUpdate`, `ToolResult`, and `SyntheticMessage`?
3. Should interrupt markers such as `[Interrupted Follow-up]` be materialized into `pending_messages`, or injected only when constructing the next prompt?
4. When `state = End` and `errno` is present from idle compaction failure, exactly when should host clear the error fields?
5. If `agent_frame` becomes a long-lived background process, should the per-session runtime binding survive ordinary host reloads, or only ordinary turn boundaries?

## Current Recommendation

Implement the first version with these constraints:

- one durable `SessionState`
- transient `Running` kept only in `agent_host`
- micro-round return after full tool-batch settlement
- no standalone compaction return
- `pending_messages` ordered by causal order
- long-running task truth remains in runtime-state storage
- active task ids exposed back to the model through transcript facts plus derived runtime summaries
