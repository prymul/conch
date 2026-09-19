# Rule: Agent teams before generic subagents

When delegating work via the Agent tool in this project, prefer the
specialized custom agents defined in `~/.claude/agents/*.md` (the "agent
team" — e.g. `rust-specialist`, `rust-debugging-agent`, `ci-release-gate`,
`github`, `security-engineer`, `tauri`, `playwright-testing`, etc.) over the
generic built-in agent types (`general-purpose`, `fork`, `Explore`, `Plan`).

- Check the available custom agents for one matching the task's domain first.
- Use that specialized agent via `subagent_type: "<agent-name>"`.
- Only fall back to a generic subagent when no specialized agent in the team
  covers the task.
- When several specialized agents could apply, prefer the most specific one
  (e.g. `rust-debugging-agent` over `rust-specialist` for an existing panic
  or compiler error).
