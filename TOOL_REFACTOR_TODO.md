# Tool Refactor TODO

Goal: builtin tools should be concrete tool types that implement `BaseTool` / `ProviderNativeTool`, instead of sharing a definition-only wrapper plus string dispatch. File-level static state such as shell process tables can stay in the tool module when that is the clearest owner.

Status legend:

- `[x]` Concrete tool entry registered in the builtin catalog.
- `[runtime]` Dynamic Ext tool surface; no fixed builtin list.

## Base Tools

### File

- `[x]` `apply_patch`
- `[x]` `shell_make_visible`
- `[x]` `attachment_make_visible`

### Process

- `[x]` `shell_exec`
- `[x]` `shell_write_stdin`
- `[x]` `shell_stop`

### Web

- `[x]` `web_fetch`
- `[x]` `web_search`

### Media

- `[x]` `image_view`
- `[x]` `pdf_view`
- `[x]` `audio_view`
- `[x]` `image_analysis`
- `[x]` `image_stop`
- `[x]` `pdf_analysis`
- `[x]` `pdf_stop`
- `[x]` `audio_analysis`
- `[x]` `audio_stop`
- `[x]` `image_generation` provider-backed mode
- `[x]` `image_generation_stop`

### Host / Bridge

- `[x]` `update_plan`
- `[x]` `subagent_start`
- `[x]` `subagent_kill`
- `[x]` `subagent_join`
- `[x]` `background_agent_start`
- `[x]` `background_agents_list`
- `[x]` `terminate`

### Cron / Bridge

- `[x]` `cron_tasks_list`
- `[x]` `cron_task_get`
- `[x]` `cron_task_create`
- `[x]` `cron_task_update`
- `[x]` `cron_task_remove`

### Memory / Bridge

- `[x]` `memory_search`
- `[x]` `memory_write`
- `[x]` `memory_update`
- `[x]` `memory_delete`

### Skill / Bridge

- `[x]` `skill_load`
- `[x]` `skill_create`
- `[x]` `skill_update`
- `[x]` `skill_delete`

## Provider Native Tools

- `[x]` `image_generation` native mode

## Ext Tools

- `[runtime]` `ToolEntry::Ext(Arc<dyn ExtTool>)`; names are supplied by each runtime extension's `definition()`.
- `[runtime]` Test-only examples currently include `ShellEchoExtTool` and `VisibilityExtTool`.

## Notes

- Keep per-call runtime data in `ToolExecutionContext`: workspace/data roots, remote mode, bridge, model helpers, and cancellation token.
- Keep shared cross-call state where it naturally belongs. Current examples are `SHELL_MANAGER` in `process_tools.rs`, `MEDIA_JOBS` in `media_tools.rs`, and bridge request id generation in `tool_catalog/mod.rs`.
- Builtin registration now uses concrete `ToolEntry` values. Definition-only wrappers and tool-name string execution dispatch have been removed from the builtin path.
