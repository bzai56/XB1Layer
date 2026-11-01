# XB1Layer

An experimental compatibility layer for running Xbox One (GDK/XDK "Durango") x64 titles on Windows x64.

**The aim is to be compatible with a wide range of Xbox One games**, not one title. The Forza Horizon 3 kiosk demo is only the first test case, used to find out what the console APIs need; anything specific to it is a stand-in to be replaced by a general implementation.

**Status: early prototype. Nothing is drawn yet.** It loads and starts an unpacked Xbox One title (the Forza Horizon 3 kiosk demo is the current test case), runs it through startup into its main loop, and gives it a desktop Direct3D 11 device. The title writes its GPU work as console command packets that are not decoded yet, and there is no swap chain or window, so there is no picture. The goal is a layer that runs titles without graphical glitches; it is not there.

## What works
- Manual PE64 mapping (headers, sections, base relocations)
- Import resolution against real Windows DLLs
- Logging stubs for unresolved imports (console-only DLLs such as `d3d11_x.dll`, `xg_x.dll`)
- `kernelx.dll` forwarded to `kernel32.dll`; DLLs next to the title are searched first
- On the FH3 demo: 708 imports resolve natively, 32 remain stubbed
- Process bootstrap (unwind tables, PEB image base, per-thread static TLS) and console `VirtualAlloc` handling
- The FH3 demo runs through CRT startup, `Initialize`, `SetWindow`, `Load`, activation and into `IFrameworkView::Run`, where its main loop runs. Its thread-local storage is owned by `ntdll` through a generated helper DLL.
- Graphics: `D3D11XCreateDeviceX` returns proxy device and context objects over a real desktop D3D11 device. The console-only device methods for textures and buffers (slots 59, 62, 64, 65) are implemented on top of it; the title's render targets create successfully; placed buffers are mirrored to the GPU (verified), placed textures are not (console tiling is not decoded). Only the context-table entries backed by logged calls forward to the driver (indices 7, 11, 14, 15, 23 and 34 to 63); the rest log and do nothing. `XB1_D3D_DEBUG=1` turns on the D3D11 debug layer and logs its messages.
- The title writes GPU command packets into a 4 MB buffer each context owns; they are logged and discarded. Decoding them is the main missing piece. `DXGIXPresentArray` and `DXGIXSetVLineNotification` are logging stubs, so there is no swap chain.
- A WinRT shell layer (`src/winrt.rs`): our own `CoreApplication`, view, window and dispatcher run the title's lifecycle. Console-only `Windows.Xbox.*` and `Microsoft.Xbox.*` classes are served by logging stand-ins whose method shapes are guessed from call sites, with specific handling for the user list (one user), the controller list (empty), strings and event subscriptions.
- The title's own console DLLs next to it (the telemetry library) are mapped by the layer's loader, and `G:` is mapped to the title folder for the session so the title can read its game data (hundreds of zips load).
- **Where it stops:** a run now fails when the title queries its game database before it has been opened (`title+0xd93e57`), before its first present. See [Findings](docs/FINDINGS.md).
- `XMem*` console memory API
- `D3D11XCreateDeviceX` backed by a real desktop D3D11 device (`xb1layer --test-d3d` to check it), and a proxy for the console audio extension of the MMDevice enumerator
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

Debug flags for `--run`: `--watch=N`, `--trace-api`, `--trace-heap`, `--iat`, `--ring=`, `--watch-sites=`. Environment: `XB1_D3D_DEBUG=1` (D3D11 debug layer), `XB1_NO_EXTEND=1`, `XB1_NO_MIRROR=1`. See [Architecture](docs/ARCHITECTURE.md#debugging-aids).

Run a title from its own folder so relative paths resolve.

## Scope and legal
- Works only with content you own and have already unpacked/decrypted by legitimate means.
- This project does not include, and will not include, code for breaking package encryption, extracting keys, or bypassing DRM.
- No Microsoft binaries or game assets are included in this repository.
- Not affiliated with or endorsed by Microsoft. Xbox, Xbox One, Durango and Forza Horizon are trademarks of Microsoft; they are used here only to say what the software is compatible with.
- This project does not help obtain titles.

## License
[MIT](LICENSE).

## Contributing
Contributions are welcome. The biggest open problem is decoding the console GPU command packets (see the [Roadmap](docs/ROADMAP.md)); reports of what other titles need are useful too.

## Docs
- [Architecture](docs/ARCHITECTURE.md)
- [Findings: Forza Horizon 3 kiosk demo](docs/FINDINGS.md)
- [Roadmap](docs/ROADMAP.md)
