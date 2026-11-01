# Roadmap

1. ~~**Import report**: `--imports` flag that prints every stubbed `dll!name`, grouped by DLL.~~ Done.
2. ~~**DLL search path**: let the loader resolve bundled DLLs from the title's own folder.~~ Done.
2b. ~~**`kernelx` aliasing**: forward `kernelx.dll` to `kernel32.dll`.~~ Done (6 console-only functions remain).
3. **Process bootstrap**: PEB/loader registration, TLS callbacks, exception table registration, correct `GetModuleHandle(NULL)`.
4. **`kernelx` shim**: real implementations of the most-called functions, driven by stub-hit logs.
5. **`xg_x` and `EtwPlus`**: resource-layout helpers and no-op tracing.
6. **GPU**: `d3d11_x` onto desktop D3D11, then D3D12.X for newer titles.
7. **Audio, input, services**: stubs first, real backends later.
8. **Per-section memory protections** instead of RWX.
