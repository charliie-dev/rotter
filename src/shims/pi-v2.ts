// rotter review extension for Pi, written by `rotter integration install pi`.
// Removed by `rotter integration uninstall pi`; rotter replaces this file only while it is
// byte-for-byte what rotter wrote. Plain JavaScript, so Pi's loader and Node run it as is.
import { spawn } from "node:child_process";

const EXE = __ROTTER_EXE__;
const TIMEOUT = __ROTTER_TIMEOUT__;
const HOST = "pi";
const MAX_STDOUT = 65536;

// Never rejects; settles once, at the latest when the timeout fires.
function ask(input) {
  return new Promise((resolve) => {
    let settled = false;
    let exited = false;
    let overflow = false;
    let timer = null;
    let child = null;
    let size = 0;
    const chunks = [];
    const finish = (value) => {
      try {
        if (settled) return;
        settled = true;
        if (timer !== null) clearTimeout(timer);
        resolve(value);
      } catch {}
    };
    // The whole process group, and only until rotter's exit is seen (its id may be reused).
    const group = () => {
      try {
        if (child !== null && typeof child.pid === "number") process.kill(-child.pid, "SIGKILL");
      } catch {}
    };
    const kill = () => {
      if (!exited) group();
    };
    try {
      const env = { PATH: typeof process.env.PATH === "string" ? process.env.PATH : "" };
      if (typeof process.env.LANG === "string") env.LANG = process.env.LANG;
      child = spawn(EXE, ["hook", HOST, "--timeout", String(TIMEOUT)], {
        shell: false,
        detached: true,
        env,
        stdio: ["pipe", "pipe", "ignore"],
      });
      child.on("error", () => {
        try {
          finish(null);
        } catch {}
      });
      child.stdin.on("error", () => {});
      child.stdout.on("error", () => {});
      child.stdout.on("data", (chunk) => {
        try {
          if (overflow) return;
          size += chunk.length;
          if (size > MAX_STDOUT) {
            overflow = true;
            chunks.length = 0;
            kill();
            finish(null);
            return;
          }
          chunks.push(chunk);
        } catch {}
      });
      child.on("exit", () => {
        try {
          // A descendant left in the group can hold stdout open, and `close` would wait for it
          // until the timeout. While any member lives, the group keeps its id.
          group();
          exited = true;
        } catch {}
      });
      child.on("close", () => {
        try {
          if (overflow) return finish(null);
          const reply = JSON.parse(Buffer.concat(chunks).toString("utf8"));
          finish(
            reply !== null && typeof reply === "object" && typeof reply.continue === "string"
              ? reply.continue
              : null,
          );
        } catch {
          finish(null);
        }
      });
      timer = setTimeout(() => {
        try {
          kill();
          finish(null);
        } catch {}
      }, TIMEOUT * 1000);
      timer.unref();
      child.stdin.end(JSON.stringify(input));
    } catch {
      kill();
      finish(null);
    }
  });
}

// One run at a time per session. The per-session cap on consecutive requests lives in rotter
// itself, which keeps it on disk and fails closed; a second count here would silence one more stop
// than it.
const inFlight = new Set();

async function review(session, cwd) {
  if (typeof session !== "string" || typeof cwd !== "string") return null;
  if (inFlight.has(session)) return null;
  inFlight.add(session);
  try {
    return await ask({ session_id: session, cwd });
  } finally {
    inFlight.delete(session);
  }
}

export default function (pi) {
  try {
    pi.on("agent_before_settle", async (event, ctx) => {
      try {
        if (event === null || typeof event !== "object" || event.outcome !== "completed") {
          return undefined;
        }
        const text = await review(ctx.sessionManager.getSessionId(), ctx.cwd);
        if (text === null) return undefined;
        const entries = Array.isArray(event.entries) ? event.entries : [];
        return {
          entries: [
            ...entries,
            { type: "custom_message", customType: "rotter-review", content: text, display: true },
          ],
          continue: true,
        };
      } catch {
        return undefined;
      }
    });
  } catch {}
}
