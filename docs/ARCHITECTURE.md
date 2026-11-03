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
- **Context table forwarding is evidence-based.** Forwarded to the desktop context: 7 (`VSSetConstantBuffers`), 10 and 11 (`SetShader`, with `r8`/`r9` zeroed because the console takes only the shader; 23 is no longer forwarded because Sonic Mania calls it with a different shape, so it only logs), 14 and 15 (`Map`/`Unmap`), 16, 18, and the shifted indices 32, 34, 35, 42, 43, 46, 49, 52, 59, 63 (console index = desktop index - 1 from 32 on). Console-specific shapes, each wrapped in `src/gpu.rs`: 8 = `PSSetShaderResources(&view)` (view passed by address, start 0, count 1), 12 = `DrawIndexed` with `(count << 32) | first index` packed in one register and no base vertex, 18 = `IASetVertexBuffers`, 19 = `IASetIndexBuffer(format enum, buffer, offset)`, 43 = `RSSetViewports`. `XB1_LOG_ALL=1` turns the forwarding off (except slots 0 to 3, 14, 15) so every index logs its raw arguments. Every other index logs and returns; forwarding unverified ones corrupted the NVIDIA driver thread. Device slots 41 and 42 (`SetExceptionMode` on the desktop) are logging stubs; a console `DYNAMIC` constant buffer is created as `DEFAULT`. Only context offsets from `0x3E0` are function-pointer stubs; lower offsets are data fields. See [FINDINGS.md](FINDINGS.md#device-creation-methods-decoded-and-implemented).
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
| `??2@YAPEAX_K@Z`, `??_U@YAPEAX_K@Z`, `malloc` (`MSVCR110`) | Return zeroed blocks (see Heap) |
| `D3D11CreateDevice` (`d3d11_x`), `GetActivationFactoryByPCWSTR` (`vccorlib110`) | Same handling as the `vccorlib140` / `D3D11XCreateDeviceX` versions; `vccorlib110` also marks an older-XDK title |

### Older-XDK titles (Minecraft; `vccorlib110`)
A title that imports `vccorlib110.dll` / `MSVCR110.dll` is built with an older XDK; `override_for` sets the `VCCORLIB110` flag when it sees the import and the layer switches the behaviors below. Everything here comes from call sites and logged arguments of Minecraft Xbox One Edition 1.2.0.0, not from documentation ([FINDINGS.md](FINDINGS.md) has the evidence).
- **DXGI and D3D11 objects have a different layout.** Objects handed to the title are wrapped (`wrap_dxgi(real, kind)`: 0 DXGI object, 1 DXGI device, 2 swap chain, 3 resource, 4 view) with one vtable of forward thunks per kind and one wrapper per real object (hash maps `WRAPPERS` and `WRAPPER_OF`); binding calls turn a wrapper back into the real object (`unwrap_object`). The shifts: DXGI console slot n >= 7 is desktop n - 1 (`GetParent` 7, device `GetAdapter` 8, swap chain `Present` 9 and `GetBuffer` 10); resources: console 12 is `GetDesc` (desktop 10), console 8 to 10 are desktop 7 to 9; views: slot >= 8 is n - 1 (`GetResource` is 8). Views created through the device get their own vtable shifted in place (`shift_view_vtable`); the device's `GetImmediateContext` is slot 41.
- **The embedded context table maps non-monotonically.** Console index to desktop slot: 13 `Map` (the console shape `Map(res, sub, type, flags, void** data)` returns the data pointer, `ctx_map_old`), 14 `Unmap`, 31 `OMSetRenderTargets`, 87 `OMGetRenderTargets` (results wrapped), 42 `RSSetViewports`, 43 scissor rectangles, 33 / 34 / 41 blend, depth-stencil and rasterizer state, 46 `UpdateSubresource`, 51 `ClearDepthStencilView`, 17 `IASetVertexBuffers`, 18 `IASetIndexBuffer(format, buffer, **byte offset**)` (the offset matters: one index buffer holds many meshes; format 0 is 16-bit), 6 / 15 constant buffers, 7 / 9 pixel-shader resources and samplers, 23 / 24 the vertex-shader pair, 11 a packed `DrawIndexed` (`count << 32 | start`, sets a triangle list), 12 a plain `Draw` (4 vertices are drawn as a triangle strip, others as a list: guessed, the title's topology is not read). Index 104 (`+0x740`) takes only `this` and sets dirty bits; it only logs. The inlined state cache (`+0x178`, `+0x180`, `+0x188`) works as for Sonic Mania.
- **Extended vtables.** Objects the title calls console-only methods on get a copy of their vtable with logging stubs after the real entries (`extend_object`, `extend_resource`): states created through device slots 20 to 23 (slot 8 is `GetDesc`), resources (slot 12 is `GetDesc`, slot 13 on 2D textures returns width and height). The real and extended vtables are kept in hash maps (`ExtendedVtables`); an object that already carries an extension is never extended again (the runtime returns the same object for an identical state description, and layering extensions leaked about nine vtables per frame, which made the game slower the longer it ran). A bound state gets its real vtable back for the desktop call and its extension afterwards.
- **Draw cost.** A world frame is about 3,200 draw calls, each through the proxy; the per-call lookups are hash maps and the environment checks are cached, which is what keeps it at 60 FPS in a release build.

### Input (`src/winrt.rs`, `src/dualsense.rs`)
`fill_gamepad_reading` fills the console's 40-byte `GamepadReading` (buttons, two triggers, four stick axes) from the first XInput pad and, when there is none, the legacy joystick API (winmm) and a DualSense read straight through HID (`src/dualsense.rs`: USB report 0x01, Bluetooth reports 0x01 and 0x31; `XB1_DS_LOG=1` prints stick changes); the keyboard is added while the layer's window has the focus (arrows, Z/X/A/S, Enter, Backspace, Q/W, I/K/J/L left stick, numpad 8/5/4/6 right stick, R/E triggers). The older SDK needs two objects: `IGamepad` (`082E402C`, `GetCurrentReading` on slot 8) and `IController` (`9776FF56`, id, type string `Windows.Xbox.Input.Gamepad`, user); `generic_qi` routes the IIDs, and a thread (`raise_input_events`) raises the registered controller events.

### Asynchronous operations and the stand-in services
Console WinRT operations are `IAsyncOperation` objects the title hooks with `put_Completed(handler)` (slot 6, one argument) and reads with slot 8. The generic stand-in cannot tell that from an event subscription (`add_Event(handler, &token)`), so classes known to be operations (`SystemUI#slot10` / `#slot11`, Xbox Live operations, everything under `Windows.Xbox.Storage.` whose slot 6 has a layer-owned or NULL second register) complete: the handler is invoked with `Completed` on a short-lived thread 5 ms after `put_Completed` returns (`complete_later`; a console runs it later on another thread, and calling it inline let other threads read half-built state; `XB1_ASYNC_DELAY_MS`, `XB1_ASYNC_INLINE=1`). Result shapes found so far: user `IsGuest` / `IsSignedIn` booleans (slots 10 / 11), the `GetForUser` storage factory, `(name, out)` and `(object, 0, out)` creators, an operation whose out pointer is the fifth argument (on the stack), empty collections (`get_Size` 0) for storage queries, Xbox Live results and the chat session, a one-item collection for the user's display information whose four string getters return HSTRINGs, the system keyboard (returns the default world name), and `ConnectedStorageSpace` objects that keep **no data**. Nothing the title writes is stored and every read finds nothing, which is why the per-title patch below exists. `XB1_STORAGE_TRACE=1` logs every storage stand-in call with its call site, `XB1_NW_TRACE=1` the calls that wrote nothing.

### Heap
The console hands out zeroed memory and Minecraft relies on it (a settings block was read before it was filled: random controller mappings, crashes on the first button press). The `operator new`, `operator new[]` and `malloc` of `MSVCR110` are overridden (`xb_operator_new`, `xb_operator_new_array`, `xb_malloc`) to call the real function and zero the block.

### Per-title compatibility patches (`main.rs`)
Two byte-guarded patches (they apply only if the original bytes at a fixed RVA match; `XB1_NO_COMPAT=1` turns them off; `XB1_PATCH="0xRVA=hex;..."` writes bytes for experiments): `patch_filesys_storage` (a Forza Horizon 3 `CFileSys` object that nothing provides) and `patch_profile_copy` (Minecraft: the storage manager copies the loaded profile over the default options at `title+0x5a8b4a`; the stand-in storage returns a blank record, so the copy is skipped when option bytes 1 to 6 are all zero). These are workarounds for missing services, not general behavior; a real connected-storage backend would replace the second.

### Profiler
`XB1_PROFILE=1` (`src/profile.rs`): a thread suspends the presenting thread every 2 ms, reads its instruction pointer and counts the module it is in; every 5 s it prints the shares and the busiest addresses (title RVAs, and layer offsets that can be looked up in a linker map: `cargo rustc --release -- -C link-arg=/MAP:file`). It found the leak that made Minecraft slow.

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

Environment switches (all optional; `XB1_*`):
| Variable | Effect |
|----------|--------|
| `XB1_D3D_DEBUG` | Desktop device with the debug layer |
| `XB1_FPS=n` | Frame limit (default 60, 0 = off) |
| `XB1_LOG_ALL` | Log every context call instead of forwarding (except slots 0 to 3, 14, 15) |
| `XB1_CTX_TRACE=n`, `XB1_TRACE_FRAME=n` | Trace the first n logged context calls, starting at the n-th Present |
| `XB1_DRAW_TRACE=n` | Log the first n draws (count, start, bound index buffer size, format, offset) from `XB1_TRACE_FRAME` on |
| `XB1_READBACK` | Report how many back buffer pixels are not black |
| `XB1_DEFAULT_STATES`, `XB1_NO_EXTEND`, `XB1_NO_MIRROR` | Rule out state binding, vtable extension and placement-buffer mirroring |
| `XB1_NO_DEFERRED`, `XB1_TEST_DEFERRED`, `XB1_DEFERRED_ON_HELPER` | Deferred-context experiments (creation is serialized by default) |
| `XB1_NO_GAMEPAD`, `XB1_INPUT_LOG`, `XB1_DS_LOG`, `XB1_AUTOPRESS` | Pad reading off / raw joystick changes / DualSense sticks / timed button presses |
| `XB1_NO_INPUT_EVENTS`, `XB1_NO_USER_EVENTS`, `XB1_WINDOW_EVENTS`, `XB1_WINDOW_EVENT_VALUE` | Event raising experiments (window events close Minecraft: do not use) |
| `XB1_ASYNC_DELAY_MS`, `XB1_ASYNC_INLINE` | Async completion delay (default 5) / inline completion |
| `XB1_STORAGE_TRACE`, `XB1_NW_TRACE` | Log storage stand-in calls with call sites / stand-in calls that write nothing |
| `XB1_NO_HEAP_OUT` | Leave out-parameters that are not on the stack alone |
| `XB1_NO_COMPAT`, `XB1_PATCH` | Per-title patches off / write bytes into the title |
| `XB1_PROFILE` | Sampling profiler (above) |
| `XB1_STALL_MS=n` | Hold a thread n ms at a `--watch-sites` hit |
| `XB1_PEEK="reg+off:len;reg+off>off2:len"`, `XB1_POKE="reg+off>off2=v"` | At a `--watch-sites` hit: print memory at a register plus offset (optionally through a pointer) / write a byte |
| `XB1_HWBP="reg+off"`, `XB1_HWBP_RVA=0xrva`, `XB1_HWBP_FOLLOW` | Hardware write breakpoint on a field at a watch-site hit / on a fixed title address from the start / then on the object the written pointer points at; reports the writer and its stack |
| `XB1_FILES`, `XB1_DUMP_STACK`, `XB1_BT` | Log file calls / dump the raw stack on a crash / backtrace on a crash |
| `XB1_BIG_ALLOCS`, `XB1_MAX_ALLOC_MB`, `XB1_TITLE_MEM_MB`, `XB1_LAUNCH_ARGS`, `XB1_SWAP_EFFECT`, `XB1_DUMP_CMD`, `XB1_VTABLES`, `XB1_PROBE_GUEST`, `XB1_ALL_THREADS`, `XB1_NO_COREAPP_9FF` | Older experiments (Forza Horizon 3 era), see the source |

`ntdll!NtTerminateProcess` is hooked (16-byte trampoline) so every process exit logs its status and callers, and `CreateThread` routines and returns are logged. Hooking `NtCreateThreadEx` hung the run and is disabled.

`tools\run-title.ps1 -Title <exe> -Seconds N -Ring` builds the command line, runs the title from its own folder with a timeout, and prints the exit code and stderr. The crash logger also reports MSVC C++ throws (`0xE06D7363`, first 30), which shows where a title throws.

The crash logger is a vectored exception handler that reports `title+RVA`, registers, access-violation target, memory-region state and return-address-looking stack values. It allocates nothing, because the title may have crashed inside the heap. It cannot see fast-fails (`int 0x29`), which bypass exception dispatch.

## Known limitations
- Threads not created through the title's `CreateThread` import (thread pools, fibers) do not get static TLS unless the layer installs it (its own helper threads call `ensure_tls_on_current_thread`, and must hand the loader's array back before they end or the heap is corrupted).
- The storage stand-ins hold no data, so nothing a title saves survives; the status of what runs is in the README.
- Some console behaviors are guessed from one call site (draw topology, option meanings, result shapes); every such guess is named in [FINDINGS.md](FINDINGS.md).
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
