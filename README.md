# XB1Layer

An experimental compatibility layer for running Xbox One (GDK/XDK "Durango") x64 titles on Windows x64.

**The aim is to be compatible with a wide range of Xbox One games**, not one title. Anything specific to a single title is a stand-in to be replaced by a general implementation. **Sonic Mania** is the first title that runs, chosen because it is simple (plain D3D11 2D renderer, XAudio2). The Forza Horizon 3 kiosk demo that was used to find the first console API needs is retired; the next target is a full Forza Horizon game (2, 3 or 4) once a dump is available.

**Status: early prototype. Sonic Mania is playable with picture, sound and input.** The layer loads unpacked Xbox One titles, runs them through startup into their main loop, and presents their frames in a native window. The goal is a layer that runs titles without graphical glitches; it is not there yet. Only Sonic Mania has been checked for that, and only the parts played so far (intro, menus and the early part of the first zone).

## What works
- **Sonic Mania** boots and plays: intro, logos, menus and the first zone, with picture (desktop D3D11 through the layer's proxy device, 60 FPS limiter) and sound (XAudio2). No graphical glitches seen in what was played. Not yet tested: later zones, special stages, two-player split screen, DualSense over USB.
- **Input** (Sonic Mania): an XInput gamepad, a DualSense (USB or Bluetooth, read directly through HID, Bluetooth verified), or the keyboard (arrows, Z/X/A/S = A/B/X/Y, Enter = Menu, Backspace = View, Q/W = shoulders) while the window has focus.
- **Loader**: manual PE64 loader, import report, `kernelx.dll` forwarded to `kernel32.dll`, DLLs next to the title searched first, the title's own console DLLs mapped by the layer.
- **Process bootstrap**: exception tables, PEB image base, per-thread static TLS, console `VirtualAlloc` flag handling, `XMem*` and `TitleMemoryStatus`, `G:` mapped to the title folder.
- **WinRT shell** (`src/winrt.rs`): own `CoreApplication`, view, window and dispatcher run the title's lifecycle; console-only `Windows.Xbox.*` and `Microsoft.Xbox.*` classes are served by stand-ins (one signed-in user, a gamepad, `SystemUI`, strings, event subscriptions) whose method shapes come from call sites.
- **Graphics** (`src/gpu.rs`, `src/display.rs`): `D3D11XCreateDeviceX` and plain `D3D11CreateDevice` return proxy device and context objects over a real desktop device. The console's embedded context function table is decoded where evidenced (including the packed `DrawIndexed` and the state the title keeps in the context object instead of calling bind methods). The swap chain is redirected to a native window. `XB1_D3D_DEBUG=1` turns on the D3D11 debug layer.
- **Audio**: proxy for the console extension of `MMDeviceEnumerator`, console audio HAL stand-ins.
- **Diagnostics**: crash-proof ring trace (`--ring=`, `tools\read-ring.ps1`), `--watch-sites`, `--test-d3d`, per-call GPU logging.

## What does not work
- **Forza Horizon 3 kiosk demo** (retired): booted into its main loop but never drew or presented. It writes AMD PM4 command packets that nothing decodes, uses placed textures with console tiling that is not de-tiled, and its D3D11X present path (`DXGIXPresentArray`, `DXGIXSetVLineNotification`) is still a logging stub.
- **Other titles**: untested. Console context-table indices 23 and 24 to 31 are unidentified, and anything not evidenced by a logged call only logs.
- D3D12.X titles are not supported.

## Build and run
Requires Rust (MSVC toolchain, x64).

```
cargo build
cargo run -- path\to\title.exe            # load only, print import summary
cargo run -- path\to\title.exe --imports  # also list every stubbed import, grouped by DLL
cargo run -- path\to\title.exe --run      # also run the entry point
```

For example, Sonic Mania, from its own folder: `xb1layer.exe "SonicMania.exe" --run`.

Debug flags for `--run`: `--watch=N`, `--trace-api`, `--trace-heap`, `--iat`, `--ring=`, `--watch-sites=`. Environment: `XB1_D3D_DEBUG=1` (D3D11 debug layer), `XB1_LOG_ALL=1` (log every context call instead of forwarding), `XB1_FPS=<n>` (frame limit, default 60, 0 = off), `XB1_NO_GAMEPAD=1`, `XB1_INPUT_LOG=1` (print raw joystick changes to map an unknown pad), `XB1_NO_EXTEND=1`, `XB1_NO_MIRROR=1`. See [Architecture](docs/ARCHITECTURE.md#debugging-aids).

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
Contributions are welcome. The biggest open problems are mapping the rest of the console's embedded context functions to desktop D3D11 and, for titles that use it, decoding the GPU command packets (see the [Roadmap](docs/ROADMAP.md)). Reports of what other titles need are useful too.

## Docs
- [Architecture](docs/ARCHITECTURE.md)
- [Findings](docs/FINDINGS.md) (Forza Horizon 3 kiosk demo and Sonic Mania notes)
- [Roadmap](docs/ROADMAP.md)
