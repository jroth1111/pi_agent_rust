# Skills

Skills provide specialized instructions and capabilities to the agent. They are defined using the [Agent Skills](https://github.com/Start-Agent-Skills) format.

## Locations

Skills are loaded from:

1. **Global**: `~/.pi/agent/skills/*/SKILL.md` or `~/.pi/agent/skills/*.md`
2. **Project**: `.pi/skills/*/SKILL.md` or `.pi/skills/*.md`
3. **Packages**: Installed packages.

## File Format

A skill is defined in a `SKILL.md` file with YAML frontmatter.

```markdown
---
name: "sql-expert"
description: "Expert at writing and optimizing SQL queries"
disable-model-invocation: false
---
You are an expert SQL developer. When writing queries:
1. Always prefer CTEs over subqueries.
2. Use uppercase for keywords.
3. Check for index usage.
```

### Frontmatter Fields

| Field | Description |
|-------|-------------|
| `name` | Skill ID (must match directory name if in a subdir; mismatches emit a warning). |
| `description` | **Required.** Short description used for selection; empty descriptions are skipped. |
| `disable-model-invocation` | If `true`, the skill is not shown to the model in the system prompt. |

If `name` is omitted, the parent directory name is used.

## Usage

### Auto-Discovery

By default, Pi includes all enabled skills in the system prompt. The model can decide to "activate" a skill by reading its definition file using the `read` tool.

### Explicit Invocation

You can explicitly invoke a skill using the slash command:

```bash
/skill:sql-expert "Optimize this query..."
```

This effectively wraps your prompt with the skill's instructions.

## Configuration

To disable the `/skill:` slash commands, set `enable_skill_commands` to `false` in `settings.json`.

## Self-Improving Skills

Pi now treats skills as observable components instead of static prompt files.

### Automatic Observation

- Explicit `/skill:<name>` invocations are recorded automatically.
- Implicit skill selection is recorded when the agent successfully reads a `SKILL.md` file with the `read` tool.
- Each observation captures the skill version, task preview, activation source, tool failures, and terminal outcome.
- Observations are appended to `.pi/skill-observations.jsonl` and also persisted as `skill/observation.v1` session custom entries when a session is available.
- Explicit user ratings are recorded with `pi skills feedback` and persisted to `.pi/skill-feedback.jsonl`.

### Explicit Feedback

Use explicit feedback when a skill technically completed but the outcome quality was poor:

```bash
pi skills feedback bug-triage --rating 2 --notes "wrong format and missed the failing test"
pi skills feedback bug-triage --rating 1 --session-id sess-123 --notes "used the right tools but made up evidence"
```

When `--session-id` is provided, Pi binds the feedback to the latest observed revision of that skill in the matching session. Otherwise it binds to the current on-disk skill revision.

### Automated Inspection And Fixing

Use the built-in doctor to inspect recorded runs:

```bash
pi skills doctor
pi skills doctor --format json
pi skills doctor --fix
```

`pi skills doctor --fix` applies a managed guardrail block to unhealthy skills using these markers:

```markdown
<!-- PI-SKILL-GUARDRAILS:BEGIN -->
## Operational Guardrails
...
<!-- PI-SKILL-GUARDRAILS:END -->
```

Applied amendments are written to `.pi/skill-amendments.jsonl`.

If a skill has been amended but the new revision has not been observed yet, `pi skills doctor` reports that skill as `PendingData` instead of continuing to judge the old revision as current.

The doctor now evaluates both execution evidence and user feedback. A skill can therefore be marked unhealthy even when tool calls succeeded, if the resulting output repeatedly earned low ratings.

### Success Criteria

- Minimum sample size: 3 observed runs per skill version
- Target success rate: 80% or better
- Consecutive failure threshold: 2 failures in a row triggers intervention
- Minimum feedback sample size: 2 ratings per skill version
- Target average feedback score: 3.5/5 or better
- Amendment evaluation window: 3 post-amend runs minimum
- Promotion threshold: 15 percentage-point success-rate improvement over the prior version
- Feedback promotion threshold: 0.5 points average-rating improvement over the prior version

The automated fixer remains conservative, but it is no longer limited to tool errors. Low-feedback notes now drive guardrails for output contracts, completeness, verification, verbosity, and trigger tightening. Higher-risk semantic rewrites of a skill's core instructions remain a future step.
