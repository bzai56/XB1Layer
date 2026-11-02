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
- **Static TLS is owned by `ntdll`** (`src/tlsdll.rs`, `helper/xb1tls.rs`, `build.rs`). `build.rs` compiles a tiny helper DLL whose only thread-local is a 4 KB buffer and embeds it. Before the title thread is created, the DLL's TLS directory is patched (template start moved 8 bytes forward, past the linker's `_tls_start` marker, so the loader's block starts at the buffer), written to the temp directory and loaded. The loader assigns it a TLS index and from then on allocates, grows and copies its block for every thread, so the title's static TLS survives any later DLL load and is present on thread-pool and driver threads. The title's `_tls_index` is set to that index, and the DLL's template is filled with the title's own template. Limit: the title's TLS plus zero-fill must fit in 4 KB. A hand-assembled PE was tried first and the loader ignored its TLS directory ([FINDINGS.md](FINDINGS.md#static-tls-keeps-getting-lost)).
- **Fallback TLS (`--legacy-tls`, or if the helper fails):** the template is copied per thread and installed in `TEB.ThreadLocalStoragePointer` at index 60 in a larger copy of the first 16 entries. That array must come from the process heap (`ntdll` frees it with `RtlFreeHeap`), and it is fragile: `ntdll` rebuilds the vector when a DLL with static TLS loads and can drop the title's entry. TLS callbacks run once for the main thread.
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
`D3D11XCreateDeviceX(params, &device, &context)` is overridden to call desktop `D3D11CreateDevice` (hardware, feature levels 11.1 then 11.0, BGRA support). The console's parameter struct is logged (first 64 bytes) and otherwise ignored, and its flags are not forwarded. The device and context returned to the title are **proxies** around the real desktop objects:

- Standard slots (`ID3D11Device` 3 to 42, `ID3D11DeviceContext` 3 to 114) are forwarded by a 16-byte thunk per slot: `mov rax,[rcx+8]; mov rcx,rax; mov rax,[rax]; jmp [rax+slot*8]`. It swaps `this` for the real object and tail-jumps, so all argument registers and stack arguments are preserved.
- `GetImmediateContext`, `CreateDeferredContext` and the context's `GetDevice` are custom, so they return proxies, not raw objects.
- `QueryInterface`: standard non-D3D11 interfaces are answered by the real object; D3D11 device/context interfaces by the proxy; interfaces the real object rejects (console-only IIDs) by the proxy itself.
- Vtable slots beyond the standard range, and every 8-byte slot of the context proxy's embedded region (offsets `0x18` to `0x4018`), are logging stubs that return 0. The embedded region exists because the title calls function pointers stored inside the context object (`call [obj+0xA08]`), not through its vtable.
- The embedded region holds a copy of the desktop context method table starting at offset `0x3E0` (slot *n* at `0x3E0 + 8n`; see [FINDINGS.md](FINDINGS.md#decoding-the-embedded-context-functions)). Console indexes 0 to 23 forward to the same desktop slot (`QueryInterface`, `AddRef`, `Release` and `GetDevice` are the proxy's own), and indexes 34 to 113 forward to desktop index + 1: the console table lacks one method among desktop slots 24 to 34. Indexes 24 to 33 are logging stubs.
- Device extension slots implemented: 59 (a logging stand-in COM object), 64 (`CreatePlacementTexture2D` shape: an ordinary desktop 2D texture is created from the description; console-only `MiscFlags` bits are dropped, tile mode and placement memory are ignored) 65 (same, 3D) and 62 (`CreatePlacementBuffer` shape: a desktop constant buffer). Slot 73 is a dumping stub. Placed buffers are mirrored: draws, dispatches and copies go through a pre-sync thunk that uploads changed placement memory (`UpdateSubresource`) before forwarding. Placed textures are not mirrored yet.
- The device's standard `CreateBuffer`, `CreateTexture2D` and `CreateTexture3D` are sanitizing wrappers (console-only `MiscFlags` bits dropped, failures logged with the description); the four view creators retry with no description when the given one is rejected with `E_INVALIDARG`; created resources get an extended vtable (11 real slots, logging stubs, slot 13 on 2D textures returns width and height) by redirecting the object's vtable pointer.
- Each context proxy owns a 4 MB command buffer (cursor `[ctx+0x50]`, end `[ctx+0x58]`, grow function `+0xA68`). Forza Horizon 3 writes PM4 packets there (idle loops so far); Sonic Mania writes none. **Inlined state cache:** the title does not call bind methods for its vertex shader, pixel shader and input layout; it writes them into the context object (`+0x178` layout, `+0x180` VS, `+0x188` PS, dirty bits `+0x168`). `apply_inlined_state` binds them (found through the creating device slot, `CREATED_KINDS`) and sets a triangle-list topology before each draw; the object's desktop vtable is restored first because the desktop runtime rejects extended vtables.
- **Context table forwarding is evidence-based.** Forwarded to the desktop context: 7 (`VSSetConstantBuffers`), 10, 11 and 23 (`SetShader`, with `r8`/`r9` zeroed because the console takes only the shader), 14 and 15 (`Map`/`Unmap`), 16, 18, and the shifted indices 32, 34, 35, 42, 43, 46, 49, 52, 59, 63 (console index = desktop index - 1 from 32 on). Console-specific shapes, each wrapped in `src/gpu.rs`: 8 = `PSSetShaderResources(&view)` (view passed by address, start 0, count 1), 12 = `DrawIndexed` with `(count << 32) | first index` packed in one register and no base vertex, 18 = `IASetVertexBuffers`, 19 = `IASetIndexBuffer(format enum, buffer, offset)`, 43 = `RSSetViewports`. `XB1_LOG_ALL=1` turns the forwarding off (except slots 0 to 3, 14, 15) so every index logs its raw arguments. Every other index logs and returns; forwarding unverified ones corrupted the NVIDIA driver thread. Device slots 41 and 42 (`SetExceptionMode` on the desktop) are logging stubs; a console `DYNAMIC` constant buffer is created as `DEFAULT`. Only context offsets from `0x3E0` are function-pointer stubs; lower offsets are data fields. See [FINDINGS.md](FINDINGS.md#device-creation-methods-decoded-and-implemented).
- The logging stubs capture `this` and the next three arguments, show what each pointer points at, name the call site in the title, and log only the first six calls of each stub and then powers of two.
- On a crash, the logger lists which embedded fields differ from their original stub pointers, i.e. the state the title has written.

For titles that create their device with plain `D3D11CreateDevice` (Sonic Mania) `src/display.rs` runs a native window on its own thread, redirects `IDXGIFactory2::CreateSwapChainForCoreWindow` to `CreateSwapChainForHwnd` and substitutes empty parameters for the NULL `Present1` argument (this DXGI build crashes on NULL). `DXGIXPresentArray` and `DXGIXSetVLineNotification` (the D3D11X path, Forza) are still logging stubs, so that path has no swap chain or window. `xb1layer --test-d3d` creates a device and exercises the proxies without loading a title.

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
| `GetActivationFactoryByPCWSTR` (vccorlib140) | Routes WinRT class lookups to the shell layer (`Windows.Xbox.*`, `Microsoft.Xbox.*` and `Windows.ApplicationModel.Package`, which fails without package identity, get stand-ins) |
| `DXGIXPresentArray`, `DXGIXSetVLineNotification` (d3d11_x) | Logging stubs |
| `ExitProcess`, `TerminateProcess` | Log the caller, then forward |

### Debugging aids
| Flag | Effect |
|------|--------|
| `--imports` | Lists every stubbed import by DLL |
| `--iat` | Prints `slot-RVA dll!name` for every import |
| `--watch=N` | After N seconds, briefly suspends the title thread and prints RIP plus title return addresses on its stack |
| `--trace-api` | Logs every natively resolved import call through a register-preserving thunk (noisy calls are filtered, capped at 20,000 lines) |
| `--trace-heap` | Logs `HeapAlloc`, `HeapFree` and `VirtualAlloc` arguments and results |
| `--ring=<path>` | Crash-proof trace: every native import call, with the title address that made it, goes into a 64 MB memory-mapped circular buffer that survives the process dying (fast-fails included). Also logs arguments and results for file calls (`CreateFile2`, `CreateFileW`, `ReadFile`, `GetFileInformationByHandleEx`, `FindFirstFileExA`, `GetFileAttributesExA`) and `GetActivationFactoryByPCWSTR`. Read it with `tools\read-ring.ps1` |
| `--watch-sites=0xRVA,...` | One-shot breakpoints at title RVAs: report registers, text the argument registers point at, and the title's call chain, then continue. `--watch-repeat` keeps them armed and prints one compact line per hit (used on the title's logger at `0x16DDB90`) |
| `--patch-fastfail` | Replaces `mov ecx, code; int 0x29` sequences in the title with a breakpoint so the crash logger can report a fast-fail's site and code |
| `--legacy-tls` | Uses the hand-built TLS vector instead of the helper DLL |
| `--test-tls-load=<dll>` | Loads a DLL and reports which slots of the calling thread's TLS vector changed, and where its `xb1_block` buffer sits in its block. Used to check how the loader treats DLLs with static TLS |
| `--no-peb-patch` | Leaves `PEB.ImageBaseAddress` pointing at the host (used to rule the patch out as a cause) |
| `--trace-window` | Replaces the `windows`-crate window's vtable with a logging copy (`CoreWindow slot N`). Only useful for the desktop-layout window object |
| `--test-d3d` | Creates the desktop D3D11 device through the `D3D11XCreateDeviceX` path without loading a title |
| `XB1_D3D_DEBUG=1` (environment) | Creates the desktop device with the debug layer and logs its messages as `[d3d-debug]` (it found the `SetShader`, slot 41 and bad-index problems) |
| `XB1_NO_EXTEND=1`, `XB1_NO_MIRROR=1` (environment) | Disable resource vtable extension / placement-buffer mirroring, to rule them out |
| `--heapcheck` | With `--watch`, polls `HeapValidate`. Experimental: it takes the heap lock and can stall a title that is itself stuck in the heap |

`ntdll!NtTerminateProcess` is hooked (16-byte trampoline) so every process exit logs its status and callers, and `CreateThread` routines and returns are logged. Hooking `NtCreateThreadEx` hung the run and is disabled.

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
