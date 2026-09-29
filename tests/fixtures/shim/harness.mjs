// Runs one rendered rotter shim against a stub host API: <engine> harness.mjs <host> <shim>
// <scenario> <cwd>, with Node or Bun as the engine. The shim's exe is a stub rotter (stub.cjs)
// that reads its behaviour from `mode` next to it. Prints one JSON line: the host engine, each
// stop's answer and any uncaught exception or unhandled rejection, which must never fire; then
// exits, so a process the shim left behind is not waited for.
import { pathToFileURL } from "node:url";

const [host, shim, scenario, cwd] = process.argv.slice(2);
const fired = [];
process.on("uncaughtException", (error) => fired.push(`uncaughtException: ${error}`));
process.on("unhandledRejection", (error) => fired.push(`unhandledRejection: ${error}`));

// None of these may reach rotter: the shim passes only PATH and LANG.
process.env.LD_PRELOAD = "/nonexistent/preload.so";
process.env.DYLD_INSERT_LIBRARIES = "/nonexistent/insert.dylib";
process.env.DEVELOPER_DIR = "/nonexistent/developer";
process.env.HOME = "/nonexistent/home";
process.env.ROTTER_STATE_DIR = "/nonexistent/state";
process.env.GIT_DIR = "/nonexistent/git";

const module = await import(pathToFileURL(shim).href);
let handler = null;

// OpenCode: what the stub client was asked to prompt, every session.idle, and every event
// promise (OpenCode does not await them; the harness waits only to know when a chain ends).
const prompts = [];
let idles = 0;
const events = [];
const emit = (event) => {
  if (event.type === "session.idle") idles += 1;
  const promise = handler({ event: { id: `e${events.length}`, ...event } });
  events.push(promise);
  return promise;
};
const idle = (sessionID) => ({ type: "session.idle", properties: { sessionID } });

if (host === "pi") {
  module.default({
    on(name, fn) {
      if (name === "agent_before_settle") handler = fn;
    },
  });
} else if (host === "letta") {
  const off = module.default({
    capabilities: { events: { turns: true } },
    events: {
      on(name, fn) {
        if (name === "turn_end") handler = fn;
        return () => {};
      },
    },
  });
  if (typeof off !== "function") fired.push("activate returned no disposer");
} else {
  // OpenCode calls every export as a plugin and rejects a module with any other export.
  const plugins = Object.values(module);
  if (plugins.length !== 1 || typeof plugins[0] !== "function") fired.push("not one plugin");
  const client = {
    session: {
      async get({ path }) {
        if (scenario === "throwing") throw new Error("host API failure");
        return { data: path.id === "child" ? { id: path.id, parentID: "parent" } : { id: path.id } };
      },
      async promptAsync({ path, body }) {
        const [part, ...extra] = body.parts;
        prompts.push(part.type === "text" && extra.length === 0 ? part.text : { bad: body });
        if (scenario === "rejecting") throw new Error("prompt failed");
        if (scenario === "chain") {
          // The injected prompt is a new user message; its turn ends with the next idle, both
          // before this call has even returned.
          const info = { role: "user", sessionID: path.id };
          void emit({ type: "message.updated", properties: { info } });
          void emit(idle(path.id));
        }
        return { data: true };
      },
    },
  };
  const hooks = await plugins[0]({ client, directory: cwd, worktree: cwd, project: {} });
  handler = hooks.event;
}
if (typeof handler !== "function") fired.push("no handler registered");

// One host stop; resolves to the review request the shim injected, or null.
async function stop(session, { outcome, throwing } = {}) {
  if (host === "pi") {
    const earlier = { type: "custom", customType: "other" };
    const ctx = throwing
      ? {
          cwd,
          get sessionManager() {
            throw new Error("host API failure");
          },
        }
      : { cwd, sessionManager: { getSessionId: () => session } };
    const result = await handler(
      { type: "agent_before_settle", outcome: outcome ?? "completed", entries: [earlier], continue: false },
      ctx,
    );
    if (result === undefined) return null;
    const [kept, added, ...extra] = result.entries;
    if (result.continue !== true || kept !== earlier || extra.length > 0) return { bad: result };
    if (added.type !== "custom_message" || added.display !== true) return { bad: result };
    return added.content;
  }
  if (host === "letta") {
    const result = await handler(
      { agentId: "a", conversationId: session, stopReason: outcome ?? "end_turn" },
      throwing ? null : { cwd, sessionId: "fallback" },
    );
    if (result === undefined) return null;
    return typeof result.continue === "string" ? result.continue : { bad: result };
  }
  const before = prompts.length;
  const result = await emit(idle(outcome ?? session));
  if (result !== undefined) return { bad: result };
  return prompts.length > before ? prompts[prompts.length - 1] : null;
}

const timed = async (promise) => {
  const start = Date.now();
  const answer = await promise;
  return { answer, ms: Date.now() - start };
};

const answers = [];
switch (scenario) {
  case "loop": {
    // Four stops in one session; with the capped stub the third is rotter's silent one.
    for (let turn = 0; turn < 4; turn += 1) answers.push(await timed(stop("s")));
    // One run at a time: a second stop while the first runs asks nothing.
    const [first, second] = await Promise.all([timed(stop("t")), timed(stop("t"))]);
    answers.push(first, second);
    break;
  }
  case "chain": {
    // OpenCode: one idle, then whatever the injected prompts cause, until nothing is pending.
    const start = Date.now();
    await emit(idle("s"));
    for (let seen = 0; seen < events.length; ) {
      seen = events.length;
      await Promise.allSettled(events);
    }
    for (const answer of prompts) answers.push({ answer, ms: Date.now() - start });
    break;
  }
  case "skipped":
    answers.push(
      await timed(stop("s", { outcome: { pi: "aborted", letta: "max_steps", opencode: "child" }[host] })),
    );
    break;
  case "throwing":
    answers.push(await timed(stop("s", { throwing: true })));
    break;
  default:
    answers.push(await timed(stop("s")));
}

// Keep the loop alive a while so late callbacks (kills, closes, timers) still run.
setTimeout(() => {
  const engine = typeof Bun === "undefined" ? `node ${process.versions.node}` : `bun ${Bun.version}`;
  const report = JSON.stringify({ engine, answers, idles, fired });
  process.stdout.write(`${report}\n`, () => process.exit(0));
}, 1200);
