// Bork status plugin for OpenCode
// Writes agent status to .bork/agent-status/{session}.json so the bork
// kanban board can show live status indicators on issue cards.
//
// Requires BORK_SESSION and BORK_STATUS_DIR environment variables, which
// are exported automatically when bork launches an opencode session.

import { writeFileSync, mkdirSync } from "fs"

function getEnv() {
  const statusDir = process.env.BORK_STATUS_DIR
  const session = process.env.BORK_SESSION
  if (!statusDir || !session) return null
  return { statusDir, session, statusFile: `${statusDir}/${session}.json` }
}

function writeStatus(
  statusFile: string,
  status: string,
  activity?: string,
) {
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

export const BorkStatusPlugin = async () => {
  const env = getEnv()
  if (!env) return {}

  return {
    event: async ({ event }: { event: { type: string; properties: any } }) => {
      if (event.type === "session.status") {
        const s = event.properties.status
        if (s.type === "busy") {
          writeStatus(env.statusFile, "Busy")
        } else if (s.type === "idle") {
          writeStatus(env.statusFile, "Idle")
        } else if (s.type === "retry") {
          writeStatus(env.statusFile, "Error", s.message)
        }
      } else if (event.type === "session.error") {
        const msg = event.properties.error?.message
        writeStatus(env.statusFile, "Error", msg)
      } else if (event.type === "permission.updated") {
        writeStatus(
          env.statusFile,
          "WaitingPermission",
          event.properties.title,
        )
      } else if (event.type === "permission.replied") {
        writeStatus(env.statusFile, "Busy")
      }
    },
    "tool.execute.before": async (input: { tool: string }) => {
      writeStatus(env.statusFile, "Busy", input.tool)
    },
  }
}
