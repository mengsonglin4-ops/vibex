import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import {
  SUITES, compileArguments, isWithin, matchesArtifact, parseArguments, parseListing,
  runProcess, runReview, validateManifest, validateTestOutput
} from "./run-model-review-tests.mjs";

const FIXTURES = mkdtempSync(join(tmpdir(), "vibex-review-runner-"));
const CHILD = join(FIXTURES, "child.mjs");
const HEAD = "a".repeat(40);
writeFileSync(CHILD, `
import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";
const input = JSON.parse(process.argv[2]);
const mode = input.mode;
if (input.role === "git") {
  console.log(input.head);
} else if (input.role === "cargo") {
  process.stderr.write("fixture compiler stderr\\n");
  if (mode === "compile-failure") process.exit(17);
  if (mode === "huge-json") {
    process.stdout.write("x".repeat(5 * 1024 * 1024));
    setInterval(() => {}, 1000);
  } else {
    console.log(JSON.stringify({ ...input.artifact, target: { ...input.artifact.target, name: "dependency" } }));
    if (mode !== "wrong-target") {
      const line = JSON.stringify(input.artifact);
      process.stdout.write(line.slice(0, 23));
      setTimeout(() => {
        process.stdout.write(line.slice(23) + "\\n");
        if (mode === "duplicate-target") console.log(line);
      }, 10);
    }
  }
} else if ((mode === "timeout-list" && input.list) || (mode === "timeout" && !input.list)) {
  const descendant = spawn(process.execPath, ["-e", "setInterval(() => {}, 1000)"], { stdio: "ignore", windowsHide: true });
  writeFileSync(input.pids, JSON.stringify([process.pid, descendant.pid]));
  console.log("fixture waiting");
  setInterval(() => {}, 1000);
} else if (input.list) {
  if (mode === "list-failure") process.exit(7);
  if (mode !== "zero") console.log(input.filter + "::first: test\\n" + input.filter + "::second: test");
  if (mode === "two-lists") console.log(input.filter + "::first: test");
} else {
  console.log("running 2 tests");
  if (mode === "ignored") console.log(".i\\ntest result: ok. 1 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s");
  else if (mode === "count") console.log(".\\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s");
  else if (mode === "failed") {
    console.log("F.\\ntest result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s");
    process.exitCode = 1;
  } else {
    console.log("..\\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s");
    if (mode === "two-summaries") console.log("test result: ok. 2 passed; 0 failed; 0 ignored;");
    if (mode === "exit-failure") process.exitCode = 9;
  }
}
`);

function fixture(suiteName = "git") {
  const cwd = mkdtempSync(join(FIXTURES, `${suiteName}-`));
  const targetDir = join(cwd, "target with spaces");
  const suite = SUITES[suiteName];
  const crateDir = resolve(cwd, dirname(dirname(suite.source)));
  mkdirSync(crateDir, { recursive: true });
  const executable = join(targetDir, "debug", "deps", `${suite.target}-fixture${process.platform === "win32" ? ".exe" : ""}`);
  mkdirSync(dirname(executable), { recursive: true });
  writeFileSync(executable, "fixture only");
  const state = { mode: "success", head: HEAD, calls: [], stdout: "", stderr: "" };
  const artifact = {
    reason: "compiler-artifact", profile: { test: true }, executable,
    target: { name: suite.target, kind: [suite.kind], src_path: resolve(cwd, suite.source) }
  };
  const summaryPath = join(cwd, "summary.md");
  const pids = join(cwd, "pids.json");
  const context = {
    cwd,
    env: { ...process.env, VIBEX_REVIEW_TEST_TARGET: targetDir, CARGO_TARGET_DIR: join(cwd, "wrong-target"), GITHUB_STEP_SUMMARY: summaryPath },
    stdout: { write(chunk) { state.stdout += chunk; } },
    stderr: { write(chunk) { state.stderr += chunk; } },
    spawnProcess(command, args, options) {
      assert.equal(options.shell, false);
      assert.equal(options.windowsHide, true);
      assert.equal(options.env.CARGO_TARGET_DIR, targetDir);
      const role = command === "git" ? "git" : command === "cargo" ? "cargo" : "test";
      if (role === "test") {
        assert.equal(command, executable);
        assert.equal(options.cwd, crateDir);
        if (process.platform === "win32") {
          const pathKey = Object.keys(context.env).filter((key) => key.toUpperCase() === "PATH").sort()[0];
          assert.deepEqual(Object.keys(options.env).filter((key) => key.toUpperCase() === "PATH"), ["PATH"]);
          assert.equal(options.env.PATH, [dirname(executable), join(targetDir, "debug"), context.env[pathKey]].filter(Boolean).join(";"));
        }
      } else {
        assert.equal(options.cwd, cwd);
      }
      state.calls.push({ command, args, role });
      return spawn(process.execPath, [CHILD, JSON.stringify({
        role, mode: state.mode, head: state.head, artifact, filter: suite.filter, list: args.includes("--list"), pids
      })], options);
    }
  };
  return {
    cwd, targetDir, executable, artifact, state, context, summaryPath, pids,
    manifestPath: join(targetDir, `.model-review-${suiteName}.json`),
    run(phase, timeoutSeconds = 5) { return runReview({ suite: suiteName, phase, timeoutSeconds }, context); }
  };
}

test("argument parsing and exact compile selections include one optimization override argument", () => {
  assert.deepEqual(parseArguments(["--suite", "git", "--phase", "test"]), { suite: "git", phase: "test", timeoutSeconds: 180 });
  for (const args of [[], ["--suite", "__proto__", "--phase", "test"], ["--suite", "git", "--phase", "other"],
    ["--suite", "git", "--phase", "test", "--timeout-seconds", "0"], ["--suite", "git", "--suite", "git"]]) {
    assert.throws(() => parseArguments(args));
  }
  for (const [name, suite] of Object.entries(SUITES)) {
    const args = compileArguments(suite);
    assert.deepEqual(args.slice(0, 4), ["test", "--locked", "-p", suite.package]);
    assert.deepEqual(args.slice(-4), ["--no-run", "--message-format=json", "--config", 'profile.test.package."*".opt-level=0']);
    assert.equal(args.includes("--no-default-features"), name === "credentials");
    assert.equal(args.includes("--lib"), name !== "credentials");
    if (name !== "credentials") assert.equal(suite.target, suite.package.replaceAll("-", "_"));
  }
});

test("all suites compile exact artifacts and directly discover/run with matching filters", async () => {
  for (const name of Object.keys(SUITES)) {
    const current = fixture(name);
    assert.deepEqual(await current.run("compile"), { outcome: "compiled", discovered: 0, passed: 0 });
    assert.match(current.state.stderr, /fixture compiler stderr/);
    assert.deepEqual(current.state.calls.find((call) => call.role === "cargo").args, compileArguments(SUITES[name]));
    current.state.calls = [];
    assert.deepEqual(await current.run("test"), { outcome: "passed", discovered: 2, passed: 2 });
    assert.deepEqual(current.state.calls.filter((call) => call.role === "test").map((call) => call.args), [
      [SUITES[name].filter, "--list", "--format", "terse"],
      [SUITES[name].filter, "--test-threads=1", "--format", "pretty", "--nocapture"]
    ]);
    assert.equal(current.state.calls.some((call) => call.role === "cargo"), false);
    const summary = readFileSync(current.summaryPath, "utf8");
    assert.match(summary, /compile: compiled; \d+\.\d{2}s/);
    assert.match(summary, /test: passed; \d+\.\d{2}s; discovered 2, validated passed 2/);
    assert.doesNotMatch(summary, /fixture compiler stderr/);
  }
});

test("ACP runtime probe suite is mandatory before portable build and uses its own filter", async () => {
  assert.deepEqual(SUITES.probe, {
    package: "vibex-agent-acp", target: "vibex_agent_acp", kind: "lib",
    source: "crates/agent-acp/src/lib.rs", filter: "runtime_probe::tests"
  });
  assert.deepEqual(Object.keys(SUITES), ["git", "config", "credentials", "runtime", "backend", "desktop", "probe", "session"]);
  assert.deepEqual(parseArguments(["--suite", "probe", "--phase", "test"]), { suite: "probe", phase: "test", timeoutSeconds: 180 });
  const workflow = readFileSync(resolve(dirname(fileURLToPath(import.meta.url)), "../.github/workflows/development-windows.yml"), "utf8");
  const compilePosition = workflow.indexOf("--suite probe --phase compile");
  const testPosition = workflow.indexOf("--suite probe --phase test");
  const buildPosition = workflow.indexOf("- name: Build desktop and terminal client");
  assert.ok(compilePosition > 0 && compilePosition < testPosition && testPosition < buildPosition);
  const gateStart = workflow.indexOf("- name: Compile ACP runtime probe tests");
  const gateEnd = workflow.indexOf("- name: Compile runtime review tests", gateStart);
  const gate = workflow.slice(gateStart, gateEnd);
  assert.match(gate, /timeout-minutes: 20/);
  assert.match(gate, /timeout-minutes: 5/);
  assert.doesNotMatch(gate, /continue-on-error:|if:/);
  const current = fixture("probe");
  await current.run("compile");
  for (const mode of ["zero", "ignored", "failed"]) {
    current.state.mode = mode;
    await assert.rejects(current.run("test"));
  }
});

test("Codex live session convergence is a separate mandatory Windows gate", async () => {
  assert.equal(SUITES.session.filter, "runtime::tests::codex_model::");
  assert.equal(SUITES.session.package, "vibex-agent-acp");
  const workflow = readFileSync(resolve(dirname(fileURLToPath(import.meta.url)), "../.github/workflows/development-windows.yml"), "utf8");
  const compilePosition = workflow.indexOf("--suite session --phase compile");
  const testPosition = workflow.indexOf("--suite session --phase test");
  const buildPosition = workflow.indexOf("- name: Build desktop and terminal client");
  assert.ok(compilePosition > 0 && compilePosition < testPosition && testPosition < buildPosition);
  assert.doesNotMatch(workflow.slice(workflow.indexOf("- name: Compile Codex session model tests"), testPosition), /continue-on-error:|if:/);
  const current = fixture("session");
  await current.run("compile");
  for (const mode of ["zero", "ignored", "failed"]) {
    current.state.mode = mode;
    await assert.rejects(current.run("test"));
  }
});

test("rejects zero discovery, ignored, count mismatch, failures and multiple suites", async () => {
  const current = fixture();
  await current.run("compile");
  for (const mode of ["zero", "ignored", "count", "failed", "two-lists", "two-summaries", "list-failure", "exit-failure"]) {
    current.state.mode = mode;
    current.state.calls = [];
    await assert.rejects(current.run("test"));
    assert.equal(current.state.calls.some((call) => call.role === "cargo"), false);
    if (["zero", "two-lists", "list-failure"].includes(mode)) {
      assert.equal(current.state.calls.filter((call) => call.role === "test").length, 1);
    }
    assert.match(readFileSync(current.summaryPath, "utf8").trim().split("\n").at(-1), /test: failed;/);
  }
});

test("wrong/ambiguous compiler targets, failed compile and oversized JSON cannot preserve a runnable manifest", async () => {
  const current = fixture();
  await current.run("compile");
  for (const mode of ["wrong-target", "duplicate-target", "compile-failure", "huge-json"]) {
    current.state.mode = mode;
    await assert.rejects(current.run("compile"));
    assert.equal(JSON.parse(readFileSync(current.manifestPath, "utf8")).status, "compiling");
    await assert.rejects(current.run("test"), /manifest/);
  }
});

test("missing and stale manifests fail before an executable starts", async () => {
  const current = fixture();
  await assert.rejects(current.run("test"), /ENOENT/);
  await current.run("compile");
  const manifest = JSON.parse(readFileSync(current.manifestPath, "utf8"));
  for (const changes of [
    { suite: "config" }, { head: "b".repeat(40) }, { targetDir: join(current.cwd, "other") },
    { sourceRoot: FIXTURES }, { target: "dependency" }, { kind: "bin" }, { source: "wrong/lib.rs" },
    { executable: CHILD }, { executable: join(current.targetDir, "missing.exe") }
  ]) {
    writeFileSync(current.manifestPath, JSON.stringify({ ...manifest, ...changes }));
    current.state.calls = [];
    await assert.rejects(current.run("test"));
    assert.equal(current.state.calls.some((call) => call.role === "test"), false);
  }
  writeFileSync(current.manifestPath, JSON.stringify(manifest));
  current.state.head = "c".repeat(40);
  await assert.rejects(current.run("test"), /stale/);
});

test("target identity requires test profile, exact source, target name and kind", () => {
  const current = fixture();
  const suite = SUITES.git;
  assert.equal(matchesArtifact(current.artifact, suite, current.cwd), true);
  if (process.platform === "win32") {
    assert.equal(matchesArtifact({
      ...current.artifact, target: { ...current.artifact.target, src_path: current.artifact.target.src_path.toUpperCase() }
    }, suite, current.cwd), true);
  }
  for (const changes of [
    { profile: { test: false } }, { executable: null }, { executable: "relative.exe" },
    { target: { ...current.artifact.target, kind: ["bin"] } },
    { target: { ...current.artifact.target, src_path: join(FIXTURES, "dependency.rs") } }
  ]) assert.equal(matchesArtifact({ ...current.artifact, ...changes }, suite, current.cwd), false);
  assert.equal(isWithin(current.targetDir, `${current.targetDir}-elsewhere/test.exe`), false);
  assert.equal(isWithin(current.targetDir, current.targetDir), false);
  assert.throws(() => validateManifest({}, { suite: "git" }), /manifest/);
  assert.equal(parseListing("review::one: test\n1 test, 0 benchmarks\n", "review"), 1);
  assert.throws(() => parseListing("review::one: test\n1 test, 0 benchmarks\n1 test, 0 benchmarks\n", "review"));
  assert.throws(() => parseListing("unrelated::one: test\n", "review"));
  assert.throws(() => validateTestOutput("test result: ok. 2 passed; 0 failed; 0 ignored;", 2));
});

test("hard deadline kills the actual subprocess tree during discovery and execution", { timeout: 15000 }, async () => {
  for (const mode of ["timeout-list", "timeout"]) {
    const current = fixture();
    await current.run("compile");
    current.state.mode = mode;
    const started = Date.now();
    await assert.rejects(current.run("test", 0.8), /timed out/);
    assert.ok(Date.now() - started < 6000);
    assert.ok(existsSync(current.pids));
    const pids = JSON.parse(readFileSync(current.pids, "utf8"));
    for (const pid of pids) assert.throws(() => process.kill(pid, 0), { code: "ESRCH" });
    assert.match(current.state.stdout, /fixture waiting/);
    assert.match(readFileSync(current.summaryPath, "utf8"), /test: failed;/);
  }
});

test("subprocess startup failures reject without a false pass", async () => {
  await assert.rejects(runProcess(join(FIXTURES, "does-not-exist"), [], {
    cwd: FIXTURES, stdout: { write() {} }, stderr: { write() {} }
  }), /ENOENT/);
});
