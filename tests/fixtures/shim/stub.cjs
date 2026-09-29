// A stub rotter for the shim harness (the test prepends a `#!<node>` line). It appends its argv,
// environment and stdin to `log` next to it and then acts as `mode` there says.
const fs = require("node:fs");
const path = require("node:path");
const { spawn } = require("node:child_process");

const dir = __dirname;
const mode = fs.readFileSync(path.join(dir, "mode"), "utf8").trim();
const record = (stdin) =>
  fs.appendFileSync(
    path.join(dir, "log"),
    `${JSON.stringify({ argv: process.argv.slice(2), env: process.env, stdin })}\n`,
  );
const reply = JSON.stringify({ continue: "review the comments" });
const big = "x".repeat(70 * 1024);

switch (mode) {
  case "epipe":
    // Exits before reading stdin.
    record(null);
    process.exit(0);
    break;
  case "hang": {
    record(fs.readFileSync(0, "utf8"));
    // A grandchild in the same process group, which the timeout must kill too; it outlives the
    // harness, which exits without waiting for it.
    const child = spawn("/bin/sleep", ["300"], { stdio: "ignore" });
    fs.writeFileSync(path.join(dir, "grandchild"), String(child.pid));
    setTimeout(() => {}, 30000);
    break;
  }
  case "big":
    record(fs.readFileSync(0, "utf8"));
    process.stdout.write(big);
    setTimeout(() => {}, 30000);
    break;
  case "bigexit":
    record(fs.readFileSync(0, "utf8"));
    process.stdout.write(`{"continue":"${big}"}`, () => process.exit(0));
    break;
  case "attimeout":
    // Answers just as a 1 s timeout fires.
    record(fs.readFileSync(0, "utf8"));
    setTimeout(() => process.stdout.write(reply, () => process.exit(0)), 1000);
    break;
  case "capped": {
    // Rotter's own per-session cap: two consecutive requests, then one quiet answer that resets.
    const stdin = fs.readFileSync(0, "utf8");
    record(stdin);
    const file = path.join(dir, "counts.json");
    const counts = fs.existsSync(file) ? JSON.parse(fs.readFileSync(file, "utf8")) : {};
    const session = JSON.parse(stdin).session_id;
    const count = counts[session] ?? 0;
    counts[session] = count >= 2 ? 0 : count + 1;
    fs.writeFileSync(file, JSON.stringify(counts));
    process.stdout.write(count >= 2 ? "" : reply, () => process.exit(0));
    break;
  }
  default: {
    record(fs.readFileSync(0, "utf8"));
    const out = { reply, quiet: "", nonstring: '{"continue":5}', garbage: "not json {" }[mode];
    process.stdout.write(out ?? "", () => process.exit(0));
  }
}
