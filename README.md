# XB1Layer

An experimental compatibility layer for running Xbox One (GDK/XDK "Durango") x64 titles on Windows x64.

**Status: early prototype.** Today it is a PE64 loader that maps a title's executable and reports which console-only APIs it needs. It does not run games yet.

## What works
- Manual PE64 mapping (headers, sections, base relocations)
- Import resolution against real Windows DLLs
- Logging stubs for unresolved imports (console-only DLLs such as `d3d11_x.dll`, `xg_x.dll`)
- `kernelx.dll` forwarded to `kernel32.dll`; DLLs next to the title are searched first
- On the FH3 demo: 701 imports resolve natively, 39 remain stubbed
- Successfully loads the unpacked Forza Horizon 3 kiosk demo binary in load-only mode

## Build and run
Requires Rust (MSVC toolchain, x64).

```
cargo build
cargo run -- path\to\title.exe          # load only, print import summary
cargo run -- path\to\title.exe --imports  # also list every stubbed import, grouped by DLL
cargo run -- path\to\title.exe --run    # also jump to the entry point (will crash on real titles for now)
```

## Scope and legal
- Works only with content you own and have already unpacked/decrypted by legitimate means.
- This project does not include, and will not include, code for breaking package encryption, extracting keys, or bypassing DRM.
- No Microsoft binaries or game assets are included in this repository.

## Docs
- [Architecture](docs/ARCHITECTURE.md)
- [Findings: Forza Horizon 3 kiosk demo](docs/FINDINGS.md)
- [Roadmap](docs/ROADMAP.md)
