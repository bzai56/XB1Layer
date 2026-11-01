# Findings: Forza Horizon 3 kiosk demo

Package: `Kiosk-Opus_1.0.0.0_x64__8wekyb3d8bbwe` (unpacked Xbox One app, tested locally; not distributed here).

## Layout
- `forza_durango_release_final.exe` (~74 MB): plain x64 PE, loads without any decryption step.
- `AppxManifest.xml`: Xbox manifest (`mx:ApplicationEnvironment=title`, `OSName=era`), single app `ReleaseFinal`, GPU availability `variable`.
- Bundled DLLs: `Microsoft.Xbox.Services.dll` (~20 MB), `Microsoft.Xbox.GameChat.dll`, `Microsoft.Xbox.Input.Wheel.dll`, `commonEventLoggingLibrary_release.dll`, `psapi.dll`.
- `media/`: not yet inspected.

## Loader results (load-only)
- 16 sections, mapped at the preferred base `0x140000000`.
- 569 imports resolved natively, 171 stubbed.

## Console-only DLLs (not present on Windows)
| DLL | Role |
|-----|------|
| `kernelx.dll` | Console kernel API (memory, threading, system queries) |
| `d3d11_x.dll` | Console D3D11 variant: this title is D3D11.X, not D3D12.X |
| `xg_x.dll` | Xbox graphics helper (texture/resource layout) |
| `EtwPlus.dll` | Event tracing extensions |
| `AcpHal.dll` | Audio hardware abstraction |
| `commonEventLoggingLibrary_Release.dll` | Ships in the package folder; likely loadable once the DLL search path includes it |

## Import breakdown (initial 171 stubs)
Full list: [fh3-stubbed-imports-initial.txt](fh3-stubbed-imports-initial.txt).

| DLL | Stubbed | Notes |
|-----|---------|-------|
| `kernelx.dll` | 138 | All but 6 are standard Win32 names (`CreateFileW`, `EnterCriticalSection`, fibers, SRW locks, ...) |
| `commonEventLoggingLibrary_Release.dll` | 9 | C++ mangled exports; still fails to load because its own dependencies are console DLLs |
| `AcpHal.dll` | 8 | Audio processor heap and shape contexts (`Apu*`, `AcpHal*`) |
| `xg_x.dll` | 7 | Texture layout and tile-mode computation (`XGCompute*`, `XGCreateTexture*Computer`) |
| `EtwPlus.dll` | 4 | `Etx*` tracing: safe to no-op |
| `d3d11_x.dll` | 4 | `D3D11XCreateDeviceX`, `D3DMapEsramMemory`, `DXGIXPresentArray`, `DXGIXSetVLineNotification` |
| `MMDevAPI.DLL` | 1 | `SetWasapiThreadAffinityMask` |

## After aliasing `kernelx.dll` to `kernel32.dll`
Stubs drop from 171 to 39 (701 resolved natively). Remaining `kernelx` stubs, the genuinely console-specific ones:
`TitleMemoryStatus`, `XMemAlloc`, `XMemAllocDefault`, `XMemFree`, `XMemFreeDefault`, `XMemSetAllocationHooks`.

Caveat: aliasing resolves names only. Console semantics can differ (for example `VirtualAlloc` page sizes and large pages), so behavior will need checking once the title actually runs.

The GPU surface is small by import count: only 4 `d3d11_x` entry points are imported directly. The rest of the D3D11.X API is reached through the device object that `D3D11XCreateDeviceX` returns, so the shim will need to wrap that device and its interfaces.

## Next investigation steps
1. Inspect `media/` to see whether content is plain or encrypted (encrypted content is out of scope).
2. Implement `XMem*` and `TitleMemoryStatus`.
3. Work out which D3D11.X-only interfaces and methods the title calls on the returned device.
