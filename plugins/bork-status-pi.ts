// Bork status extension for Pi (https://pi.dev)
// Writes agent status to .bork/agent-status/{session}.json so the bork
// kanban board can show live status indicators on issue cards.
//
// Requires BORK_SESSION and BORK_STATUS_DIR environment variables, which
// are exported automatically when bork launches a pi session.
//
// Pi has no native permission/approval lifecycle events, so only Busy/Idle
// statuses are reported (same limitation as the Codex hooks).

import { writeFileSync, mkdirSync } from "node:fs"

function getEnv() {
  const statusDir = process.env.BORK_STATUS_DIR
  const session = process.env.BORK_SESSION
  if (!statusDir || !session) return null
  return { statusDir, session, statusFile: `${statusDir}/${session}.json` }
}

function writeStatus(statusFile: string, status: string, activity?: string) {
  const data = JSON.stringify({
    status,
    ...(activity ? { activity } : {}),
    updated_at: Date.now(),
  })
  try {
    writeFileSync(statusFile, data)
  } catch {
    // Status dir might not exist yet (race with bork startup)
    try {
      const dir = statusFile.substring(0, statusFile.lastIndexOf("/"))
      mkdirSync(dir, { recursive: true })
      writeFileSync(statusFile, data)
    } catch {
      // Silently fail - bork status is best-effort
    }
  }
}

export default function (pi: any) {
  const env = getEnv()
  if (!env) return

  pi.on("session_start", async () => {
    writeStatus(env.statusFile, "Idle")
  })

  pi.on("agent_start", async () => {
    writeStatus(env.statusFile, "Busy")
  })

  pi.on("tool_execution_start", async (event: { toolName?: string }) => {
    writeStatus(env.statusFile, "Busy", event?.toolName)
  })

  pi.on("agent_end", async () => {
    writeStatus(env.statusFile, "Idle")
  })
}
