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
- Desktop preview derives `<parent of VIBEX_DB_PATH>/desktop-preview`; TUI reads
  `VIBEX_HOME` directly. The launcher points both at the same isolated home.
- Artifact `vibex-development-windows-x64`, retention 7 days; no target cache,
  installer, release, tag or user data. Only explicitly allowlisted files ship.
- Manifest v1: integer `schemaVersion=1`, `project=vibex`, `repository`,
  `headSha`, string `runId`/`runAttempt`, `profile=dev`, `channel=preview`,
  `entrypoint=vibex-desktop.exe`, `checks`, `resources`, and
  `files: [{path, size, sha256}]`. Payload paths are relative with `/`; hashes
  are lowercase SHA-256. The manifest excludes its own self-referential hash.
- `--probe` is `first-frame-probe.v1`/`compiled_probe`, Windows/x86_64,
  preview and `nativePixelsVerified=false`. Never interpret it as GUI testing.
- PDFium remains absent while the source review marks Windows deferred. Font
  bytes are embedded; full font license texts accompany third-party notices.

## 4. Validation & Error Matrix

| Condition | Required result |
| --- | --- |
| Existing output directory | Reject without deleting or overwriting it. |
| Missing/invalid x64 executable or license input | Fail before creating output. |
| Checkout HEAD differs from GITHUB_SHA | Reject provenance mismatch. |
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

## 7. Wrong vs Correct

Wrong: set `VIBEX_INSTALL_PACKAGE` only during compilation, launch against the
default home, and report the compiled probe as GUI acceptance.

Correct: preserve the runtime environment through the launcher, use a separate
development home shared by Desktop/TUI, and report cloud contract checks and
local GUI acceptance as independent evidence.
