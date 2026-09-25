// OpenCode adapter for the shared guardrail. The rules live in ONE script,
// .claude/hooks/guard-bash.sh (JSON on stdin, exit 2 + reason on stderr to block), which the
// Claude Code PreToolUse hook also runs. Never re-implement rules here; they would drift.
export const AmalgumGuardrails = async ({ $, directory }) => {
  const script = `${directory}/.claude/hooks/guard-bash.sh`
  return {
    "tool.execute.before": async (input, output) => {
      if (input.tool !== "bash") return
      const command = output?.args?.command
      if (typeof command !== "string" || !command) return
      const payload = JSON.stringify({ tool_input: { command } })
      const result = await $`echo ${payload} | ${script}`.nothrow().quiet()
      if (result.exitCode !== 0) throw new Error(result.stderr.toString().trim() || `Blocked by ${script}`)
    },
  }
}
