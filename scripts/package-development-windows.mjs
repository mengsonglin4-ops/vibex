import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import {
  copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync,
  statSync, writeFileSync
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: ROOT, encoding: "utf8", windowsHide: true, timeout: 60_000,
    maxBuffer: 4 * 1024 * 1024, ...options
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`${command} exited ${result.status}: ${result.stderr}`);
  }
  return result.stdout.trim();
}

function argumentsByName() {
  const values = process.argv.slice(2);
  const args = {};
  for (let index = 0; index < values.length; index += 2) {
    const name = values[index];
    if (!["--output", "--binary-dir"].includes(name) || !values[index + 1]) {
      throw new Error("Usage: node scripts/package-development-windows.mjs --output <new-directory> [--binary-dir <debug-directory>]");
    }
    if (args[name]) throw new Error(`Duplicate argument ${name}`);
    args[name] = resolve(values[index + 1]);
  }
  if (!args["--output"]) throw new Error("--output is required");
  return args;
}

function assertWindowsX64(path) {
  const bytes = readFileSync(path);
  const offset = bytes.length >= 64 ? bytes.readUInt32LE(0x3c) : -1;
  if (bytes.toString("ascii", 0, 2) !== "MZ" || offset < 0 || offset + 6 > bytes.length ||
      bytes.readUInt32LE(offset) !== 0x00004550 || bytes.readUInt16LE(offset + 4) !== 0x8664) {
    throw new Error(`Expected a Windows x64 PE executable: ${path}`);
  }
}

function main() {
  const args = argumentsByName();
  if (process.platform !== "win32" || process.arch !== "x64") {
    throw new Error("Package and probe on a Windows x64 host");
  }
  const output = args["--output"];
  const binaryDir = args["--binary-dir"] ?? join(resolve(process.env.CARGO_TARGET_DIR || "target"), "debug");
  if (existsSync(output)) throw new Error("Output already exists; choose a fresh directory");
  const repository = process.env.GITHUB_REPOSITORY || "local";
  const headSha = run("git", ["rev-parse", "HEAD"]);
  if (process.env.GITHUB_SHA && process.env.GITHUB_SHA !== headSha) {
    throw new Error("Checkout HEAD does not match GITHUB_SHA");
  }
  const version = JSON.parse(readFileSync(join(ROOT, "package.json"), "utf8")).version;
  const review = JSON.parse(readFileSync(join(ROOT, "docs/licenses/pdfium-7881-review.json"), "utf8"));
  if (!review.review.deferredRuntimeTargets.includes("windows-x86_64")) {
    throw new Error("Windows PDFium policy changed; review the portable payload before packaging");
  }
  const payload = [
    [join(binaryDir, "vibex-desktop.exe"), "vibex-desktop.exe"],
    [join(binaryDir, "vibex.exe"), "vibex.exe"],
    [join(ROOT, "scripts/start-development-windows.ps1"), "Start-VibexDevelopment.ps1"],
    [join(ROOT, "LICENSE"), "LICENSE"],
    [join(ROOT, "docs/licenses/desktop-third-party-notices.md"), "THIRD-PARTY-NOTICES.md"],
    [join(ROOT, "docs/operations/development-windows.md"), "README.md"],
    [join(ROOT, "node_modules/.pnpm/@fontsource-variable+inter@5.2.8/node_modules/@fontsource-variable/inter/LICENSE"), "licenses/inter-LICENSE"]
  ];
  // The font bytes are embedded in the desktop executable; carry their full
  // license texts, but do not duplicate the multi-megabyte font files.
  const policy = JSON.parse(readFileSync(join(ROOT, "docs/licenses/desktop-policy.json"), "utf8"));
  for (const font of policy.fontInputs.filter((font) => font.ownerPackage === "vibex-desktop")) {
    const fontRoot = join(ROOT, font.root);
    const notices = readdirSync(fontRoot).filter((name) => /\.txt$|^LICENSE|^OFL/i.test(name));
    if (notices.length === 0) throw new Error(`No license text for ${font.id}`);
    for (const name of notices) payload.push([join(fontRoot, name), `licenses/${font.id}/${name}`]);
  }
  for (const [source, destination] of payload) {
    if (!statSync(source).isFile()) throw new Error(`Missing payload file: ${source}`);
    if (destination.endsWith(".exe")) assertWindowsX64(source);
  }
  mkdirSync(output, { recursive: true });
  for (const [source, destination] of payload) {
    const target = join(output, destination);
    mkdirSync(dirname(target), { recursive: true });
    copyFileSync(source, target);
  }

  // Keep probe state outside the artifact and leave it available for diagnosis.
  // RUNNER_TEMP is ephemeral in CI; a local caller controls TEMP/TMP.
  const probeHome = mkdtempSync(join(process.env.RUNNER_TEMP || tmpdir(), "vibex-development-probe-"));
  const env = { ...process.env,
    HOME: probeHome, USERPROFILE: probeHome,
    APPDATA: join(probeHome, "AppData/Roaming"), LOCALAPPDATA: join(probeHome, "AppData/Local"),
    XDG_CONFIG_HOME: join(probeHome, "config"), XDG_DATA_HOME: join(probeHome, "data"),
    XDG_CACHE_HOME: join(probeHome, "cache"), TEMP: join(probeHome, "temp"), TMP: join(probeHome, "temp"),
    VIBEX_HOME: join(probeHome, "desktop-preview"), VIBEX_DB_PATH: join(probeHome, "vibex.db"),
    VIBEX_CHANNEL: "preview", VIBEX_INSTALL_PACKAGE: "unmanaged"
  };
  for (const key of ["APPDATA", "LOCALAPPDATA", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "TEMP", "VIBEX_HOME"]) {
    mkdirSync(env[key], { recursive: true });
  }
  const probeText = run(join(output, "vibex-desktop.exe"), ["--probe"], { cwd: probeHome, env });
  const probe = JSON.parse(probeText);
  if (probe.schemaVersion !== "first-frame-probe.v1" || probe.status !== "compiled_probe" ||
      probe.platform !== "windows" || probe.architecture !== "x86_64" ||
      probe.releaseChannel !== "preview" || probe.applicationId !== "dev.vibex.desktop.preview" ||
      probe.nativePixelsVerified !== false) {
    throw new Error("Desktop probe failed the Windows preview contract");
  }
  const tuiVersion = run(join(output, "vibex.exe"), ["--version"], { cwd: probeHome, env });
  if (tuiVersion !== `vibex ${version}`) throw new Error(`Unexpected terminal version: ${tuiVersion}`);
  writeFileSync(join(probeHome, "desktop-probe.json"), `${probeText}\n`);
  const files = payload.map(([, path]) => {
    const bytes = readFileSync(join(output, path));
    return { path, size: bytes.length, sha256: createHash("sha256").update(bytes).digest("hex") };
  });
  const manifest = {
    schemaVersion: 1, project: "vibex", repository, headSha,
    runId: process.env.GITHUB_RUN_ID || "local",
    runAttempt: process.env.GITHUB_RUN_ATTEMPT || "local",
    version, platform: "windows", architecture: "x64", profile: "dev", channel: "preview",
    entrypoint: "vibex-desktop.exe", installPackage: "unmanaged",
    build: { debug: 0, incremental: false, jobs: 2, locked: true },
    checks: {
      windowsX64Executables: "passed", desktopProbe: "passed", tuiVersion: "passed",
      embeddedAssets: "checked_by_rust_include_bytes", fileHashes: "sha256",
      nativePixelsVerified: false, localGuiAcceptance: "required"
    },
    resources: { fonts: "embedded", icons: "embedded", pdfium: "not_bundled_windows_validation_deferred" },
    files
  };
  writeFileSync(join(output, "manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
  console.log(`Portable payload verified: ${relative(ROOT, output)} (${files.length} files)`);
}

try { main(); } catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
}
