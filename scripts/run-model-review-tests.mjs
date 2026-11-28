import { execFile, spawn } from "node:child_process";
import { appendFileSync, mkdirSync, readFileSync, realpathSync, statSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const MAX_LINE = 4 * 1024 * 1024;
export const SUITES = {
  git: { package: "vibex-git", target: "vibex_git", kind: "lib", source: "crates/git/src/lib.rs", filter: "review" },
  config: { package: "vibex-config-switch", target: "vibex_config_switch", kind: "lib", source: "crates/config-switch/src/lib.rs", filter: "model_review" },
  credentials: { package: "vibex-config-switch", target: "model_review_credentials", kind: "test", source: "crates/config-switch/tests/model_review_credentials.rs", filter: "model_review" },
  runtime: { package: "vibex-desktop-runtime", target: "vibex_desktop_runtime", kind: "lib", source: "crates/desktop-runtime/src/lib.rs", filter: "model_review" },
  backend: { package: "vibex-backend", target: "vibex_backend", kind: "lib", source: "crates/vibex-backend/src/lib.rs", filter: "disconnected_review" },
  desktop: { package: "vibex-desktop", target: "vibex_desktop", kind: "lib", source: "apps/desktop/src/lib.rs", filter: "model_review" },
  probe: { package: "vibex-agent-acp", target: "vibex_agent_acp", kind: "lib", source: "crates/agent-acp/src/lib.rs", filter: "runtime_probe::tests" }
};

export function parseArguments(argv) {
  const values = {};
  for (let index = 0; index < argv.length; index += 2) {
    const key = argv[index];
    if (!["--suite", "--phase", "--timeout-seconds"].includes(key) || !argv[index + 1] || values[key] !== undefined) {
      throw new Error("Usage: --suite <git|config|credentials|runtime|backend|desktop|probe> --phase <compile|test> [--timeout-seconds <positive number>]");
    }
    values[key] = argv[index + 1];
  }
  if (!Object.hasOwn(SUITES, values["--suite"]) || !["compile", "test"].includes(values["--phase"])) {
    throw new Error("A valid --suite and --phase are required");
  }
  const timeoutSeconds = Number(values["--timeout-seconds"] ?? 180);
  if (!Number.isFinite(timeoutSeconds) || timeoutSeconds <= 0 || timeoutSeconds > 86400) {
    throw new Error("--timeout-seconds must be positive and at most 86400");
  }
  return { suite: values["--suite"], phase: values["--phase"], timeoutSeconds };
}

export function compileArguments(suite) {
  return ["test", "--locked", "-p", suite.package,
    ...(suite.kind === "test" ? ["--no-default-features", "--test", suite.target] : ["--lib"]),
    "--no-run", "--message-format=json", "--config", 'profile.test.package."*".opt-level=0'];
}

export function matchesArtifact(message, suite, cwd) {
  return message.reason === "compiler-artifact" && message.profile?.test === true &&
    typeof message.executable === "string" && isAbsolute(message.executable) &&
    message.target?.name === suite.target && message.target.kind?.length === 1 &&
    message.target.kind[0] === suite.kind && typeof message.target.src_path === "string" &&
    relative(resolve(message.target.src_path), resolve(cwd, suite.source)) === "";
}

export function isWithin(directory, path) {
  const suffix = relative(directory, path);
  return suffix !== "" && suffix !== ".." && !suffix.startsWith(`..${sep}`) && !isAbsolute(suffix);
}

export function validateManifest(manifest, expected) {
  const suite = SUITES[expected.suite];
  if (manifest.schemaVersion !== 1 || manifest.status !== "compiled" ||
      manifest.suite !== expected.suite || manifest.head !== expected.head ||
      manifest.sourceRoot !== expected.sourceRoot || manifest.targetDir !== expected.targetDir ||
      manifest.package !== suite.package || manifest.target !== suite.target || manifest.kind !== suite.kind ||
      manifest.source !== suite.source || manifest.filter !== suite.filter ||
      typeof manifest.executable !== "string" || !isAbsolute(manifest.executable) ||
      !isWithin(expected.targetDir, manifest.executable)) {
    throw new Error("Invalid or stale model-review manifest (suite, HEAD, source, target or executable mismatch)");
  }
  return manifest.executable;
}

export function parseListing(output, filter) {
  const lines = output.split(/\r?\n/).filter((line) => line.trim());
  const tests = lines.filter((line) => line.endsWith(": test")).map((line) => line.slice(0, -6));
  const summaries = lines.filter((line) => /^\d+ tests?, \d+ benchmarks?$/.test(line));
  const other = lines.filter((line) => !line.endsWith(": test") && !summaries.includes(line));
  if (!tests.length || new Set(tests).size !== tests.length || tests.some((name) => !name.includes(filter)) ||
      summaries.length > 1 || other.length ||
      (summaries.length === 1 && !new RegExp(`^${tests.length} tests?, 0 benchmarks?$`).test(summaries[0]))) {
    throw new Error("Discovery must contain nonzero unique matching tests from exactly one suite");
  }
  return tests.length;
}

export function validateTestOutput(output, discovered) {
  const summaries = output.split(/\r?\n/).filter((line) => line.startsWith("test result:"));
  const result = summaries.length === 1 && /^test result: ok\. (\d+) passed; 0 failed; 0 ignored;/.exec(summaries[0]);
  const starts = [...output.matchAll(/^running (\d+) tests?\r?$/gm)];
  if (!result || Number(result[1]) !== discovered || starts.length !== 1 || Number(starts[0][1]) !== discovered) {
    throw new Error("Expected one successful suite summary, all discovered tests passed and zero failed/ignored");
  }
  return Number(result[1]);
}

async function killTree(child) {
  if (!child.pid) return;
  if (process.platform === "win32") {
    await new Promise((accept, reject) => {
      execFile("taskkill", ["/pid", String(child.pid), "/T", "/F"],
        { windowsHide: true, shell: false, timeout: 10000 }, (error) => {
          if (error) {
            child.kill("SIGKILL");
            reject(new Error("Failed to terminate process tree with taskkill"));
          } else accept();
        });
    });
  } else {
    try {
      process.kill(-child.pid, "SIGKILL");
    } catch (error) {
      if (error.code !== "ESRCH") throw error;
    }
  }
}

export function runProcess(command, args, {
  cwd = ROOT, env = process.env, spawnProcess = spawn, stdout = process.stdout, stderr = process.stderr,
  label = command, timeoutMs = 0, onLine, capture = false, echo = true
} = {}) {
  return new Promise((accept, reject) => {
    const started = Date.now();
    const child = spawnProcess(command, args, {
      cwd, env, shell: false, windowsHide: true, detached: process.platform !== "win32", stdio: ["ignore", "pipe", "pipe"]
    });
    let captured = "";
    let pending = "";
    let failure;
    let termination;
    const stop = (error) => {
      if (failure) return;
      failure = error;
      termination = killTree(child).catch((killError) => { failure = new Error(`${error.message}; ${killError.message}`); });
    };
    const heartbeat = setInterval(() => {
      stderr.write(`[model-review] ${label}: still running (${((Date.now() - started) / 1000).toFixed(0)}s)\n`);
    }, 30000);
    const timeout = timeoutMs > 0 ? setTimeout(() => stop(new Error(`${label} timed out after ${(timeoutMs / 1000).toFixed(2)}s`)), timeoutMs) : undefined;
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (chunk) => stderr.write(chunk));
    child.stdout.on("data", (chunk) => {
      if (echo) stdout.write(chunk);
      if (failure) return;
      try {
        if (capture) {
          if (captured.length + chunk.length > MAX_LINE) throw new Error(`${label} output exceeds limit`);
          captured += chunk;
        }
        if (onLine) {
          pending += chunk;
          let boundary;
          while ((boundary = pending.indexOf("\n")) !== -1) {
            if (boundary > MAX_LINE) throw new Error(`${label} JSON line exceeds limit`);
            const line = pending.slice(0, boundary).replace(/\r$/, "");
            pending = pending.slice(boundary + 1);
            if (line) onLine(line);
          }
          if (pending.length > MAX_LINE) throw new Error(`${label} JSON line exceeds limit`);
        }
      } catch (error) { stop(error); }
    });
    child.on("error", (error) => { failure ??= error; });
    child.on("close", async (code, signal) => {
      clearInterval(heartbeat);
      clearTimeout(timeout);
      if (termination) await termination;
      try {
        if (failure) throw failure;
        if (code !== 0) throw new Error(`${label} exited ${code ?? signal}`);
        if (onLine && pending.trim()) onLine(pending);
        accept(captured);
      } catch (error) { reject(error); }
    });
  });
}

export async function runReview(options, {
  cwd = ROOT, env = process.env, spawnProcess = spawn, stdout = process.stdout, stderr = process.stderr
} = {}) {
  const started = Date.now();
  const suite = SUITES[options.suite];
  let outcome = "failed";
  let discovered = 0;
  let passed = 0;
  const processEnv = { ...env, CARGO_TARGET_DIR: resolve(cwd, env.VIBEX_REVIEW_TEST_TARGET || env.CARGO_TARGET_DIR || "target/review-tests") };
  const processOptions = { cwd, env: processEnv, spawnProcess, stdout, stderr };
  const head = async () => {
    const value = (await runProcess("git", ["rev-parse", "HEAD"], {
      ...processOptions, label: "git HEAD", timeoutMs: 30000, capture: true, echo: false
    })).trim();
    if (!/^[a-f0-9]{40,64}$/i.test(value)) throw new Error("Invalid source HEAD");
    return value;
  };
  try {
    if (!suite || !["compile", "test"].includes(options.phase)) throw new Error("Invalid suite or phase");
    if (options.phase === "compile") mkdirSync(processEnv.CARGO_TARGET_DIR, { recursive: true });
    const targetDir = realpathSync(processEnv.CARGO_TARGET_DIR);
    const manifestPath = join(targetDir, `.model-review-${options.suite}.json`);
    const expected = { suite: options.suite, head: await head(), sourceRoot: realpathSync(cwd), targetDir };
    if (options.phase === "compile") {
      writeFileSync(manifestPath, JSON.stringify({ schemaVersion: 1, status: "compiling", ...expected }));
      const artifacts = [];
      await runProcess("cargo", compileArguments(suite), {
        ...processOptions, label: `${options.suite} compile`, echo: false,
        onLine(line) {
          const message = JSON.parse(line);
          if (message.reason === "compiler-message" && message.message?.rendered) stderr.write(message.message.rendered);
          if (matchesArtifact(message, suite, cwd)) {
            artifacts.push(message.executable);
            if (artifacts.length > 1) throw new Error("Expected exactly one matching compiler artifact");
          }
        }
      });
      if (artifacts.length !== 1) throw new Error("Expected exactly one matching compiler artifact");
      const executable = realpathSync(artifacts[0]);
      if (!isWithin(targetDir, executable) || !statSync(executable).isFile()) throw new Error("Executable is outside target directory or not a file");
      if (await head() !== expected.head) throw new Error("Source HEAD changed during compile");
      const manifest = {
        schemaVersion: 1, status: "compiled", ...expected, package: suite.package, target: suite.target,
        kind: suite.kind, source: suite.source, filter: suite.filter, executable
      };
      writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
      outcome = "compiled";
    } else {
      const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
      const executable = validateManifest(manifest, expected);
      if (!isWithin(targetDir, realpathSync(executable)) || !statSync(executable).isFile()) {
        throw new Error("Executable is outside target directory or not a file");
      }
      if (process.platform === "win32") {
        const pathKeys = Object.keys(processEnv).filter((key) => key.toUpperCase() === "PATH").sort();
        const inheritedPath = processEnv[pathKeys[0]] ?? "";
        for (const key of pathKeys) delete processEnv[key];
        processEnv.PATH = [dirname(executable), join(targetDir, "debug"), inheritedPath].filter(Boolean).join(";");
      }
      const testCwd = resolve(cwd, dirname(dirname(suite.source)));
      const deadline = Date.now() + (options.timeoutSeconds ?? 180) * 1000;
      const remaining = () => {
        const milliseconds = deadline - Date.now();
        if (milliseconds <= 0) throw new Error(`${options.suite} test timed out`);
        return milliseconds;
      };
      const listing = await runProcess(executable, [suite.filter, "--list", "--format", "terse"], {
        ...processOptions, cwd: testCwd, label: `${options.suite} discovery`, timeoutMs: remaining(), capture: true
      });
      discovered = parseListing(listing, suite.filter);
      const output = await runProcess(executable, [suite.filter, "--test-threads=1", "--format", "pretty", "--nocapture"], {
        ...processOptions, cwd: testCwd, label: `${options.suite} tests`, timeoutMs: remaining(), capture: true
      });
      passed = validateTestOutput(output, discovered);
      outcome = "passed";
    }
    return { outcome, discovered, passed };
  } finally {
    const duration = ((Date.now() - started) / 1000).toFixed(2);
    const counts = options.phase === "test" ? `; discovered ${discovered}, validated passed ${passed}` : "";
    const summary = `Model review ${options.suite} / ${options.phase}: ${outcome}; ${duration}s${counts}`;
    stdout.write(`${summary}\n`);
    if (env.GITHUB_STEP_SUMMARY) appendFileSync(env.GITHUB_STEP_SUMMARY, `- ${summary}\n`);
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    await runReview(parseArguments(process.argv.slice(2)));
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  }
}
