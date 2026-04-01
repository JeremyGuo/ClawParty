---
name: skill-creator
description: Create or update a skill folder with a valid SKILL.md frontmatter/body format and keep the instructions concise.
---

# Skill Creator

Use this skill when creating or revising a skill under the local runtime skills directory.

Requirements:
- Every skill must contain a SKILL.md file.
- SKILL.md must begin with YAML frontmatter containing:
  - name
  - description
- Keep the body concise and procedural.
- Prefer putting durable workflow instructions in SKILL.md.
- Do not create extra documentation files unless they are directly needed by the skill.

Recommended layout:
- SKILL.md
- references/ only if extra material is truly needed
- scripts/ only when deterministic execution is important
