// hc-status: report opencode's status to herdr-core (hc) through OSC 7501,
// the Program Status Protocol, when opencode runs inside an hc terminal.
//
// opencode v2.1.4 already reports `working`, `idle` and `blocked` natively
// (OpenTUI speaks 7501 when the terminal answers its capability query, which
// hc does). This plugin adds what the native reporter does not send:
//
//   - `idle` once at startup, so an hc template with `ready: status`
//     (the default) sees its first report immediately.
//   - `done` when a session's execution succeeds (native only says `idle`).
//   - `error` with the failure message when an execution fails.
//   - `working` with a note while a failed model call is being retried.
//
// It is a TUI part: only the TUI process is attached to the hc terminal, and
// only there can OSC 7501 reach hc. (Server parts run in a detached server
// process with no tty; a single-file plugin loads as a server part only.)
// The TUI may load this module more than once; a global guard keeps one
// instance reporting.
//
// No-op unless HC_TERMINAL_ID is set, so a user's normal opencode is
// unaffected. No dependencies.

import fs from "node:fs"

const DEBUG = process.env.HC_STATUS_DEBUG // set to a file path to log

function log(...args) {
  if (!DEBUG) return
  try {
    fs.appendFileSync(DEBUG, new Date().toISOString() + " " + args.join(" ") + "\n")
  } catch {}
}

function esc(v) {
  // PSP: title and msg travel base64-encoded so any content is safe.
  return Buffer.from(String(v ?? ""), "utf8").toString("base64")
}

export default {
  id: "hc-status",
  setup(ctx) {
    if (!process.env.HC_TERMINAL_ID) {
      log("HC_TERMINAL_ID not set; hc-status inactive")
      return
    }
    if (globalThis.__hcStatusActive) {
      log("already active in this process; skipping duplicate load")
      return
    }
    globalThis.__hcStatusActive = true
    log("hc-status active, terminal", process.env.HC_TERMINAL_ID)

    // --- write OSC 7501 to the terminal -----------------------------------
    let fd
    try {
      fd = fs.openSync("/dev/tty", "w")
    } catch (e) {
      log("/dev/tty unavailable, falling back to stdout:", String(e))
      fd = process.stdout.fd
    }
    const report = (fields) => {
      const parts = []
      for (const [k, v] of Object.entries(fields)) {
        if (v === undefined || v === null || v === "") continue
        parts.push(`${k}=${v}`)
      }
      try {
        fs.writeSync(fd, Buffer.from(`\x1b]7501;${parts.join(":")}\x1b\\`))
      } catch (e) {
        log("write failed:", String(e))
      }
      log("report", JSON.stringify(fields))
    }

    // Sessions with a running execution. The terminal's own state is
    // derived from them: any execution running -> working, the last one
    // succeeding -> done, failing -> error.
    const running = new Set()
    const timers = new Set()
    const later = (ms, fn) => {
      const t = setTimeout(() => {
        timers.delete(t)
        fn()
      }, ms)
      timers.add(t)
    }

    report({ state: "idle", app: "opencode" })

    // --- events ------------------------------------------------------------
    let listen
    try {
      listen = ctx.data.listen
    } catch (e) {
      log("ctx.data.listen unavailable:", String(e))
      return
    }
    const stop = listen((ev) => {
      const details = ev?.details ?? ev ?? {}
      const type = details.type ?? ev?.name
      const d = details.data ?? {}
      switch (type) {
        case "session.execution.started":
          running.add(d.sessionID)
          break
        case "session.execution.succeeded":
          running.delete(d.sessionID)
          if (running.size === 0) {
            // The native reporter writes root `idle` at the same moment;
            // let ours land last so hc rests on `done`.
            later(300, () => report({ state: "done", app: "opencode" }))
          }
          break
        case "session.execution.interrupted":
          // reason: user | shutdown | superseded | inactivity. Not `done`:
          // the work did not finish. `idle` matches what the native
          // reporter sends.
          running.delete(d.sessionID)
          if (running.size === 0) {
            later(300, () => report({ state: "idle", app: "opencode" }))
          }
          break
        case "session.execution.failed": {
          running.delete(d.sessionID)
          const msg = String(d.error?.message ?? "execution failed").slice(0, 500)
          const send = () => {
            if (running.size > 0) return // other work continues; not stuck
            report({ state: "error", app: "opencode", msg: esc(msg) })
          }
          send()
          // once more, in case the native reporter's `idle` lands after ours
          later(2000, send)
          break
        }
        case "session.retry.scheduled":
          report({
            state: "working",
            app: "opencode",
            msg: esc(`retrying after error (attempt ${d.attempt ?? "?"})`),
          })
          break
      }
    })

    return () => {
      log("hc-status cleanup")
      for (const t of timers) clearTimeout(t)
      try {
        globalThis.__hcStatusActive = false
        stop?.()
      } catch {}
    }
  },
}
