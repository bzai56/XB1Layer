# Roadmap

1. ~~**Import report**: `--imports` flag that prints every stubbed `dll!name`, grouped by DLL.~~ Done.
2. ~~**DLL search path**: let the loader resolve bundled DLLs from the title's own folder.~~ Done.
2b. ~~**`kernelx` aliasing**: forward `kernelx.dll` to `kernel32.dll`.~~ Done (6 console-only functions remain).
3. ~~**Process bootstrap**: exception tables, PEB image base, static TLS, TLS callbacks.~~ Done for the main thread and `CreateThread` threads. Remaining: loader-list registration, TLS for other thread sources.
3b. ~~**Find the fast-fail** the FH3 demo hits.~~ Done: an uncaught WinRT `E_INVALIDARG` exception after `CoreApplication` activation (see [FINDINGS.md](FINDINGS.md)).
3c. **WinRT shell layer**: our own `CoreApplication`, `CoreApplicationView`, `CoreWindow` and dispatcher on a native window, plus tracing of WinRT vtable calls.
3d. **`G:` content root**: map the title folder as the game-content drive.
4. **`kernelx` shim**: real implementations of the most-called functions, driven by stub-hit logs.
5. **`xg_x` and `EtwPlus`**: resource-layout helpers and no-op tracing.
6. **GPU**: `d3d11_x` onto desktop D3D11, then D3D12.X for newer titles.
7. **Audio, input, services**: stubs first, real backends later.
8. **Per-section memory protections** instead of RWX.
