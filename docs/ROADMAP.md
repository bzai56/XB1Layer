# Roadmap

The goal is a layer that runs many Xbox One titles on Windows x64 without graphical glitches. Today Sonic Mania is playable; no other title draws yet. Details of each item are in [FINDINGS.md](FINDINGS.md).

**Focus: Sonic Mania, then a full Forza Horizon.** Sonic Mania is far simpler than Forza (plain D3D11 device, about eleven context calls a frame, XAudio2) and now renders and plays. The Forza Horizon 3 kiosk demo is retired; the next big target is a full Forza Horizon 2, 3 or 4 dump supplied by the user.

## Next up (in order)

0. **Minecraft Xbox One Edition:** the main menu renders. Next: its input (old `IGamepad`), the unmapped embedded context calls, the topology (currently guessed), then the world.

1. **Play Sonic Mania through and hunt glitches.** Later zones, special stages, two-player split screen, DualSense over USB; compare every screen with the original (formats, blending, sampling).
2. **Get the full Forza Horizon 3 past its zip-mount crash** (NULL read at title+`0x1b04f7f`, see FINDINGS), then check whether its shaders are DXBC or GCN microcode, which decides the size of the graphics job. FH4 probably needs D3D12.X (unverified).
3. **Memory.** The demo asked for an 8.8 GB allocation and hit the commit limit; expect the same from a full game.
4. **Decode GPU command packets** (AMD PM4) into desktop D3D11 calls if the title uses them (Sonic Mania does not; Forza does).
5. **Textures.** Decode the console tile modes and de-tile placed textures.
6. **D3D11X present path.** `DXGIXPresentArray` and `DXGIXSetVLineNotification` are logging stubs; reuse the display module.
7. **Identify the remaining context-table indices** (23, 24 to 31, 64 and up). Only evidenced indices forward to the driver; forwarding guesses corrupted it.

## Open problems

- The `0x87D` exit under the debug layer (`XB1_D3D_DEBUG=1`): a NULL-execute in a thread, then termination. Unexplained.
- Console `ICoreWindow` layout: pin down the members, then raise window events (activation, size, visibility, input).
- The two `CoreApplication` console extension interfaces (`42A6A529`, `9FF3799E`) and the real shapes of `PackageTransferManager`.
- Device slot 73 and context `+0xA08` / `+0x4E0`.
- A real signed-in user (one stand-in user exists).
- A `G:` content root for the title folder.

## Later

- `kernelx` shim: real implementations of the most-called functions, driven by stub-hit logs.
- `xg_x` (resource layout helpers) and `EtwPlus` (no-op tracing).
- D3D12.X for newer titles.
- Audio, input and services: stubs first, real backends later.
- Per-section memory protections instead of RWX.
- Test more titles than Sonic Mania and replace title-specific stand-ins with general implementations.

## Done

- Import report (`--imports`), DLL search path (title folder first), `kernelx.dll` aliased to `kernel32.dll`.
- Process bootstrap: exception tables, PEB image base, static TLS (owned by `ntdll` through a generated helper DLL), TLS for `CreateThread` threads.
- Found the title's first fast-fail (an uncaught WinRT `E_INVALIDARG` after `CoreApplication` activation).
- WinRT shell layer: own `CoreApplication`, view, window and dispatcher drive `IFrameworkView` through `Run`; the console `ICoreWindow` layout (slot 7 = Dispatcher) was found.
- `XMem*` and `TitleMemoryStatus`; console `VirtualAlloc` flag handling.
- Audio: proxy for the console extension of `MMDeviceEnumerator`.
- Graphics: `D3D11XCreateDeviceX` over a real desktop device; proxy device and context; device slots 59, 62, 64, 65; placed-buffer mirroring; sanitizing resource creation (the title's render targets now create); the embedded context table decoded where evidenced.
- Fixed the `ntdll+0x5ac00` TLS array fault, the console-protection-bit allocator crash and the NULL render-target read.
- Fixed the GPU driver worker-thread fault (unverified context indices were being forwarded).
- Xbox user list (one user), event subscriptions, strings, `Package.Current`, `Store.Product`, `XboxLiveConfiguration`, and a one-gamepad controller list.
- Diagnostics: ring trace, `--watch-sites`, `--patch-fastfail`, `--test-d3d`, D3D11 debug layer mode, exit and thread logging.
- Sonic Mania: first picture, packed `DrawIndexed`, inlined state cache, SystemUI stand-in, 60 FPS limiter, input (XInput, DualSense over HID, keyboard).
