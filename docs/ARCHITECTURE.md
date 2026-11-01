# Architecture

## Premise
Xbox One titles are x64 PE binaries built for a customized Windows kernel ("Durango"/GameOS) on an x86-64 Jaguar CPU. No CPU emulation is needed. The work is replacing the console's system layer, so the design is closer to Wine than to a CPU emulator.

## Current components

### PE loader (`src/main.rs`, `load`)
1. Validates the MZ/PE headers: x64 machine type (0x8664) and PE32+ magic (0x20B).
2. Allocates `SizeOfImage` bytes, preferring the image's preferred base and falling back to any address (RWX for now).
3. Copies headers and each section to its virtual address.
4. Applies `IMAGE_REL_BASED_DIR64` relocations when the image moved.
5. Walks the import directory. For each imported DLL it calls `LoadLibraryA`, then `GetProcAddress` per function (names or ordinals).
6. Anything unresolved, whether the DLL is missing or the export is absent, gets a stub.

### Stubs (`StubArena`, `stub_hit`)
Each unresolved import gets a 32-byte executable thunk:

```
mov rcx, <stub index>
mov rax, <address of stub_hit>
jmp rax
```

`stub_hit` logs the import's `dll!name` on first call and returns 0. Original arguments are discarded, so stubs are for discovery only, not correct behavior.

## Known limitations
- The entry point is called directly. There is no PEB/loader-list registration, no TLS callbacks, no CRT/process setup, so `--run` on real titles will fail.
- `GetModuleHandle(NULL)` returns the host process, not the loaded title.
- Image memory is RWX instead of per-section protections.
- No delay-load imports, no import forwarders handling beyond what `GetProcAddress` does, no exception tables (`RtlAddFunctionTable`), so SEH will not work.

## Planned layers
| Layer | Replaces | Approach |
|-------|----------|----------|
| System | `kernelx.dll`, `xg_x.dll`, `EtwPlus.dll`, `AcpHal.dll` | Rust implementations exported through proper thunks |
| GPU | `d3d11_x.dll` (and D3D12.X for newer titles) | Map onto desktop D3D11/D3D12 |
| Services | `Microsoft.Xbox.Services.dll`, GameChat | Offline stubs |
| Process | Appx/UWP launch, `AppxManifest.xml` | Own process bootstrap that sets up PEB, TLS, SEH |
