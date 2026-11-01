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

## First runs (`--run`)
Calling the entry point after the process bootstrap (see [ARCHITECTURE.md](ARCHITECTURE.md)) gets the title through CRT startup and a long stretch of static initialization (20,000+ traced API calls) before it dies. What it took to get there, in order:

1. **Stack size.** The title overflowed Rust's 1 MB main-thread stack and died silently. Running it on a 256 MB-reserve thread fixed the random hangs and crashes.
2. **Console `VirtualAlloc` flags.** The title reserves seven large regions at fixed addresses (for example `0x60000000000` for 12 GiB, `0x800000000`, `0x900400000`) with allocation-type bits that desktop Windows rejects with `ERROR_INVALID_PARAMETER`: `0x40000000` and `0x10000000` (plus `0x20000000`, large pages). With every reservation failing, its pool allocator computed `size / element_size = 0`, called `HeapAlloc(0)` and then looped on `count - 1`. Stripping the unsupported bits and keeping the address hint fixed this; all seven reservations now succeed at the requested addresses, and later commits use `0x40000000` as well.
3. **Static TLS on every thread.** The title uses `__declspec(thread)` and starts worker threads. Threads created through `CreateThread` now get their own TLS block via a trampoline.
4. **`XMemSetAllocationHooks`** is the first console-only stub the title actually calls, and it returns 0.

## Where it stops now
The process ends with `0xC0000409` (fast-fail), which bypasses exception handlers, so the crash logger prints nothing. The last traced activity is heavy string processing (`isalpha`, `tolower`, `strcpy_s`) after thousands of `memcpy` calls, so the title is still in initialization, not in a game loop. Finding the fast-fail site needs either a debugger or a ring buffer of the last API calls that survives process death.

## Next investigation steps
1. Locate the fast-fail (ring buffer or debugger), then implement whatever it was waiting for.
2. Implement `XMem*` and `TitleMemoryStatus`.
3. Inspect `media/` to see whether content is plain or encrypted (encrypted content is out of scope).
4. Work out which D3D11.X-only interfaces and methods the title calls on the returned device.
