# Roadmap

The goal is a layer that runs many Xbox One titles on Windows x64 without graphical glitches. Today nothing is drawn. Details of each item are in [FINDINGS.md](FINDINGS.md).

**Focus: Sonic Mania.** It is far simpler than the Forza Horizon 3 kiosk demo (plain D3D11 device, about eleven context calls a frame, XAudio2), so it is the vehicle for the first real picture. The Forza items below are parked until Sonic Mania renders.

## Next up: Sonic Mania (in order)

1. **Find why Sonic Mania's draws show nothing.** The context calls, the inlined vertex shader, pixel shader and input layout are bound and the device is no longer removed, but the window stays black and the debug layer warns that vertex slot 0 is too small for the draw (stride 20, 30 indices). Check the vertex and index buffer contents (Map/Unmap path, buffer sizes), the viewport and the render target the draw writes to.
2. **First picture.** Run Sonic Mania and get the title's 2D scene into the window.
3. **Hunt glitches.** Compare with the expected output, fix formats, blending, sampling and de-tiling that look wrong.
4. **Input.** Gamepad and keyboard so the game can be played.

## Parked: Forza Horizon 3 kiosk demo

1. **Get the title to render.** It boots into its main loop and stays there but never draws or presents. Find what its loading screens wait on (the `CPresentation` readiness flag, window events the shell never raises, the database gate in FINDINGS). Until it renders there are no draw packets to decode.
2. **Remove the startup gaps.** The test title now stops on a different Xbox stand-in gap from run to run, before its first present. Each gap is a console WinRT method whose shape was guessed wrong; fix them from the call sites until the title reaches its first present.
3. **Swap chain and window.** `DXGIXPresentArray` and `DXGIXSetVLineNotification` are logging stubs. Create a native window, a desktop swap chain, and present the title's display planes.
4. **Decode the GPU command packets.** The stream is AMD PM4 (see FINDINGS). So far it contains only two repeating idle-loop packet kinds and no draws, because the title is stuck before rendering; decode draws, state, binding, clears and copies once it gets that far, and turn them into desktop D3D11 calls. This is the core of rendering.
5. **Textures.** Decode the console tile modes and de-tile placed textures (placed buffers are already mirrored).
6. **Identify the remaining context-table indices** (24 to 33, 64 and up). Only evidenced indices forward to the driver; forwarding guesses corrupted it.

## Open problems

- The `0x87D` exit under the debug layer (`XB1_D3D_DEBUG=1`): a NULL-execute in a thread, then termination. Unexplained.
- Console `ICoreWindow` layout: pin down the members, then raise window events (activation, size, visibility, input).
- The two `CoreApplication` console extension interfaces (`42A6A529`, `9FF3799E`) and the real shapes of `PackageTransferManager`.
- Device slot 73 and context `+0xA08` / `+0x4E0`.
- Gamepad input (the controller list is empty) and a real signed-in user.
- A `G:` content root for the title folder.

## Later

- `kernelx` shim: real implementations of the most-called functions, driven by stub-hit logs.
- `xg_x` (resource layout helpers) and `EtwPlus` (no-op tracing).
- D3D12.X for newer titles.
- Audio, input and services: stubs first, real backends later.
- Per-section memory protections instead of RWX.
- Test more titles than the Forza Horizon 3 kiosk demo and replace title-specific stand-ins with general implementations.

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
- Xbox user list (one user), event subscriptions, strings, `Package.Current`, `Store.Product`, `XboxLiveConfiguration`, and an empty controller list.
- Diagnostics: ring trace, `--watch-sites`, `--patch-fastfail`, `--test-d3d`, D3D11 debug layer mode, exit and thread logging.
