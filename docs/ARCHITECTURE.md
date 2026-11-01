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

### Process bootstrap (`bootstrap`, `--run` only)
Done on a dedicated thread with a 256 MB stack reserve, because titles expect far more than Rust's default:
- **Unwind tables:** the exception directory is registered with `RtlAddFunctionTable`, so SEH and C++ exceptions can unwind through title code.
- **PEB image base:** `PEB.ImageBaseAddress` is pointed at the title so `GetModuleHandle(NULL)` returns it.
- **Static TLS:** the TLS template is copied per thread and installed in `TEB.ThreadLocalStoragePointer` at a fixed high index (100); `_tls_index` is set to match. The array has no recorded length, so a larger copy of the first 16 entries is swapped in. **The array must be allocated from the process heap**, because `ntdll` frees and reallocates it with `RtlFreeHeap` when a DLL with static TLS is loaded; a `VirtualAlloc` block there crashes the loader. TLS callbacks run once for the main thread. The index is 60. **This design is fragile**: `ntdll` rebuilds every thread's vector when a DLL with static TLS loads and drops entries above its own size, so the title can lose its TLS mid-run (see [FINDINGS.md](FINDINGS.md#static-tls-keeps-getting-lost)).
- **Threads:** `CreateThread` is overridden to wrap the start routine in a trampoline that installs TLS first.

### WinRT shell layer (`src/winrt.rs`)
Titles are C++/CX (UWP) apps: their entry point calls `CoreApplication::Run(IFrameworkViewSource)`. The real Windows implementation cannot host them in a plain desktop process, so `GetActivationFactoryByPCWSTR` (imported from `vccorlib140.dll`) is overridden and class lookups are intercepted:

| Class | Handling |
|-------|----------|
| `Windows.ApplicationModel.Core.CoreApplication` | Our own `ICoreApplication`. `Run` calls the title's `IFrameworkView` lifecycle: `CreateView`, `Initialize`, `SetWindow`, `Load`, then raises `CoreApplicationView.Activated`, then `Run`, `Uninitialize` |
| `Windows.Xbox.*`, `Microsoft.Xbox.*` | A generic stand-in object (see below) |
| everything else | Passed through to the real Windows runtime |

Objects behind that lifecycle: a `CoreApplicationView` (`ICoreApplicationView`), a `CoreWindow` (`ICoreWindow`, all events accepted but never raised yet) and a `CoreDispatcher` (`ICoreDispatcher`: `RunAsync` queues work, `ProcessEvents` runs the queue and pumps Win32 messages, `ProcessUntilQuit` loops until `CoreWindow::Close`). `ICoreWindow` and the launch-activation args are implemented with the `windows` crate. `ICoreApplication`, `ICoreApplicationView` and `ICoreDispatcher` have no implementable trait there, so they are hand-written COM objects built on the crate's vtable structs, which gives the correct slot order. **The window the title sees is not the `windows`-crate `ICoreWindow`.** The title's `ICoreWindow` vtable layout differs from desktop (see [FINDINGS.md](FINDINGS.md#the-corewindow-layout-is-not-the-desktop-one)), so `SetWindow` and `CoreApplicationView::CoreWindow` hand it a generic object with `kind = 2` instead: slot 6 returns Bounds (assumed), slot 7 returns the dispatcher (confirmed), and every other slot logs and returns success, storing an event token where the arguments look like `add_Event(handler, &token)`. The `windows`-crate `Window` object still exists but is unused unless `--trace-window` instruments it. Not done: a native window handle behind `CoreWindow`, window events, input, `CoreWindow::GetForCurrentThread`, `ICoreApplicationExit`.

**Generic stand-in for console-only classes.** There is no metadata for `Windows.Xbox.*`, so its interfaces are unknown. The stand-in answers `QueryInterface` for any IID with itself and logs each method call by vtable slot. If the first argument looks like writable memory it stores a new stand-in there and returns success; otherwise it returns success without writing anything. This is a discovery aid that fakes behavior, not an implementation.

### Graphics (`src/gpu.rs`)
`D3D11XCreateDeviceX(params, &device, &context)` is overridden to call desktop `D3D11CreateDevice` (hardware, feature levels 11.1 then 11.0, BGRA support) and return the device and context. The console's parameter struct is logged (first 64 bytes) and otherwise ignored, and its flags are not forwarded. This is a plain desktop device: the console's extended interfaces (`ID3D11DeviceX`, `ID3D11DeviceContextX`) are not provided, nor are the `DXGIX*` functions (`DXGIXPresentArray` is still a stub). `xb1layer --test-d3d` creates a device without loading a title.

### MMDevice enumerator proxy
`CoCreateInstance` is overridden. When the title creates `CLSID_MMDeviceEnumerator`, the result is wrapped in a forwarding proxy (`IMMDeviceEnumerator`, 5 methods) that answers the console-only IID `8B557ADC-555E-47A0-B223-43477E481DAD` with a stand-in: slot 8 reports two zero 32-bit values, slot 9 accepts a pointer to the caller's own object. Anything else is forwarded to the real object.

### Console memory API
`XMemAlloc`/`XMemAllocDefault`/`XMemFree`/`XMemFreeDefault` allocate whole pages aligned to 64 KB; the attribute bits (alignment, memory type) are not decoded. `XMemSetAllocationHooks` is accepted and ignored. `TitleMemoryStatus` zero-fills the caller's struct and reports 5 GiB total / 4.5 GiB available at offsets 8 and 16; that layout is a guess.

### Import overrides (`override_for`)
Some imports are implemented by us instead of forwarded to Windows:

| Import | Why |
|--------|-----|
| `GetProcessHeap` | Returns a private guest heap so the title cannot corrupt the host's heap |
| `VirtualAlloc` | Strips console-only allocation-type bits and falls back to any address if a fixed-address reservation is unavailable |
| `CreateThread`, `CreateThreadpoolIo` | Install static TLS on new threads and on thread-pool I/O callback threads |
| `VirtualAlloc` | Also masks the protection argument to `0x7FF` (console-only bit `0x10000` otherwise fails with `ERROR_NOT_SUPPORTED`) |
| `XMem*`, `TitleMemoryStatus` | Console memory API (see below) |
| `GetActivationFactoryByPCWSTR` (vccorlib140) | Routes WinRT class lookups to the shell layer |

### Debugging aids
| Flag | Effect |
|------|--------|
| `--imports` | Lists every stubbed import by DLL |
| `--iat` | Prints `slot-RVA dll!name` for every import |
| `--watch=N` | After N seconds, briefly suspends the title thread and prints RIP plus title return addresses on its stack |
| `--trace-api` | Logs every natively resolved import call through a register-preserving thunk (noisy calls are filtered, capped at 20,000 lines) |
| `--trace-heap` | Logs `HeapAlloc`, `HeapFree` and `VirtualAlloc` arguments and results |
| `--ring=<path>` | Crash-proof trace: every native import call, with the title address that made it, goes into a 64 MB memory-mapped circular buffer that survives the process dying (fast-fails included). Also logs arguments and results for file calls (`CreateFile2`, `CreateFileW`, `ReadFile`, `GetFileInformationByHandleEx`, `FindFirstFileExA`, `GetFileAttributesExA`) and `GetActivationFactoryByPCWSTR`. Read it with `tools\read-ring.ps1` |
| `--no-peb-patch` | Leaves `PEB.ImageBaseAddress` pointing at the host (used to rule the patch out as a cause) |
| `--trace-window` | Replaces the `windows`-crate window's vtable with a logging copy (`CoreWindow slot N`). Only useful for the desktop-layout window object |
| `--test-d3d` | Creates the desktop D3D11 device through the `D3D11XCreateDeviceX` path without loading a title |
| `--heapcheck` | With `--watch`, polls `HeapValidate`. Experimental: it takes the heap lock and can stall a title that is itself stuck in the heap |

`tools\run-title.ps1 -Title <exe> -Seconds N -Ring` builds the command line, runs the title from its own folder with a timeout, and prints the exit code and stderr. The crash logger also reports MSVC C++ throws (`0xE06D7363`, first 30), which shows where a title throws.

The crash logger is a vectored exception handler that reports `title+RVA`, registers, access-violation target, memory-region state and return-address-looking stack values. It allocates nothing, because the title may have crashed inside the heap. It cannot see fast-fails (`int 0x29`), which bypass exception dispatch.

## Known limitations
- Threads not created through the title's `CreateThread` import (thread pools, fibers) do not get static TLS.
- Only one TLS callback pass, for the main thread; thread attach/detach callbacks are not run.
- Image memory is RWX instead of per-section protections.
- No delay-load imports or loader-list registration.
- Stubs lose their arguments and return 0, so they are for discovery only.

## Planned layers
| Layer | Replaces | Approach |
|-------|----------|----------|
| System | `kernelx.dll`, `xg_x.dll`, `EtwPlus.dll`, `AcpHal.dll` | Rust implementations exported through proper thunks |
| GPU | `d3d11_x.dll` (and D3D12.X for newer titles) | Map onto desktop D3D11/D3D12 |
| Services | `Microsoft.Xbox.Services.dll`, GameChat | Offline stubs |
| Process | Appx/UWP launch, `AppxManifest.xml` | Own process bootstrap that sets up PEB, TLS, SEH |
