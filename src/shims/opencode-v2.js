// rotter review plugin for OpenCode, written by `rotter integration install opencode`.
// Removed by `rotter integration uninstall opencode`; rotter replaces this file only while it is
// byte-for-byte what rotter wrote. OpenCode loads every export as a plugin: keep only one.
import { spawn } from "node:child_process";

const EXE = __ROTTER_EXE__;
const TIMEOUT = __ROTTER_TIMEOUT__;
const HOST = "opencode";
const MAX_STDOUT = 65536;
// How long stdout may stay open after rotter exits before the shim ends what holds it.
const LINGER_MS = 500;
// Messages read first on a stop: usually the whole turn. A longer turn is read in full.
const RECENT = 50;

// Never rejects; settles once, at the latest when the timeout fires.
function ask(input) {
  return new Promise((resolve) => {
    let settled = false;
    let exited = false;
    let overflow = false;
    let timer = null;
    let linger = null;
    let child = null;
    let size = 0;
    const chunks = [];
    const finish = (value) => {
      try {
        if (settled) return;
        settled = true;
        if (timer !== null) clearTimeout(timer);
        if (linger !== null) clearTimeout(linger);
        resolve(value);
      } catch {}
    };
    // rotter's whole process group.
    const group = () => {
      try {
        if (child !== null && typeof child.pid === "number") process.kill(-child.pid, "SIGKILL");
      } catch {}
    };
    // Until rotter's exit is seen, its pid, and with it the group's id, cannot be reused.
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
          exited = true;
          if (settled) return;
          // Stdout still open: something rotter started holds it, and `close` would wait for it
          // until the timeout. A group member that holds it keeps the group's id from reuse; one
          // that left the group survives, but the pipe is closed on this side.
          linger = setTimeout(() => {
            try {
              group();
              child.stdout.destroy();
            } catch {}
          }, LINGER_MS);
          linger.unref();
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

// OpenCode does not await event handlers, so this one never rejects. On `session.idle` of a
// top-level session it asks rotter and re-prompts with rotter's request through `promptAsync`,
// which returns at once: the session's in-flight mark is already released when the injected turn
// runs, and its own idle reaches rotter like any other stop, so rotter's cap ends the chain.
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
          if (typeof id !== "string" || inFlight.has(id)) return;
          const found = await client.session.get({ path: { id } });
          const info = found?.data;
          if (info === null || typeof info !== "object" || info.parentID != null) return;
          // Ask only after a turn that ended normally (docs/hosts.md lists the conditions).
          const read = async (query) => {
            const listed = await client.session.messages({ path: { id }, query });
            return Array.isArray(listed?.data) ? listed.data : [];
          };
          const isUser = (message) => message?.info?.role === "user";
          let messages = await read({ limit: RECENT });
          if (messages.length >= RECENT && !messages.some(isUser)) messages = await read({});
          const entry = messages[messages.length - 1];
          const last = entry?.info;
          if (last === null || typeof last !== "object" || last.role !== "assistant") return;
          if (last.error != null || typeof last.time?.completed !== "number") return;
          if (typeof last.finish !== "string" || ["tool-calls", "unknown"].includes(last.finish)) {
            return;
          }
          const own = Array.isArray(entry.parts) ? entry.parts : [];
          const orphan = (part) =>
            part.state?.status === "error" && part.state?.metadata?.interrupted === true;
          const pending = (part) =>
            part?.type === "tool" && !part.metadata?.providerExecuted && !orphan(part);
          if (own.some(pending)) return;
          const user = messages.findLast(isUser)?.info;
          if (typeof user?.id !== "string" || last.parentID !== user.id) return;
          const reviewed = last.id;
          if (typeof reviewed !== "string") return;
          const agent = user.agent;
          const model = user.model;
          if (typeof agent !== "string" || typeof model?.providerID !== "string") return;
          if (typeof model.modelID !== "string") return;
          const variant = model.variant;
          if (variant !== undefined && typeof variant !== "string") return;
          const text = await review(id, directory);
          if (text === null) return;
          // rotter runs for seconds, and the user can start and stop another turn meanwhile.
          // Re-prompt only while the message rotter was asked about is still the last one.
          const now = await read({ limit: 1 });
          if (now[now.length - 1]?.info?.id !== reviewed) return;
          const parts = [{ type: "text", text }];
          const chosen = { providerID: model.providerID, modelID: model.modelID };
          const body = { agent, model: chosen, parts };
          if (variant !== undefined) body.variant = variant;
          await client.session.promptAsync({ path: { id }, body });
        } catch {}
      },
    };
  } catch {
    return {};
  }
};
