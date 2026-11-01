# XB1Layer

An experimental compatibility layer for running Xbox One (GDK/XDK "Durango") x64 titles on Windows x64.

**Status: early prototype.** Today it is a PE64 loader that maps a title's executable and reports which console-only APIs it needs. It does not run games yet.

## What works
- Manual PE64 mapping (headers, sections, base relocations)
- Import resolution against real Windows DLLs
- Logging stubs for unresolved imports (console-only DLLs such as `d3d11_x.dll`, `xg_x.dll`)
- `kernelx.dll` forwarded to `kernel32.dll`; DLLs next to the title are searched first
- On the FH3 demo: 701 imports resolve natively, 39 remain stubbed
- Process bootstrap (unwind tables, PEB image base, per-thread static TLS) and console `VirtualAlloc` handling
- The FH3 demo runs through CRT startup and static initialization and into `IFrameworkView::Initialize`, then fails after a failed `QueryInterface`; it does not reach a game loop
- A WinRT shell layer (`src/winrt.rs`): our own `CoreApplication`, view, window and dispatcher run the title's `IFrameworkView` lifecycle. The FH3 demo now reaches `IFrameworkView::Initialize`, then needs the console-only `Windows.Xbox.*` WinRT classes (served by a generic discovery stand-in for now)
- `XMem*` console memory API
- `--ring=<path>` crash-proof trace and `tools\read-ring.ps1` to read it
- Successfully loads the unpacked Forza Horizon 3 kiosk demo binary in load-only mode

## Build and run
Requires Rust (MSVC toolchain, x64).

```
cargo build
cargo run -- path\to\title.exe          # load only, print import summary
cargo run -- path\to\title.exe --imports  # also list every stubbed import, grouped by DLL
cargo run -- path\to\title.exe --run    # also run the entry point (real titles do not get far yet)
```

Debug flags for `--run`: `--watch=N`, `--trace-api`, `--trace-heap`, `--iat`. See [Architecture](docs/ARCHITECTURE.md#debugging-aids).

Run a title from its own folder so relative paths resolve.

## Scope and legal
- Works only with content you own and have already unpacked/decrypted by legitimate means.
- This project does not include, and will not include, code for breaking package encryption, extracting keys, or bypassing DRM.
- No Microsoft binaries or game assets are included in this repository.

## Docs
- [Architecture](docs/ARCHITECTURE.md)
- [Findings: Forza Horizon 3 kiosk demo](docs/FINDINGS.md)
- [Roadmap](docs/ROADMAP.md)
