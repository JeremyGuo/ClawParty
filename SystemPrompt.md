# System Prompt Refactor Progress

## Goal

Stop treating rendered upstream system prompts as durable session state.
Session state should store structured prompt/runtime state, while AgentHost and
AgentFrame assemble request-time prompts from current code/config plus explicit
snapshots.

## Confirmed Design

- `session_state.messages` must not persist AgentFrame-rendered prompts such as
  `[AgentFrame Runtime]`.
- AgentHost prompt content:
  - Always use latest values for Host static intro, role rules, memory mode,
    current model profile, and available model catalog.
  - Track Identity and User meta with canonical/notified prompt component
    snapshots because they may change frequently through model-editable profile
    files.
  - Snapshot-only at compaction for workspace summary, remote workpaths,
    runtime notes, and PARTCLAW.md.
  - Do not include RuntimeContext in prompt; its fields are already persisted as
    structured `session.json` fields.
- AgentFrame prompt content:
  - Always use latest AgentFrame runtime constants, native capability notices,
    and tool schemas.
  - Track skills metadata with a canonical snapshot and a notified snapshot.
- Emit profile and skill-change notifications only on user-message turn
  boundaries.
- Assistant resume, background auto-resume, and tool-progress loops must not
  advance prompt notification state.
- Obsolete Host dynamic prompt hash fields and old profile/model notification
  fields are removed from runtime state; the `0.28` workdir upgrade cleans them
  from persisted sessions and snapshots.

## Implementation Checklist

- [x] Confirm RuntimeContext fields are recoverable from structured session data.
- [x] Add durable skill prompt snapshot/notified state and workdir upgrade.
- [x] Add durable Identity/User meta prompt snapshot/notified state.
- [x] Move skills metadata prompt assembly to use snapshot state.
- [x] Emit profile and skill metadata notices only on user-message turn
      boundaries.
- [x] Remove RuntimeContext from AgentHost prompt.
- [x] Normalize background persistence so Frame wrapper prompts are not saved.
- [x] Add regression tests for background prompt recursion, RuntimeContext
      removal, and skill notice boundaries.
- [x] Update FEATURES/VERSION for workdir schema impact.
- [x] Run focused tests and format.
