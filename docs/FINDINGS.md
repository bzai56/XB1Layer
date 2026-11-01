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

## Where it stopped before the shell layer
(Historical: this is the state before our own `CoreApplication` was added; see the next section for the current state.) The process ends with exit code `0xC000027B` (`STATUS_STOWED_EXCEPTION`). One earlier run ended in `0xC0000409` instead; I did not find out why, and later runs all gave `0xC000027B`. Found with the crash-proof ring trace (`--ring=`, see [ARCHITECTURE.md](ARCHITECTURE.md)):

1. The title is a **C++/CX (WinRT) app**: it imports `vccorlib140.dll` and `combase.dll`.
2. It first looks for `g:\hash.manifest`. `G:` is the console's game-content drive, which does not exist here, so `GetFileAttributesExA` and `FindFirstFileExA("g:\*")` fail with `ERROR_PATH_NOT_FOUND`. It then falls back to `.\hash.manifest` (found because the working directory is the title folder).
3. It activates `Windows.ApplicationModel.Core.CoreApplication` (`ICoreApplication`, IID `0AACF7A4-5E1D-49DF-8034-FB6A68BC5ED1`) successfully through the real Windows runtime.
4. It then reads `hash.manifest` as 12-byte records (an 8-byte hash and a `0xFFFFFFFF` marker) and stops after about 100 of the file's roughly 2,260 records.
5. A WinRT call returns `E_INVALIDARG`. The C++/CX HRESULT-to-exception mapper at `title+0x227454` raises `Platform::InvalidArgumentException`, nothing catches it, and the title calls `RoFailFastWithErrorContext` from `title+0x2859bcf`.

**What is established:** the title fail-fasts on an uncaught `E_INVALIDARG` WinRT exception, after activating `CoreApplication` and while reading `hash.manifest`.

**What is not established:** which WinRT call returned `E_INVALIDARG`. WinRT method calls go through COM vtables, not through imports, so the API trace cannot see them. A reasonable guess is that the title runs inside the real `CoreApplication`, whose view and window objects do not exist for an unpackaged desktop process, but that has not been verified.

## Under the WinRT shell layer
With our own `CoreApplication` in place (see [ARCHITECTURE.md](ARCHITECTURE.md#winrt-shell-layer-srcwinrtrs)), the title's startup is visible:

1. `CoreApplication::Run` → `IFrameworkViewSource::CreateView` succeeds → `IFrameworkView::Initialize` is entered and runs for a long time. `SetWindow`, `Load`, the `Activated` event and `Run` have not been reached yet.
2. Inside `Initialize` the title calls console-only imports: `DXGIXSetVLineNotification` (still a stub), `XMemAllocDefault` and `TitleMemoryStatus`. With those stubbed it wrote through a NULL pointer; after implementing the `XMem*` family it got past that.
3. It then activates **`Windows.Xbox.Management.Deployment.PackageTransferManager`**, a class that exists only on the console, so Windows answers `REGDB_E_CLASSNOTREG` (`0x80040154`). The C++/CX runtime throws a `ClassNotRegisteredException`, the title catches it and retries 594 times in the ring trace, then reaches a NULL read in its catch path.
4. The statics interface it asks for has IID `D6CF4CAE-9EB1-4CBE-B426-7E75D429F846`. With a generic stand-in object (below) the title calls vtable slot 6 with an out-pointer, and then slot 7 on the result with a small changing value in the first argument register (`0xa9f`, `0xa95`, `0x8b`, `0x4f`, ...). The varying values suggest slot 7 may take no arguments, or that its arguments are not in that register, but this is a guess.
5. The run now ends with `rax = 0x80004002` (`E_NOINTERFACE`) followed by a NULL read at `title+0x1616fd1`: some `QueryInterface` the title makes fails. Which object, and which interface, is not yet known.

**What is established:** the title needs the `Windows.Xbox.*` WinRT surface, starting with `PackageTransferManager`, and it activates it very early.

**What is not established:** the real method names, signatures and semantics of that class. There is no metadata for it on this machine, so slot numbers are the only handle. The stand-in's behavior (success without output, or a fresh stand-in object for an out-pointer) is a heuristic to keep the title moving, not an implementation.

## The failed QueryInterface, and what came after
The `E_NOINTERFACE` at `title+0x1616fd1` was the title querying the standard Windows audio device enumerator (`CoCreateInstance` of `CLSID_MMDeviceEnumerator`, `BCDE0395-E52F-467C-8E3D-C4579291692E`) for interface `8B557ADC-555E-47A0-B223-43477E481DAD`, which exists only on the console. The title calls slot 9 on it with a pointer to its own object (looks like registering a callback), then slot 8 with two out-pointers that look like 32-bit values. The enumerator is now wrapped in a proxy that forwards the standard interface and answers that IID with a small stand-in (slot 8 reports zeros, slot 9 accepts). Meaning of those calls is unknown.

With that in place the lifecycle goes much further: `Initialize` returns, then `SetWindow`, `Load`, the `Activated` event (the title's handler runs and calls `CoreWindow::SetPointerCursor`) and `IFrameworkView::Run` are reached. Inside `Run` the title calls **`D3D11XCreateDeviceX`**, which is still a stub, so the next real wall is the graphics device.

Two more console-only interfaces are requested from the `CoreApplication` factory, `9FF3799E-6E83-468C-A5B0-501CE6BB0FA1` and `42A6A529-3CC6-4867-B5B7-6D1FAF5325F9`. Neither is in the Windows metadata. The title handles `E_NOINTERFACE` for them. Answering them with the generic stand-in made things worse (the title called a returned pointer as code and jumped to address `0x47`), so they stay unimplemented. This also shows the limit of the generic stand-in: its "write a new object into the first out-pointer" guess is wrong for some methods.

## Graphics device and the next fault
`D3D11XCreateDeviceX` now creates a real desktop D3D11 hardware device (feature level 11.1, checked with `xb1layer --test-d3d`) and returns it with its immediate context. The title's parameter struct starts with `0x00020011` (looks like a version) and `0x04000000` (a flags word with a console-only bit that desktop D3D11 would reject, so flags are not forwarded). The rest of the first 64 bytes is mostly zeros and a few pointers. Its meaning is not decoded.

The title calls this on a worker thread. Meanwhile the UI thread, inside `IFrameworkView::Run`, calls `CoreWindow::Bounds` and then reads through a NULL pointer at `title+0x156fab7`. The code there calls vtable slot 7 on a COM object with a pointer to a two-field local, expects a result back in that local, and then dereferences it. It is next to a `Platform::WeakReference` resolve (`vccorlib140!ResolveWeakReference` is the last WinRT call in the trace). Which object that is, and what it should return, is not known. Because the process dies on that thread, the log of the device creation finishing never prints in a title run, so completion is only verified by the standalone test.

**What is established:** device creation works in isolation. **What is not:** whether the title accepts a plain desktop device (it will likely call console-only `ID3D11DeviceX` methods and `DXGIX*` functions), and what the NULL read depends on.

## Next investigation steps
1. Find out which object the NULL read at `title+0x156fab7` goes through (its slot-7 call returns nothing).
2. Implement `DXGIXSetVLineNotification`, `DXGIXPresentArray` and any other `DXGIX*` the title uses.
3. Work out the real shape of the `PackageTransferManager` interfaces, from the title's own usage or from the console SDK's published API reference if available, and implement them for real.
3. Implement `DXGIXSetVLineNotification` and the rest of the `d3d11_x` surface.
4. Provide a `G:` equivalent: map the title folder as the content root instead of relying on the working directory.
5. Inspect `media/` to see whether content is plain or encrypted (encrypted content is out of scope).
6. Back the window with a native `HWND` and raise `CoreWindow` events.
