# Roadmap

1. ~~**Import report**: `--imports` flag that prints every stubbed `dll!name`, grouped by DLL.~~ Done.
2. ~~**DLL search path**: let the loader resolve bundled DLLs from the title's own folder.~~ Done.
2b. ~~**`kernelx` aliasing**: forward `kernelx.dll` to `kernel32.dll`.~~ Done (6 console-only functions remain).
3. ~~**Process bootstrap**: exception tables, PEB image base, static TLS, TLS callbacks.~~ Done for the main thread and `CreateThread` threads. Remaining: loader-list registration, TLS for other thread sources.
3b. ~~**Find the fast-fail** the FH3 demo hits.~~ Done: an uncaught WinRT `E_INVALIDARG` exception after `CoreApplication` activation (see [FINDINGS.md](FINDINGS.md)).
3c. ~~**WinRT shell layer**~~ First version done: `CoreApplication`, view, window and dispatcher objects drive the title's `IFrameworkView` through `Initialize`. Remaining: a native `HWND` behind `CoreWindow`, window events and input, `GetForCurrentThread`, `ICoreApplicationExit`, tracing of WinRT vtable calls.
3d. **`G:` content root**: map the title folder as the game-content drive.
3e. ~~`XMem*` and `TitleMemoryStatus`~~ Done, with undecoded attributes and a guessed status layout.
3f. **`Windows.Xbox.*` classes**: replace the generic stand-in with real implementations, starting with `PackageTransferManager`. Needs the real interface shapes.
3g. ~~**Find the failing `QueryInterface`**~~ Done: the console audio extension of `MMDeviceEnumerator`, now proxied.
3h. ~~`D3D11XCreateDeviceX`~~ First version done (plain desktop device). Remaining: `ID3D11DeviceX`/`ID3D11DeviceContextX` extensions, `DXGIX*` functions, swap chain on a real window.
3i. ~~**NULL read at `title+0x156fab7`**~~ Done: the title's `ICoreWindow` has a console layout (slot 7 = Dispatcher). The title's main loop now runs.
3j. ~~**Worker-thread fault at `ntdll+0x5ac00`**~~ Done: the TLS pointer array was allocated with `VirtualAlloc` and `ntdll` freed it as a heap block when the title loaded `WindowsCodecs.dll`. Now allocated from the process heap.
3j2. ~~NULL write at `title+0x1960158`~~ Done: the title's own allocator crash after a `VirtualAlloc` with console-only protection bits (`0x10000`) failed; the shim now masks protection.
3j4. ~~**Static TLS owned by `ntdll`**~~ Done: a toolchain-built helper DLL, patched so the loader's block starts at the title's template. The hand-built vector remains as `--legacy-tls`.
3j5. **Extended D3D11 device and context** (`ID3D11DeviceX`, `ID3D11DeviceContextX`): the title's last fault calls vtable slot 321 of the context, far past the desktop vtable. Needs proxy objects with the console layout, and the reason `[r14+0x40]` is NULL.
3j3. **Decode the two `CoreApplication` console extension interfaces** (`42A6A529`, `9FF3799E`).
3k. **Console `ICoreWindow` layout**: pin down the member list, then raise window events (activation, size, visibility, input).
4. **`kernelx` shim**: real implementations of the most-called functions, driven by stub-hit logs.
5. **`xg_x` and `EtwPlus`**: resource-layout helpers and no-op tracing.
6. **GPU**: `d3d11_x` onto desktop D3D11, then D3D12.X for newer titles.
7. **Audio, input, services**: stubs first, real backends later.
8. **Per-section memory protections** instead of RWX.
