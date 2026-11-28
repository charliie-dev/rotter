// A stub rotter for the shim harness (the test prepends a `#!<node>` line).
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
  case "linger":
  case "escape": {
    record(fs.readFileSync(0, "utf8"));
    // A grandchild that holds stdout open after rotter exits: in the same process group, or
    // ("escape") in a session of its own, out of the group's reach.
    // The escaped one survives by design: 30 s outlives the harness, and a failed test leaves it
    // only that long.
    const child = spawn("/bin/sleep", [mode === "escape" ? "30" : "300"], {
      detached: mode === "escape",
      stdio: ["ignore", "inherit", "ignore"],
    });
    fs.writeFileSync(path.join(dir, "grandchild"), String(child.pid));
    process.stdout.write(reply, () => process.exit(0));
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
  case "lateoverflow": {
    record(fs.readFileSync(0, "utf8"));
    // Exits at once, but a grandchild in the same process group holds stdout and overflows it
    // 100 ms later: the overflow settles the answer after rotter's exit and before `close`.
    const child = spawn("/bin/sh", ["-c", "sleep 0.1; head -c 70000 /dev/zero; exec sleep 300"], {
      stdio: ["ignore", "inherit", "ignore"],
    });
    fs.writeFileSync(path.join(dir, "grandchild"), String(child.pid));
    process.exit(0);
    break;
  }
  case "attimeout":
    // Answers just as a 2 s shim timeout fires.
    record(fs.readFileSync(0, "utf8"));
    setTimeout(() => process.stdout.write(reply, () => process.exit(0)), 2000);
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
