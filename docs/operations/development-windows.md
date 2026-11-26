# Windows cloud development builds

Push development changes to `cloud-development` to run
`.github/workflows/development-windows.yml`, or select that branch when manually
dispatching **Development Windows** in GitHub Actions. A newer run cancels an
older run for the same ref. The workflow has read-only repository permission,
uses no secrets, and never creates a release, tag, installer, or package-manager
registration.

The single Windows Server 2022 x64 job installs the Rust version from
`rust-toolchain.toml`, Node.js 22, pnpm 11.3.0, Visual Studio C++ tools and NASM.
It installs the frozen pnpm dependencies before building:

```powershell
$env:CARGO_BUILD_JOBS = '2'
$env:CARGO_INCREMENTAL = '0'
$env:CARGO_PROFILE_DEV_DEBUG = '0'
$env:CARGO_PROFILE_DEV_DEBUG_ASSERTIONS = 'false'
$env:CARGO_PROFILE_DEV_BUILD_OVERRIDE_DEBUG_ASSERTIONS = 'false'
$env:VIBEX_CHANNEL = 'preview'
$env:VIBEX_INSTALL_PACKAGE = 'unmanaged'
cargo build -p vibex-desktop -p vibex-client --locked --jobs 2
node scripts/package-development-windows.mjs --output target/development-artifact
```

This is Cargo's `dev` profile, with debug symbols, debug assertions (including
build scripts) and incremental output disabled. The repository's dependency
optimization remains in force. The job
has a 120-minute limit and does not save or upload Cargo's `target` directory.
The initial workflow uses no dependency cache, so a clean build does not depend
on a previous run's state.

The two debug-assertion settings are required for portability. GPUI's Windows
debug renderer loads HLSL files from absolute Cargo source and build-output
paths. Disabling debug symbols alone does not select its embedded shaders.
With debug assertions disabled in both the library and its build script, the
Windows SDK's `fxc.exe` compiles the shaders into embedded DXBC bytecode. The
workflow explicitly locates FXC and sets `GPUI_FXC_PATH`. A local equivalent
build needs the same SDK compiler available.

## Download and run

Download **vibex-development-windows-x64** from the successful run's Artifacts
section within seven days. Extract it into a new, writable directory. The
payload contains `vibex-desktop.exe`, `vibex.exe`,
`Start-VibexDevelopment.ps1`, license texts, this guide and `manifest.json`.
Keep each downloaded build in its own directory.

Before launching, compare `manifest.json`'s `repository`, `headSha` and `runId`
with the selected run and verify each entry in `files` with
`Get-FileHash -Algorithm SHA256`. A successful download alone is not validation
of the selected commit.

```powershell
# Run from the extracted artifact directory. Choose a development-only data path.
.\Start-VibexDevelopment.ps1 -DataRoot ..\vibex-development-data
# The terminal client joins the same home/runtime:
.\Start-VibexDevelopment.ps1 -DataRoot ..\vibex-development-data -Tui
```

The launcher sets `VIBEX_INSTALL_PACKAGE=unmanaged` at runtime, preserving the
portable deployment. It maps `VIBEX_DB_PATH` to `<DataRoot>/vibex.db` and
`VIBEX_HOME` to `<DataRoot>/desktop-preview`: the desktop preview app derives its
home by appending `desktop-preview` to the database path's parent. This keeps
Desktop and TUI together without opening the installed application's home.
It does not add anything to PATH or change an installed application's files.
Launching the executable directly skips these development defaults.

Relative `-DataRoot` paths resolve from the current PowerShell location. The
launcher opens a visible, normal desktop window. This isolates the Vibex
database and runtime home; it does not isolate the Windows credential store,
`CODEX_HOME`, other Agent configuration directories, or inherited environment
variables. Existing system credentials or Agent configuration may remain
available. Use separately configured credentials and Agent homes when an
acceptance run requires that additional isolation.

Windows must provide its normal system libraries and compatible graphics
drivers. A machine missing the Microsoft Visual C++ x64 runtime may need that
runtime installed. Agent runtimes and credentials are configured locally by the
application; they are not included in this artifact.

## Assets and validation boundary

The desktop embeds its fonts, icons and application resources. Inter's exact
5.2.8 files come from the frozen pnpm install; the other font files are checked
in. Rust's `include_bytes!`/`include_str!` compilation fails if these required
inputs are absent. The package includes the corresponding font license texts
and existing third-party notices.

PDFium is omitted on Windows, matching the existing desktop release packager.
`docs/licenses/pdfium-7881-review.json` approves only Linux x64 distribution and
marks Windows validation as deferred. This package does not claim native PDF
preview support. Changing that policy requires reviewing the Windows runtime,
its complete license payload and native rendering validation together.

The packager validates both executable PE headers, copies only its allowlisted
payload, then runs the copied desktop's `--probe` and TUI's `--version` with a
fresh home outside the artifact. The desktop probe must report Windows x86_64,
preview identity, and the expected contract schema. It is a compiled contract
probe: it does **not** open a GUI, prove a rendered first frame, test Agent
credentials, or validate a real local workspace. After download, launch the
desktop locally and check the window, text rendering, terminal and the changed
feature before accepting the build.

Before copying the payload, a separate shader gate reads GPUI's generated
`debug/build/gpui-fast-windows-*/out/shaders_bytes.rs`, requires all 16 expected
FXC shader arrays and checks that each exact byte sequence occurs in the desktop
EXE. It also rejects the known runtime HLSL source/output path strings. This
catches the source-dependent debug renderer even on a hosted runner without a
usable graphics adapter. It is a portability check, not a claim of rendered
pixels or complete GPU compatibility. Use a fresh target directory for this
workflow; ambiguous multiple generated GPUI binding files are rejected.

## Manifest contract

`manifest.json` has integer `schemaVersion: 1`, `project: "vibex"`,
`repository` (`owner/repo` in CI), `headSha`, string `runId` and `runAttempt`,
`version`, `platform: "windows"`, `architecture: "x64"`, `profile: "dev"`,
`channel: "preview"`, `entrypoint: "vibex-desktop.exe"`,
`installPackage: "unmanaged"`, build settings, check results and resource status.
`build.debugAssertions` and `build.buildScriptDebugAssertions` are `false`;
`checks.embeddedWindowsShaders` records `status: "passed"`, `shaderCount: 16`
and `format: "DXBC"`; `resources.windowsShaders` is `"embedded_dxbc"`.
Each `files` entry has a forward-slash relative `path`, byte `size` and lowercase
hex `sha256`. It covers every payload file except the manifest itself, which
cannot contain its own digest. No absolute build paths, probe home, application
data, credentials, dependency trees or logs enter the artifact.

`checks.nativePixelsVerified` is always `false` and
`checks.localGuiAcceptance` is `"required"`. These fields prevent cloud probe
success from being mistaken for local visual acceptance. The packager refuses
an existing output directory to avoid mixing builds. For a local packaging
diagnostic, `--binary-dir <debug-directory>` selects already built executables;
without GitHub run metadata the manifest is marked `repository: "local"` and
`runId: "local"`. Such a package is not evidence of a cloud build.
