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
node scripts/run-model-review-tests.mjs --suite <git|config|credentials|runtime|backend|desktop|probe> --phase <compile|test>
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
- Before building the portable executables, run the six model-review suites
  and the ACP runtime-probe suite
  listed below, each in separate compile and test workflow steps. Compile with
  `cargo test --locked --no-run --message-format=json`, identify the unique
  requested test executable from Cargo artifacts, and record its suite, source
  HEAD and target directory. Test the recorded executable directly, without
  another Cargo compile: list matching tests with `--list --format terse`,
  require at least one, then run with `--test-threads=1 --format pretty --nocapture`
  so a stalled test remains identifiable in the live log.
  Require a successful exit and exactly
  one successful summary with no ignored tests and a passed count equal to
  discovery. A missing target, empty filter, ignored test or failing suite
  blocks the subsequent build, packaging and upload steps.
- Keep the test step's `CARGO_TARGET_DIR` at `target/review-tests`, selected by
  `VIBEX_REVIEW_TEST_TARGET`, separate from portable build output at
  `target/preview`. Set `CARGO_PROFILE_TEST_DEBUG=0`,
  `CARGO_PROFILE_TEST_DEBUG_ASSERTIONS=false` and
  `CARGO_PROFILE_TEST_BUILD_OVERRIDE_DEBUG_ASSERTIONS=false`; keep the dev
  profile controls above. Desktop tests enable GPUI `test-support`, so matching
  profile settings alone cannot guarantee one shader binding directory.
  The packager searches `<binary-dir>/build/gpui-fast-windows-*/out/shaders_bytes.rs`
  and requires exactly one match. Never point it at the test target or copy test
  build outputs into the portable build directory.
- Test compilation overrides `profile.test.package."*".opt-level=0` through
  Cargo configuration so correctness tests do not inherit the desktop's costly
  dependency optimization. The Preview build retains the repository's normal
  dev dependency optimization. This is not a performance-test profile.
- The runner reports phase duration and a heartbeat during long subprocesses.
  Test execution is hard-bounded to 180 seconds by default; workflow test steps
  allow five minutes including discovery and shutdown. Compilation has separate
  step limits (15/15/10/30/10/30 minutes for the suites in order), with a
  45-minute Preview build limit and the existing 120-minute overall limit.
- ACP probe tests use `-p vibex-agent-acp --lib runtime_probe::tests`, with
  separate 20-minute compile and five-minute test steps before the full build.
  They share the same 180-second execution deadline and nonzero/no-ignored
  gate. Run the existing six suites unchanged; passing them does not substitute
  for probe isolation, projected Codex home and Windows junction regressions.
- Restore Rust dependencies and build caches only after toolchain setup, using
  the pinned rust-cache action. Cache the two target directories separately and
  exclude Cargo binaries. Keys include the lockfile, toolchain, workflow,
  runner script, compiler environment and platform. Save completed dependency
  work on failure; a restored cache never skips a regression or package gate.
  Keep caches separate from downloadable Preview artifacts and runtime data.
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
| A model-review suite fails discovery/execution, lists no tests, ignores tests, or runs a different count | Fail before building, packaging or uploading portable executables. |
| Recorded test artifact belongs to another HEAD, suite or target, or does not exist | Reject before executing it. |
| Test subprocess exceeds its deadline | Stop its process tree and fail the suite. |
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

- The Windows development workflow runs these selections, each prefixed with
  `cargo test --locked` and using the discovery/execution checks above:

  | Cargo selection | Scope |
  | --- | --- |
  | `-p vibex-git --lib review` | Frozen snapshots, selected staging sides and Windows guarded Git extraction. |
  | `-p vibex-config-switch --lib model_review` | Provider configuration and local HTTP model-review request/response behavior. |
  | `-p vibex-config-switch --no-default-features --test model_review_credentials` | Real HostFile credential/configuration integration regressions, serialized with `--test-threads=1`. |
  | `-p vibex-desktop-runtime --lib model_review` | Owner-bound single-use tickets, cancellation and report persistence. |
  | `-p vibex-backend --lib disconnected_review` | Offline review refuses local credential fallback. |
  | `-p vibex-desktop --lib model_review` | Selection and confirmation/cancellation state in `code_workbench::model_review`. |
  | `-p vibex-agent-acp --lib runtime_probe::tests` | Probe evidence/lifecycle helpers, projected home selection, protected environment keys and Windows junction rejection; no real Agent or model request. |

  Desktop has an implicit Cargo library target at `apps/desktop/src/lib.rs`;
  its `code_workbench` module owns these tests. `--bin vibex-desktop` would
  select the launcher instead. These are targeted regressions, not the full
  workspace test suite, a real model call, or native GUI acceptance.
  Run the credential suite in its own Cargo invocation: a workspace-wide run
  may unify desktop's `os-keychain` feature. That test is ignored in this case
  and asserts the feature is off even if manually forced with `--ignored`.
  The dedicated CI gate rejects ignored tests and must execute the HostFile
  path without the unit-test in-memory secret store or OS keychain access.
- Parse workflow YAML and PowerShell, `node --check` the packager, and run
  `pnpm check:frontend` for changed build tooling.
- Run `node --test scripts/run-model-review-tests.test.mjs` to cover artifact
  selection, non-empty discovery, summary/count checks, subprocess failure,
  timeout and stale or missing manifests. Unit tests use fixtures, not a native
  Cargo rebuild. The first cloud run must independently prove the real suites.
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

## 8. Windows accessibility shutdown

- AccessKit providers can receive COM geometry queries after a native HWND is
  destroyed. Normal app exit must disconnect process-owned UIA providers before
  GPUI clears its windows, not inside an async quit future that runs afterwards.
- Queue the cancellable exit preparation on the foreground executor, outside
  synchronous SendMessage handling. Guard current UI-thread top-level and child
  windows against reentrant WM_GETOBJECT, then call UiaDisconnectAllProviders.
  Keep the guards until WM_NCDESTROY on success; restore them and preserve the
  running app on failure. The synchronous on_app_quit guard is idempotent fallback.
- Windows quit-on-close refuses default window destruction, including repeated
  close requests. Close-to-tray hides and restores the same HWND instead of
  destroying it without an application shutdown. Other platforms retain their
  existing close policy. Do not disable accessibility or patch third-party crates.
- Keep the normal Win32 message loop running while GPUI shutdown queues its
  asynchronous window destruction. After installing guards but before UIA
  disconnect, record native `GpuiFast::Window` HWNDs, including logically removed
  windows still awaiting destruction. Run `App::shutdown` with explicit quit
  mode, and await their WM_NCDESTROY
  acknowledgements outside the AppCell borrow before posting quit. A second
  shutdown from the platform must have no new windows or quit observers.
  Stop state rejects duplicate quits and workbench restore/reopen. A drain
  deadline logs failure and keeps the loop alive; never call process::exit,
  manually destroy the platform HWND, or swallow TLS panics to force success.
- The direct raw-window-handle dependency reuses the locked 0.6.2 package; keep
  the lockfile and SBOM dependency edge synchronized without moving package pins.
- Validate rollback on subclass/disconnect errors, the synchronous ordering, and
  a native UIA client retaining a provider across destruction. GUI acceptance
  requires repeated exit/reopen, tray hide/restore, no panic in stderr, persisted
  state, and unchanged real user data. Exit code zero alone is insufficient.
- Forced OS session termination and individual detached-window destruction are
  separate boundaries: a quit callback cannot cancel OS teardown, and a process
  disconnect must not be used to retire one window while others remain active.
  Log skipped/failed shutdown protection; do not claim those paths were covered
  by normal-exit smoke tests.
