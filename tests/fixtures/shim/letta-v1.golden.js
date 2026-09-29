// rotter review mod for Letta Code, written by `rotter integration install letta`.
// Removed by `rotter integration uninstall letta`; rotter replaces this file only while it is
// byte-for-byte what rotter wrote.
import { spawn } from "node:child_process";

const EXE = "/opt/it's \"q\"\\ dir/ünï/rotter";
const TIMEOUT = 90;
const HOST = "letta";
const MAX_STDOUT = 65536;

// Runs `rotter hook letta --timeout <n>` with `input` on stdin and resolves to the review request,
// or null for anything else. Never rejects; settles once, at the latest when the timeout fires.
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
    // The whole process group, and only while rotter has not exited (its id may be reused).
    const kill = () => {
      try {
        if (child !== null && !exited && typeof child.pid === "number") {
          process.kill(-child.pid, "SIGKILL");
        }
      } catch {}
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

// One run at a time. The per-session cap on consecutive requests lives in rotter itself, which
// keeps it on disk and fails closed; a second count here would silence one more stop than it.
let inFlight = false;

async function review(session, cwd) {
  if (inFlight || typeof session !== "string" || typeof cwd !== "string") return null;
  inFlight = true;
  try {
    return await ask({ session_id: session, cwd });
  } finally {
    inFlight = false;
  }
}

export default function activate(letta) {
  try {
    if (!letta?.capabilities?.events?.turns) return undefined;
    return letta.events.on("turn_end", async (event, ctx) => {
      try {
        if (event === null || typeof event !== "object" || event.stopReason !== "end_turn") {
          return undefined;
        }
        const session =
          typeof event.conversationId === "string" ? event.conversationId : ctx?.sessionId;
        const text = await review(session, ctx?.cwd);
        return text === null ? undefined : { continue: text };
      } catch {
        return undefined;
      }
    });
  } catch {
    return undefined;
  }
}
