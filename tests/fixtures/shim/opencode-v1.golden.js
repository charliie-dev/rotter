// rotter review plugin for OpenCode, written by `rotter integration install opencode`.
// Removed by `rotter integration uninstall opencode`; rotter replaces this file only while it is
// byte-for-byte what rotter wrote. OpenCode loads every export as a plugin: keep only one.
import { spawn } from "node:child_process";

const EXE = "/opt/it's \"q\"\\ dir/ünï/rotter";
const TIMEOUT = 90;
const HOST = "opencode";
const MAX_STDOUT = 65536;

// Runs `rotter hook opencode --timeout <n>` with `input` on stdin and resolves to the review
// request, or null for anything else. Never rejects; settles once, at the latest when the timeout
// fires.
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

// OpenCode does not await event handlers, so this one never rejects. On `session.idle` of a
// top-level session it asks rotter and re-prompts with rotter's request through `promptAsync`,
// which returns at once: the in-flight flag is already released when the injected turn runs, and
// its own idle reaches rotter like any other stop, so rotter's cap ends the chain.
export const RotterReview = async (input) => {
  try {
    const client = input?.client;
    const directory = input?.directory;
    return {
      event: async (arg) => {
        try {
          const event = arg?.event;
          if (event === null || typeof event !== "object" || event.type !== "session.idle") {
            return;
          }
          const id = event.properties?.sessionID;
          if (inFlight || typeof id !== "string") return;
          // A child (subagent) session ends inside its parent's turn.
          const found = await client.session.get({ path: { id } });
          const info = found?.data;
          if (info === null || typeof info !== "object" || info.parentID != null) return;
          const text = await review(id, directory);
          if (text === null) return;
          const parts = [{ type: "text", text }];
          await client.session.promptAsync({ path: { id }, body: { parts } });
        } catch {}
      },
    };
  } catch {
    return {};
  }
};
