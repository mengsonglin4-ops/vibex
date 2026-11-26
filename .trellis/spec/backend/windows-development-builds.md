# Windows development artifact contract

## 1. Scope / Trigger

Applies to the branch-scoped Windows development workflow, portable packaging,
manifest consumers and local development launcher. Release workflows and
installer/update policy remain separate.

## 2. Signatures

```text
.github/workflows/development-windows.yml
  push.branches = [cloud-development]; workflow_dispatch
cargo build -p vibex-desktop -p vibex-client --locked --jobs 2
node scripts/package-development-windows.mjs --output <new-directory> [--binary-dir <debug-directory>]
Start-VibexDevelopment.ps1 [-DataRoot <directory>] [-Tui]
```

## 3. Contracts

- Windows x64, repository-pinned Rust, Node 22, pnpm 11.3.0; frozen pnpm assets
  must exist before Cargo compiles the desktop's embedded Inter font bytes.
- `CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=2`,
  `VIBEX_CHANNEL=preview`, `VIBEX_INSTALL_PACKAGE=unmanaged`. The installation
  override is read at runtime, so the launcher also sets it.
- Set `CARGO_PROFILE_DEV_DEBUG_ASSERTIONS=false` and
  `CARGO_PROFILE_DEV_BUILD_OVERRIDE_DEBUG_ASSERTIONS=false`. Debug symbols and
  debug assertions are different Cargo controls. GPUI's debug renderer reads
  absolute Cargo source/output HLSL paths; its non-debug renderer embeds FXC
  bytes generated only when its build script also has debug assertions off.
  Require the Windows SDK `fxc.exe` and set `GPUI_FXC_PATH` explicitly.
- Desktop preview derives `<parent of VIBEX_DB_PATH>/desktop-preview`; TUI reads
  `VIBEX_HOME` directly. The launcher points both at the same isolated home.
- Resolve relative `DataRoot` through PowerShell's session-state path API,
  rather than the process current directory. Launch the interactive GUI with
  `WindowStyle Normal`. Database/home isolation does not isolate the system
  credential store, `CODEX_HOME`, or other inherited Agent configuration.
- Artifact `vibex-development-windows-x64`, retention 7 days; no target cache,
  installer, release, tag or user data. Only explicitly allowlisted files ship.
- Manifest v1: integer `schemaVersion=1`, `project=vibex`, `repository`,
  `headSha`, string `runId`/`runAttempt`, `profile=dev`, `channel=preview`,
  `entrypoint=vibex-desktop.exe`, `checks`, `resources`, and
  `files: [{path, size, sha256}]`. Payload paths are relative with `/`; hashes
  are lowercase SHA-256. The manifest excludes its own self-referential hash.
- `--probe` is `first-frame-probe.v1`/`compiled_probe`, Windows/x86_64,
  preview and `nativePixelsVerified=false`. Never interpret it as GUI testing.
- Before packaging, compare the desktop executable against all 16 expected
  DXBC arrays in the single generated GPUI `shaders_bytes.rs`. Reject known
  runtime HLSL path strings in UTF-8 or UTF-16. This gate detects the source-path
  dependency without relying on a hosted runner's GPU/display availability.
  Manifest records `build.debugAssertions=false`,
  `build.buildScriptDebugAssertions=false`, and
  `checks.embeddedWindowsShaders={status: passed, shaderCount: 16, format: DXBC}`.
- PDFium remains absent while the source review marks Windows deferred. Font
  bytes are embedded; full font license texts accompany third-party notices.

## 4. Validation & Error Matrix

| Condition | Required result |
| --- | --- |
| Existing output directory | Reject without deleting or overwriting it. |
| Missing/invalid x64 executable or license input | Fail before creating output. |
| Checkout HEAD differs from GITHUB_SHA | Reject provenance mismatch. |
| FXC unavailable | Fail toolchain preparation before the long native build. |
| Runtime HLSL path remains, generated bindings are ambiguous/missing, or any expected DXBC array is not embedded | Reject portable artifact before upload. |
| Probe fails, hangs, returns another channel/architecture/schema | Fail within bounded subprocess timeout; no artifact upload. |
| TUI version differs from package.json | Reject mixed binaries. |
| Windows PDFium review changes | Stop for payload review. |
| Build/check failure | Fail the job; do not upload partial payload. |

## 5. Good / Base / Bad Cases

- Good: clean cloud build, copied binary probes succeed, file hashes are
  verified after download, then the local GUI is checked with a separate home.
- Base: local packager smoke uses existing binaries and records a `local` run;
  this validates packaging only, never claims a new cloud build.
- Bad: upload all of `target/`, use a stable home for smoke, or claim native
  pixels are verified because `--probe` exited successfully.

## 6. Tests Required

- Parse workflow YAML and PowerShell, `node --check` the packager, and run
  `pnpm check:frontend` for changed build tooling.
- Validate a real portable package against already built Windows binaries;
  verify all listed hashes and paths. A corrupt PE input and pre-existing
  output must be rejected. No local Rust rebuild is required for script edits.
- The first cloud run must build with `--locked` and pass package probes.
  Local visual acceptance is a separate recorded step after artifact download.
- Run `node scripts/check-development-windows-shaders.test.mjs`: embedded
  arrays pass; missing/malformed arrays, absent executable bytecode and both
  encodings of runtime source/output paths fail. Confirm a previously broken
  debug executable fails this gate without launching it or rebuilding locally.

## 7. Wrong vs Correct

Wrong: disable debug symbols only, treat `--probe` as renderer initialization,
and ship a debug EXE that still loads HLSL from the build runner's directories.

Correct: disable debug assertions in both the library and build script, require
FXC, inspect embedded shader bytes before upload, preserve the launcher runtime
environment and report local GUI acceptance separately.
