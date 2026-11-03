# Findings

The first sections are about the Forza Horizon 3 kiosk demo (retired as a test title); the Sonic Mania sections follow, then notes on input and frame pacing.

## Forza Horizon 3 kiosk demo

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

1. `CoreApplication::Run` → `IFrameworkViewSource::CreateView` succeeds → `IFrameworkView::Initialize` is entered and runs for a long time. (Early finding: later runs do reach `SetWindow`, `Load`, `Activated` and `Run`.)
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

## The CoreWindow layout is not the desktop one
The NULL read at `title+0x156fab7` was the title calling **slot 7 of its window object** (`Platform::Agile<CoreWindow>`) expecting an interface pointer back, then calling `ProcessEvents` on it. On desktop Windows `ICoreWindow` slot 7 is `Bounds` (a 16-byte rectangle), which is why the result was NULL. A tracer on the window's vtable (`--trace-window`, which copies the vtable with logging thunks) showed the slots the title really calls: **35, 15, 23, 25, 11, 7**. On desktop those would be `RemoveKeyDown`, `SetPointerCursor`, `SetPointerCapture`, `RemoveActivated`, `SetFlowDirection` and `Bounds`, none of which fit the call patterns.

What the call patterns show instead:
- **Slot 7** returns the dispatcher: the title calls `ProcessEvents(3)` (`ProcessAllIfPresent`) on its result, over and over, once per loop iteration. This was confirmed once the window handed it a dispatcher.
- **Slots 15, 23, 25, 35** are `add_Event(handler, &token)`: each is called with a handler object and a pointer to the same stack variable. All four are odd slot numbers, so the event registrations sit at odd slots (add/remove pairs).
- **Slot 11** is a setter taking the boolean `1`.

So the console's `ICoreWindow` has a different member list from the desktop interface, and the exact layout is not known. The shell now gives the title a stand-in window (`console layout`) with these assumptions: slot 6 is Bounds (a guess), slot 7 is Dispatcher (confirmed by behavior), every other slot logs and succeeds, writing a token where the arguments look like `add_Event`. Event objects registered through it are not kept or raised yet, so the title never receives window events.

With that in place the title's main loop runs: `IFrameworkView::Run` is entered, the dispatcher is called repeatedly, and the D3D11 device call is made.

## The `ntdll+0x5ac00` fault: our TLS array
A thread-pool worker crashed inside `ntdll` with a write exactly 16 bytes below a 64 KB-aligned pointer, and no title frames. Mapping the stack frames onto `ntdll`'s exports showed `Tp*` and `Ldr*` functions, and logging the title's library loads showed it had just called `LoadLibraryExW("WindowsCodecs.dll")`. So the fault was in the Windows loader, not in title code.

**Cause (confirmed by the fault disappearing):** the per-thread TLS pointer array I installed in `TEB.ThreadLocalStoragePointer` was allocated with `VirtualAlloc`. When a DLL with static TLS is loaded, `ntdll` reallocates that array and frees the old one with `RtlFreeHeap`. Freeing a `VirtualAlloc` block that way makes the heap code read a heap header 16 bytes below it, hence the fault. The array is now allocated from the process heap. Two hypotheses were tested first and ruled out: the PEB image-base patch (`--no-peb-patch` changed nothing) and `XMemAlloc` blocks being passed to `HeapFree` (never happened; the routing for it is kept as a harmless safety net).

**Known risk of this design:** the title's TLS slot is a fixed index (100) in an array that `ntdll` can still reallocate and resize when later DLLs with static TLS load. If it rebuilds the array from its own bookkeeping, the title's entry would be lost for that thread. This has not been seen yet.

## The NULL write: console-only protection bits
The write to address 0 at `title+0x1960158` was not an accident. It is the title's own fatal-error path in its small-block pool allocator: when a page allocation fails it logs `Small block page alloc failed (itemsize %Id), GetLastError(%d)` and then deliberately writes to NULL to crash. The failing call, seen with `--trace-heap`, was `VirtualAlloc(0x800400000, 64 KB, commit, protect=0x10004)` returning NULL with error 50 (`ERROR_NOT_SUPPORTED`). `0x10004` is `PAGE_READWRITE` (`0x4`) plus a console-only bit (`0x10000`) that desktop Windows rejects, the same class of problem as the allocation-type bits seen earlier. The shim now masks the protection to `0x7FF` (`PAGE_*`, guard, no-cache, write-combine). With that, no `VirtualAlloc` fails or is relocated in a run.

## Static TLS keeps getting lost
With the allocator fixed, the next faults are title code reading its static TLS on a thread where the slot is garbage (`title+0x3366d5` and others). The crash report now shows the thread's start address, and for the failing thread it was `xb1layer.exe+0x2f760`, my own `CreateThread` trampoline. That thread did get TLS installed at startup and lost it later: when something loads a DLL with static TLS, `ntdll` allocates a new vector for every thread and copies only the entries below its own idea of the current size. The replacement vectors seen are small (about 14 to 20 populated entries), so index 100 was always lost, and index 60 (now used) survives in some runs and not in others.

So no fixed index installed by hand is reliable.

**Fix: let `ntdll` own the block.** A helper DLL with a static TLS directory gets its block allocated, grown and copied by the loader for every thread (existing, new, thread-pool, driver) through any later DLL load. The title's `_tls_index` is pointed at the index the loader assigns, and the DLL's template is filled with the title's. Getting there took several attempts, and the findings matter:

- **A hand-assembled PE was ignored.** A minimal DLL I generated byte by byte (valid by `dumpbin`, correct TLS directory, relocations, entry point, callbacks array) loaded fine but the loader never processed its TLS directory: its index variable stayed 0, no block appeared in the thread's vector, and its loader entry showed `TlsIndex=0` (a module with implicit TLS shows `0xFFFF`). Disabling ASLR, setting the alignment field, adding an entry point and a callbacks array did not change that. The missing property was never identified; a genuine DLL built by the toolchain works, so the helper is now built by `build.rs` with `rustc` from `helper/xb1tls.rs` and embedded.
- **Block layout.** In a toolchain-built DLL, the linker's `_tls_start` marker sits 8 bytes before the thread-local buffer. The title addresses its data from offset 0 of the block, so before writing the DLL out, its TLS directory's `StartAddressOfRawData` is moved forward by 8: the loader then copies the template from the buffer, and block offset 0 is the buffer.
- **Verified at runtime:** the loader assigns a fresh index (12 in the test runs; the host executable itself holds 0), the title thread's block matches the title's template byte for byte, and the title's `_tls_index` is set accordingly.

With this in place the TLS-loss faults and the intermittent `title+0x3366d5` crash are gone: six consecutive runs end at the same single fault. The old hand-built vector remains as a fallback (`--legacy-tls` forces it).

## The next walls
After the TLS and allocator fixes the title survives loading `WindowsCodecs.dll` and continues, and the end of the run changed several times:
1. **Fast-fail from `terminate()`.** The title requests `CoreApplication`'s second console-only interface, `42A6A529-3CC6-4867-B5B7-6D1FAF5325F9`, gets `E_NOINTERFACE`, C++/CX throws `InvalidCastException`, nothing catches it, and the C runtime calls `terminate()`. (The other extension interface, `9FF3799E-...`, is tolerated by the title, and serving it with the generic stand-in breaks things, so it stays unimplemented.) The factory now serves `42A6A529` with the generic stand-in. The title then calls **slot 7 with the value `1`** on it; the stand-in reports success and writes nothing.
2. ~~A NULL write at `title+0x1960158`~~ Explained above (console protection bits), fixed.
3. ~~The one remaining fault, at `title+0x17cdbe0`~~ Analysed in the next section; the proxies below moved the run past it.

## Graphics: the console context is not a plain COM object
**Correction.** I first read the call at `title+0x17cdbe0` (`call [rax+0xA08]`) as "vtable slot 321" of the device context. That was wrong. The title loads a function pointer from **offset `0xA08` inside the context object itself** (`rcx = rax = context`, `call [rax+0xA08]`), not through the object's vtable. Further call sites confirmed it: the title calls embedded function pointers at `+0x438`, `+0x450`, `+0x458`, `+0x498`, `+0x5B8`, `+0x5D8` and `+0xA08`, and also reads and writes state fields in the same object (`+0x168`, `+0x180`, `+0x390`). This is how the console's low-overhead ("fast semantics") immediate context works: the title's compiled code reaches into the context's memory, and some methods are function pointers stored there. It is not something a desktop `ID3D11DeviceContext` can imitate with a longer vtable.

**What was built (`src/gpu.rs`).**
- `D3D11XCreateDeviceX` returns **proxy** device and context objects around the real desktop ones.
- The standard vtable slots (`ID3D11Device` 3 to 42, `ID3D11DeviceContext` 3 to 114) forward to the real object through a tail-call thunk (swap `this`, jump), which preserves every argument register and stack argument. Verified with `--test-d3d` through the normal D3D11 bindings: `GetFeatureLevel` returns `0xb100`, a real constant buffer is created through the proxy, `ClearState`/`Flush` work, and `GetImmediateContext`/`GetDevice` return the proxies.
- `QueryInterface` on a proxy returns the real object for standard non-D3D11 interfaces and the proxy itself for interfaces the real object does not know, which the title then treats as the console interface. The title asks for three such IIDs: `88671610-712E-4F1E-84AB-01B5948BD373` (device) and `9458FE06-C78D-47F7-96A0-EC7B727BE1E9` and `48800095-7134-4BE7-9186-B86BECB26477` (context).
- The context proxy is a 16 KB object. Its embedded region (offset `0x18` upward) is filled with logging stubs that return 0, so calls to embedded functions are visible by offset. The device proxy's vtable beyond slot 42 also holds logging stubs.

**What the title does with them.** Device extension slots 59 and 64 are called twice each. Embedded context functions at `+0x450`, `+0x458`, `+0x498`, `+0x5b8`, `+0x5d8` and `+0xa08` are called. The run then faults again, now at `title+0x17741b0` and `title+0x17b75fb`, after the title uses values the stubs cannot supply.

**What is established:** the title's graphics layer calls into embedded function pointers and state fields of the console context, and into console-only device methods. **What is not:** what any of those functions do. A reasonable expectation, not verified, is that the real ones write GPU command packets for the console's AMD GPU into command buffers; if so, a working layer has to either interpret those packets or reimplement each function at a high level, and the second requires knowing what each one does. This is where the project's scope grows sharply.

## Decoding the embedded context functions
The logging stubs now record `this`, three arguments (with what each pointer points at), the call site in the title, and a per-stub call count. That gave enough to identify part of the table:

**The embedded region contains a copy of the desktop `ID3D11DeviceContext` method table.** Evidence from the logged calls:
- `+0x450` and `+0x458` are called back to back with a real D3D11 resource (its vtable pointer is inside `d3d11.dll`, so it was created by the real device through the forwarded `CreateBuffer`), subresource `0`, and `4`. `4` is `D3D11_MAP_WRITE_DISCARD`. That is `Map` and `Unmap`, desktop slots 14 and 15.
- `+0x438` and `+0x498` are called with a NULL first argument in a "reset" sequence, matching `VSSetShader` and `GSSetShader` (slots 11 and 23).
- All four agree with the table starting at object offset **`0x3E0`**: slot *n* is at `0x3E0 + 8n`.
- `+0x5B8` and `+0x5D8` are called in the same NULL-argument sequence but sit one slot before the desktop `HSSetShader` (60) and `DSSetShader` (64). So the console table seems to lack one method somewhere between slots 24 and 59. Which one is unknown.

So the proxy now forwards embedded slots 0 to 23 to the real desktop context (with `QueryInterface`, `AddRef`, `Release` and `GetDevice` handled by the proxy), plus the two shifted shader slots. With that, the title's `Map`/`Unmap` and shader-unbind calls reach real D3D11 and no longer appear in the stub log. Everything else in the table is still a logging stub. Not forwarded yet because the mapping is unknown: the rest of the table above slot 23 and below the shift, and anything beyond slot 64.

**Device extension slots** (all return 0 for now, so the title gets no objects):
- **Slot 59** `(device, desc*, out*)`, called twice with descriptors `{1, 0x10000}` and `{2, 0x10000}` and consecutive 8-byte outputs in the title's own memory pool (`0x60028710170`, `0x60028710178`). It creates two objects.
- **Slot 64** `(device, desc*, 0xe or similar, 0)`. In one run the descriptor starts with `0x43800000780`, which is the dwords `0x780` and `0x438`: **1920 and 1080**. So this is a display-sized object, probably a swap chain or display plane. In earlier runs the first qword looked like a pointer instead, so the descriptor format varies.
- **Slot 65** `(device, desc*, 0xd, 0)` with a descriptor starting `{16, 16}`.

**Context `+0xA08(this, 1)`** is called once per context (the immediate context and one deferred context). Meaning not known.

## Device creation methods, decoded and implemented
Reading the call sites in the title's code (the arguments in registers, what it does with the result) pinned down the device extension slots:

| Slot | Signature | Implemented as |
|------|-----------|----------------|
| 59 | `(device, const {u32 kind, u32 size}* desc, Object** out)`, kind 1 and 2, size `0x10000` each. The title later calls vtable slot 2 (`Release`) on the old value of the output field, so the result is a normal COM object. | A logging stand-in COM object; real type unknown |
| 64 | `(device, desc*, tileMode, pitch, placementMemory*, Texture** out)`, the shape of the console SDK's `CreatePlacementTexture2D` | Ordinary desktop `CreateTexture2D` from the same description |
| 65 | the same shape; description is 16x16x16 | Ordinary desktop `CreateTexture3D` |

The descriptions begin with the desktop `D3D11_TEXTURE2D_DESC` / `D3D11_TEXTURE3D_DESC` layout: for example 960x540, 10 mips, format 99 (BC7), shader-resource bind; a 1920x1080 MSAA depth-stencil; 16x16x16 RGBA8. Console-only fields follow the desktop ones and are not copied. Tile mode (the `0xd`/`0xe` values) and the placement memory pointer have no desktop equivalent and are ignored; the texture is just allocated by the driver. Console-only bits in `MiscFlags` (seen: `0x400`, `0x40000`) make desktop creation fail with `E_INVALIDARG`, so only the desktop-valid bits are kept. With that, all 17 texture creations in a run succeed. The title also calls `GetType`, `GetDesc` and `GetDevice` (resource vtable slots 7, 10, 3) on the textures, so they are used as normal textures.

## Context table: the shift is exact from index 34
More logged calls fixed the mapping of the embedded context table:
- `+0x4f0` (console index 34) takes a state object, NULL and `0xFFFFFFFF`: `OMSetBlendState(state, factor, sampleMask)`, desktop slot **35**.
- `+0x550` (46) takes two real resources of the same type: `CopyResource`, desktop slot **47**.
- `+0x568` (49) takes a render-target view and a pointer to zeros: `ClearRenderTargetView`, desktop slot **50**.
- Indexes 59 and 63 are the hull/domain `SetShader` unbinds, desktop 60 and 64.

So from console index 34 on, the console table is exactly one slot behind the desktop one, while indexes 11, 14, 15 and 23 match the desktop numbers. The method the console lacks is therefore one of desktop slots 24 to 34 (candidates: `IASetPrimitiveTopology`, `Begin`, `End`, `GetData`, `SetPredication`, `GSSetShaderResources`, ...); which one is not known. The proxy now forwards console indexes 0 to 23 as-is and 34 to 113 to desktop index + 1 (only indexes up to 63 have direct evidence; the rest is extrapolation, for example 109 would be `ClearState`). Indexes 24 to 33 stay logging stubs. With this, essentially all the context calls the title makes reach the real context; the remaining stub hits are `+0x4e0` (index 32) and `+0xA08`.

## Slots 62 and 73
**Correction:** I first read device slot 62 as shader creation from code blobs. Dumping its arguments showed it is not. Its descriptor is a `D3D11_BUFFER_DESC`: `{ByteWidth = 0x490, Usage = 0, BindFlags = 4, CPUAccessFlags = 0, MiscFlags = 0, StructureByteStride = 0}`, i.e. a **1,168-byte constant buffer** (`D3D11_BIND_CONSTANT_BUFFER`). The call has the shape of the console SDK's `CreatePlacementBuffer(desc*, placementMemory*, Buffer** out)`: the second argument is the memory the buffer is placed in (`0x80dba0100`, `0x80dba0800`, ..., 0x700 bytes apart, inside the title's reserved GPU-visible range, all zeros at creation) and the third is a stack slot for the output. It is implemented as an ordinary desktop `CreateBuffer` (misc flags masked to the desktop-valid ones); 15 such buffers are created per run.

**Placement memory mirroring (buffers: done).** A title writes its constants straight into the placement memory, which is how the console GPU reads them. Each placement-created buffer is now registered with its placement address, size and a shadow copy. Before every draw, dispatch and copy the title issues through the context's embedded table, a hook compares the memory with the shadow and uploads only what changed with `UpdateSubresource`, on the context making the call (so deferred contexts work). The hook is a thunk in front of the forwarding thunk, so every argument still reaches the real call. Verified with `--test-d3d`: a buffer is created in a memory block, bytes are written into the block directly, a copy is issued through the embedded `CopyResource` and the readback matches. The registry keeps a reference to every placed buffer for the life of the process, so it never sees a release.

**Textures (not done).** The same problem applies to textures placed with slots 64 and 65, with a hard part: the title writes pixel data into placement memory in the console GPU's tiled layout (the tile-mode values `0xd`, `0xe`, `0x0` seen in the calls select it), so mirroring needs the tiling modes decoded and the data de-tiled into the desktop texture's layout. Nothing like that exists yet, and until it does, any texture the title fills by writing into its memory will be blank or wrong on screen.

**Test note:** a bare `Draw` with no pipeline bound removes the device on this GPU (`DXGI_ERROR_DEVICE_REMOVED` at the next `Map`), independent of anything in this layer. The self-test therefore triggers the mirror with `CopyResource` instead.

**Slot 73** is only partly decoded. It takes two ~64-byte structures plus a NULL fourth argument and is called three times. The values `5`, `8` and `0xb` in them match the mip counts of three of the textures created just before (the 16x16x16 volume has 5 mips, the 1024x1024 textures have 11), so it is most likely a texture-related call such as creating a view of a texture. Its signature is not known, so it is still a dumping stub that returns 0.

**Not shaders (yet):** no shader creation has been seen. Shaders may appear later, and if they are in the console GPU's binary format they would need translating; that is still an open question, but this data does not show it.

## The NULL read, narrowed
The NULL read at `title+0xe6319c` is in a small helper (`GetX(out)`: `*out = NULL`, then `QueryInterface` on the pointer stored at offset `0x50` of `this`) that was called on a NULL `this` (`rcx = 0x50`). The call comes from the title's generic render-resource code (it switches on a kind byte at `+0x34`/`+0x35` of a resource wrapper and reads a global render-system object), so the NULL object is a resource wrapper whose underlying COM object was never created or stored. The helper queries the IID **`DFDBA067-0B8D-4865-875B-D7B4516CC164`**, which is not a standard DXGI/D3D11 IID I could identify, so it is probably a console-only interface. Which object is NULL is not known.
## Two bugs found along the way (mine, not the title's)
- The console CoreWindow stand-in treated vtable slot 3 as `IInspectable::GetIids` and wrote through its arguments. The title calls slot 3 with non-pointers, a misaligned write panicked in the debug build, and the abort showed up as exit code `0xC0000409` (identical to a real fast-fail). The console window now has its own vtable where slots 3 to 5 are logging slots. Some earlier "fast-fail" endings may have been this.
- A tool for real fast-fails: `--patch-fastfail` replaces the compiler's `mov ecx, code; int 0x29` sequences with a breakpoint, so the crash logger reports the site and the code. Only one such site exists in the image, and it was not reached in the runs checked.

## The NULL object, found
The NULL read was the title using the result of a failed resource load. Following it back:
1. The crash was in a helper called with a NULL `this`. Its caller is a lock-and-acquire function (`0x140389F00`) that returns a pointer to a member of a resource wrapper only if the wrapper's state check passes. The wrapper keeps two state bytes at `+0x34`/`+0x35`; `4` means ready and `6` means failed. The caller ignores the failure and uses the NULL.
2. Only two places in the whole title store `6` into that byte. A one-shot watchpoint on them fired, and the code just before it logs an error through the title's own logger at `0x1416DDB90` with a file, a line and a message.
3. A repeating watchpoint on that logger (`--watch-sites=0x16DDB90 --watch-repeat`) printed the title's own messages: `ResourceManager.cpp:835: Failed to load Create(worldrender) (Texture Resources)`, and the same for **`worlddepthdsv`**, **`finaltemp`** and **`velocity`**. These are the main render targets and depth buffer.
4. Logging the device's standard creation calls (they are forwarded to desktop D3D11, and used to fail silently) showed why: `CreateTexture2D` and `CreateBuffer` failed with `E_INVALIDARG` because the console descriptions carry console-only `MiscFlags` bits (`0x20000` on the render targets, `0x20020` on a buffer: a valid raw-view bit plus `0x20000`). Those standard methods now mask the flags the same way the placement methods do.
5. View creation then failed on resources whose descriptions the console accepts but desktop D3D rejects: shader-resource views on multisampled (4x) render textures with a plain 2D view dimension, some descriptions that are all zeros. A fallback retries with no description, which lets D3D derive the view from the resource; those retries succeed.

After these fixes the title no longer marks those resources failed, and the NULL read is gone.

## Console methods on resources
A resource the title creates is called through methods a desktop resource does not have. First seen: `call [rax+0x68]` (vtable slot 13) on a 2D texture, which reads two dwords that the title stores as the texture size. A desktop texture's vtable has only 11 slots, so the call read past the end and crashed inside `d3d11.dll`. Each created resource now gets an extended copy of its vtable (the 11 real entries, argument-logging stubs after them, slot 13 returning width and height from the real `GetDesc`) and its object's vtable pointer is redirected to it. Desktop D3D11 accepts the patched objects (checked: a patched buffer still copies correctly and creation and view calls work). Slot 13 reported `1920x1080`.

## The title writes GPU command packets itself
The next fault was in code like this: load a cursor from `[ctx+0x50]`, compare with an end at `[ctx+0x58]`, call `[ctx+0xA68]` to get more space if full, then write a packet header `((n + 0x35A) << 5) | 0x70000` and 36 bytes of payload at the cursor and advance it. So the title does not draw through functions: **it emits packets into a command buffer that lives in the context object**, and the console GPU consumes them. This is why none of the draw and dispatch hooks ever fired.

It also showed a mistake in my earlier stub fill. Only the context object's offsets from `0x3E0` up are function pointers; the fields below that are data (cursor, end, flags such as `+0x168`, `+0x180`, `+0x390`). Filling them with stub pointers made the title write packets into my stub memory. Only offsets from `0x3E0` are stubs now, and each context proxy owns a 4 MB command buffer with the cursor and end set and a `+0xA68` function that logs the flush and resets the cursor. The packets are not interpreted yet: they are discarded on flush.

**This is the core of the remaining work.** To draw anything, the layer has to decode the title's packet stream (draw, state, resource binding, clears, copies) and turn it into desktop D3D11 calls, or the equivalent. The packet format is not documented here; it has to be worked out from the stream itself. The dump at each flush (`[gpu] command buffer flush #n`) is the starting point.

## The `Users` list
With the above, the run stayed up for 40 seconds, producing 150,000 lines. The title was polling `Windows.Xbox.System.User`: the statics' slot 9 returns the `Users` list, an `IVectorView<User>` (`GetAt` at slot 6, `get_Size` at slot 7). My generic stand-in treated slot 7's output as a pointer and wrote an object address into the size, so the title believed it had about 20,000 users and looped over them. A vector-view stand-in now returns size 1 with a generic element for each index. The loop is gone. The single user element has none of a user's real methods, and the title faults calling it (`title+0x16f244f`, `rcx` is a generic stand-in).

## Where the run ended at this point
The title ran and then faulted in the user stand-in or the GPU driver's worker thread. Both were resolved later (see the sections below).

## The user object, and a Direct3D debug layer
The `Users` element faults were all the same mistake in my stand-in: it answered every call by writing a new stand-in object through the first argument. Three shapes turned up.
- A getter that returns a **string**. The title passes the result to `Platform::String::Data()` (the fault at `title+0x16f244f` is the `wstring` constructor reading it). Slot 6 of the object returned by user slot 9 now returns an `HSTRING`, as does slot 15 of the user (parsed with base-10 `wcstoull`, so it is a decimal user ID). Both values are made up (`XB1Layer Player`, `2535400000000001`); what each slot means is inferred from use, not known.
- An **event subscription**, `add_Event(handler, &token)`. The first argument is the title's own delegate, and writing a stand-in pointer through it overwrote its vtable, which is why execution jumped to `0x1f`. `User` statics slots 10 and up are treated as add/remove pairs. More generally, when the first argument looks like a COM object (a vtable whose first entry is executable code), the call is taken as an event add and only the token is written.

With these the run no longer faults in the shell layer. The `Windows.Xbox.Input.Controller` calls hit the same pattern and are now covered by the general rule.

Setting `XB1_D3D_DEBUG=1` creates the desktop device with the **debug layer** and logs its messages (`[d3d-debug]`). It found two real signature mismatches in my forwarding:
- **`SetShader` calls.** The desktop methods take `(shader, class instances, count)`; the console ones take only the shader, so the title left garbage in the other two arguments (`NumClassInstances is out of range`, a corruption-severity message). Context `SetShader` thunks now zero `r8` and `r9`.
- **Device slot 41.** It is `SetExceptionMode(UINT)` on the desktop, but the title calls it with a pointer. Slots 41 and 42 now log and succeed instead of being forwarded. What they are on the console is not known.

It also reported many invalid shader-resource-view descriptions (view dimension not matching the resource, multisample resources viewed as non-multisample, mip ranges, typed formats that are not castable). The views are retried with no description, which works for the common case, but a view created that way may not match what the title meant. Creating the render-target view for a NULL resource crashes the debug layer itself, so that call now returns `E_INVALIDARG` with a log line.

Open: with the debug layer on, the run ends by itself with exit code `0x87D` without going through `ExitProcess` or `TerminateProcess` (both are now logged). Without it, the NVIDIA worker thread fault still happens, so the corruption is not explained yet; the `SetShader` fix removes one known cause but is not shown to be the one.

## Tracing the `0x87D` exit and the driver fault
**`Store.Product`.** One more crash came from my stand-in: `Windows.Xbox.ApplicationModel.Store.Product` statics slot 11 has the shape `(user, id, flag, ptr, ptr, Product** out)`, so its result comes back through the **sixth argument** (a stack argument), which a three-argument stand-in cannot see. It also looked like an event add (the first argument is a COM object), so the title got NULL and dereferenced it at `title+0x1948fb9`. The generic slot now accepts six arguments and fills that out-parameter. This is the third time the "first argument is the out-parameter" guess was wrong; unknown methods are still guesses.

**The `0x87D` exit (debug layer on).** Not a title exit code. The only `0x87D` literal in the title is a log-line number (`mov edx, 0x87d` before a logging call at `title+0x1e6c56e`), and a watch on it never fired. `ntdll!NtTerminateProcess` is now hooked: the call arrives straight from `RtlUserThreadStart` with status `0x87D`, right after a `0xC0000005 execute of 0x0` in a thread whose start address is the layer's thread trampoline and whose registers are all zero. So some thread jumps to NULL as it starts and the process is torn down; the status is not the title's. The trampoline logs any start routine below `0x10000` and never did, so the NULL jump is not in a routine the title passed to `CreateThread`. Not explained.

**The driver fault (debug layer off).** It still happens (`nvwgf2umx.dll`) and so does a fault in the title at `title+0x1e40b69`. Both vary between runs, which fits a race or memory corruption rather than a single bad call. Not explained.

**Hooking pitfalls.** An inline hook of an `ntdll` syscall stub must copy whole instructions: the stub is `mov r10, rcx; mov eax, n; test byte ptr [7FFE0308h], 1; ...`, so 12 bytes of patch need a 16-byte trampoline (an 8-byte one resumed mid-instruction and crashed with `0xC000001D`). Hooking `NtCreateThreadEx` and logging inside it hung the run, so that hook is disabled.

## The driver fault: unverified context forwarding
The NVIDIA worker-thread fault (`nvwgf2umx.dll`, same offset across runs) was **caused by my embedded context table**, not by resource vtables or the placement mirror (switches `XB1_NO_EXTEND` and `XB1_NO_MIRROR` showed neither removes it). I had forwarded every console index from 4 to 23 and from 34 to 113 to the desktop method of the same position, but only a handful were backed by logged calls. The debug layer's one corruption-severity message gave it away: `PSSetShaderResources` was being reached with `(0x288345b0, 0, 0x76b40)`, which is a pointer, zero and a size, not a slot range. So **console index 8 is not `PSSetShaderResources`**: the console table is not a plain copy of the desktop one. Only these are forwarded now: 7 (`VSSetConstantBuffers`: called as `(1, 1, &buffer)`), 11 and 23 (`SetShader` with NULL), 14 and 15 (`Map`/`Unmap`), and shifted 34 to 63. Everything else logs and returns, and with that the driver fault is gone in repeated runs. The cost is that unforwarded calls (a hot loop at `+0x418` before index 7 was added) do nothing, so each console index has to be identified from its calls before it is forwarded.

Also: a console `DYNAMIC` constant buffer (usage 1) failed `CreateBuffer` with `E_INVALIDARG` (needs CPU write access on the desktop, and `UpdateSubresource` is not allowed on it), so it is now created as `DEFAULT`.

## More shell-layer shapes
The title's startup keeps hitting methods whose shape my generic stand-in guessed wrong:
- `Windows.ApplicationModel.Package.Current` fails in a process without package identity, so the class is served by a stand-in.
- `PackageTransferWatcher` slot 7 and `Microsoft.Xbox.Services` factories take `(object, Object** out)`: an object first argument is not always a delegate.
- `Controller` statics slot 6 is the controller list (an `IVectorView`); it is empty for now, so no gamepad input yet.
- `XboxLiveConfiguration` slots return strings (placeholder service configuration ID).
- The weak-reference query on a stand-in reaches `GetIids` with unrelated arguments; it now only writes well-formed ones.

`DXGIXSetVLineNotification(0, 0, 0x2d4, 0)` is logged: the third argument looks like a scanline number. `DXGIXPresentArray` is now implemented as a logging stub (assumed shape `(SyncInterval, Threshold, Flags, NumPlanes, ppPlanes, pParameters)`) but has not been reached yet, so there is **no swap chain**: nothing is shown. The title still stops in the shell layer before its first present, with different faults in different runs.

## Startup gaps removed, round 2
Each of these was a stand-in or stub that gave the title NULL or a wrong-shaped value; the run now gets noticeably further (it passes the Xbox Live, package, DLC, speech and networking setup) before the next fault.
- **The telemetry library** (`commonEventLoggingLibrary_Release.dll`, absent, so all seven imports were stubs returning 0). `CellGlobalAllocator::Get()` now returns an allocator object (slot 0 allocates from the guest heap, slot 1 frees; the rest is assumed); `CreateCellInternal` and `CellGlobalAssertHandle::Get` return a null object whose every method returns itself (the title uses `ICell` slot 3's result as another object and calls through it); the three `get_Global*` tables return zeroed memory.
- **`Store.Product` slot 11**: the out-pointer is the fifth argument after `this` (I had counted the sixth).
- **`DownloadableContentPackageManager` slot 6** `(filter, IVectorView** out)`: an empty package list through the second argument.
- **`SpeechRecognizerFactory` slot 6** `(locale, Recognizer** out)`; its objects return strings and objects through the shapes found at the call sites (slot 8 string, a third-argument out on a later slot).
- **`Package.Current` -> slot 7 -> slot 7** is read as a string. I first took it for an `IPackageId`, which was wrong: an integer there crashed `combase`. It now returns the title folder as a path (assumed to be the installed location).
- **A general rule for string arguments**: when the first argument is a fast-pass `HSTRING_HEADER` (flags 1, small length, readable data), the call is `(name, Object** out)`, so the object goes to the second argument. This fixed `SecureDeviceAssociationTemplate`, whose header my stand-in had overwritten.

Still open: the run now faults at `title+0xd93e57`, a NULL `rdi` read inside the title's own code on the main-loop path (not a stand-in call). Not analysed yet.

## Loading the title's own DLLs, and the `G:` drive
**Guest DLLs.** The title folder ships console DLLs (`commonEventLoggingLibrary_release.dll`, `Microsoft.Xbox.Services.dll`, `Microsoft.Xbox.GameChat.dll`, `Microsoft.Xbox.Input.Wheel.dll`, a small `psapi.dll`). Windows cannot load them natively (they import console-only DLLs), so the loader reported them as missing and I had written fakes. The loader now maps them itself: sections, relocations, imports (through the same resolver as the title), an export table for `GetProcAddress`/import lookup (forwarders are not followed), unwind tables registered and `DllMain(DLL_PROCESS_ATTACH)` called on the title thread after bootstrap. With the real telemetry library loaded, all 196 of its imports resolve natively and my fake `CELL` allocator and null objects are gone. Only the telemetry DLL is loaded so far: the others are not imported by name from the title's import table (`Microsoft.Xbox.Services.dll` is reached through WinRT activation, which still goes to the stand-ins).

**Static TLS in a guest DLL.** The telemetry DLL's `CellGlobalAllocator::Get()` is a function-local static with thread-safe initialisation, which reads the DLL's own static TLS (`_tls_index`). Without a TLS block its construction was silently skipped and the allocator had a NULL vtable. A guest DLL with a TLS directory now gets its own copy of the helper DLL (a different file, so a different loader-assigned index) and its `_tls_index` is pointed at it. Each copy is limited to 4 KB of TLS.

**`G:`.** The title reads its content from `g:\media\...` (a console drive letter). The layer maps `G:` to the title folder for the session with `DefineDosDevice` (`DDD_REMOVE_DEFINITION` is `0x2`; I first used `0x1`, which is `DDD_RAW_TARGET_PATH`, so the mapping was not removed), removes it on exit including crashes (through the `NtTerminateProcess` hook), refuses to touch a `G:` that points elsewhere, and reuses one left behind for the same folder. With it the title opens `hash.manifest`, `zipmanifest.xml`, about 200 zips and `unlockrules.xml`; the content is plain zips and loose files, not encrypted. `XB1_FILES=1` logs the file calls to stderr.

**Where it stops.** The title then queries its game database (`SELECT IsCarInstalled(ID) FROM Data_Car ...`) through a global database manager and gets a NULL statement (`title+0xd93e57`). `media\stripped\gamedbRC.slt` (a 6.9 MB SQLite file) is never opened by any logged file call, so the database is probably opened later or on a worker thread (28 job threads are created suspended and the title resumes them) than the query that crashes. Not yet understood.

## Chasing the database
What was established (from the title's code and watch-site runs; names are mine):
- The title names its databases `game:\media\db\gamedb.slt` and `game:\media\Stripped\gamedbRC.slt` (two static strings; the second exists, the first does not). `gamedbRC.slt` is a plain SQLite 3 file (page size 1024). Only one function (`0x142e2c0`, a `CForza` method taking a bool) opens it; with the flag false it opens nothing, with true it picks one of the two paths and stores the connection at `CForza+0x40`.
- `CForza`'s vtable (RTTI `.?AVCForza@@`) slot 7, `0x142c190`, is the big start-up method (it allocates the subsystems, calls `Open(false)` at `0x142cf72`, and later builds the car table, which queries the database). The title's `IFrameworkView` methods are thunks at `0x15873dc..0x151751c`; the `Activated` handler is at `0x15875d0`. The `Open(true)` call is inside the launch-argument handler (strings `restart` and `ms-xbl-%X://`, `0x1431a80`), which `0x142c190` only calls when two flags on the title's `CPresentation` object are set (`[obj+0x24de]` is 1, `[obj+0x3224]` is **0**; the second is written by `CPresentation`'s virtual method 119). I did not find what sets it; a presentation or display-ready flag is a guess.
- With `XB1_POKE` forcing that flag, and also in runs where the timing allows, the title reads the whole 6.9 MB file with one `ReadFile` (it is loaded into memory), so the file access path works.
- The crash itself is not the database being closed: the query `SELECT IsCarInstalled(ID) FROM Data_Car WHERE ID=@id` is prepared through `0x159c820`, and the statement object it returns has a NULL handle at `+0x88` (read by `0x3a70d0`). That points to the prepare failing: `IsCarInstalled` is a user-defined SQL function, so the connection it runs on probably needs it registered first, which did not happen. Not confirmed.

New diagnostics: `XB1_PEEK="r13+0x24de:1;r15+0x230:2"` prints memory at register+offset on each watch-site hit, `XB1_POKE="r13+0x3224=1"` writes one byte there (experiments only), and the shell logs the title's own `IFrameworkView` and `Activated` handler addresses.

## Where `IsCarInstalled` is registered
- `IsCarInstalled` and `IsCarInstalledAndPurchased` are registered by `0x2be900`, called from the same `CForza` method that opens the database (`0x142e2c0`, after its `Open` branch) with the connection object. It does run before the query (watch site), and the connection object it receives is non-NULL and is the same one the later query uses. So registration is not simply skipped.
- The database file is read by a generic file-open routine (`0x18dd2c0`, a `CFile`-style class) earlier in start-up, before `Open(false)`; the call chain beyond that frame is not recoverable with the current unwinder. `gamedbRC.slt` itself is valid: Python's `sqlite3` opens it and finds `Data_Car` among 276 tables and views.
- `Prepare` (connection vtable slot 16, `0x16fd020`) only allocates a statement wrapper (0x60 bytes) around the SQL text, so the NULL read at `title+0xd93e57` is not sqlite refusing the statement. The NULL is a pointer at offset `0x88` of a longer-lived object that `0x159c820` hands back; I did not establish what sets it.
- The `Open(true)` branch (the one that loads and registers) is only reached from the launch-argument handler when its two checks pass: the `[object+0x3224]` flag, and `[this+0x230]`, which that same function sets from whether it parsed a non-empty list (`r12`). The list looks like the tokens of the activation arguments (`restart`, `ms-xbl-%X://`); the shell passes empty `Arguments`. A first launch from the console dashboard probably carries something there. Untested: giving `LaunchArgs.Arguments` a plausible value.
- Nothing in the code writes `[object+0x3224]` except constructors and a virtual setter (`CPresentation` vtable slot 119) that is never called before the crash; it may be set by a window or presentation event the shell does not raise (visibility, activation, size).

New: `.slt` opens log their call chain (with `XB1_FILES=1`).

## Launch arguments: not the cause
Hypothesis tested and **refuted**: that the `Open(true)` branch needs non-empty activation arguments. `XB1_LAUNCH_ARGS` sets the `Arguments` the shell reports; with an empty value, `restart`, `ms-xbl-3BC8D4D5://` (the title ID from `AppxManifest.xml`, `3BC8D4D5`) and `ms-xbl-3BC8D4D5://restart` the run is identical and crashes at the same site. Forcing both gate flags by hand at the check (`XB1_POKE="r13+0x3224=1;r15+0x230=1"`) does reach the launch-argument handler (`0x1431a80`) but still never reaches the `Open(true)` call (`0x1432191`), because that call also depends on a changed-list result computed inside the handler (`r13b`). So the database path is gated by more state than a single flag, and I have not found what the real console run provides. The service configuration ID reported by `XboxLiveConfiguration` is now the real one from the manifest (`1d220100-a389-45d9-ab7a-fd1a3bc8d4d5`) instead of a zero placeholder.

## The command stream is PM4, and it holds no draws yet
`XB1_DUMP_CMD=<dir>` writes the first three flushed 4 MB command buffers to files, and `tools/analyze-cmdbuf.py` parses them. Read as **AMD PM4** (type in bits 31:30, type 0 = register writes with the count in bits 29:16 and the first register in bits 15:0), all three dumps parse exactly to the end with no desync, which supports the reading: the console's D3D11X writes hardware command packets inline. This replaces my earlier header formula (`((n+0x35A)<<5)|0x70000` is just the type-0 header for base register `0x6b40`).

What the stream contains: exactly two packet kinds, repeating in a fixed cycle of five `0x6b40` packets (8 registers: `0,0,0,0,1,<0x12a or 0x80000b00 or 0x80000100>,2,0`) and one `0x7b40` packet (4 registers: a 64-bit address, then `0,0`). The address is `d3d11.dll+0x1d13e8` in the host process, i.e. a pointer into the desktop runtime's image (probably an object or vtable value read from a field of my context proxy that the title expects to hold something else). About 75,000 cycles fit in one 4 MB buffer, matching the title's main loop spinning on `CoreWindow` slot 7 with no work to do.

So there are **no draws, state binds, clears or copies in the stream**: the title has not started rendering, because it is stuck before that (the database gate above). Decoding render packets into desktop D3D11 calls therefore cannot be done yet; there is nothing to decode. The reverse is not needed for the register meanings either: the `0x6b40`/`0x7b40` register numbers are not in the standard GCN ranges I know, so their meaning is not established. Next: get the title past initialisation, then dump again.

## Booting into the main loop (first stable run)
With the changes below the test title **starts, initialises and settles into its main loop without crashing** (40 s runs, three in a row, about 27,000 log lines, ended only by the harness timeout). It still does not draw: the command stream is the same two idle packet kinds, and `DXGIXPresentArray` is never called.

What it took, in the order the title hit it:
- **`CFileSys+0x88`** (the object the crash at `title+0xd93e57` and its neighbours dereferenced). A hardware write breakpoint (`XB1_HWBP`, armed on every thread, and on threads created later) showed that nothing writes that field after the constructor, so this is not a race. The field holds an object the layer cannot provide; its callers use it to open `...\Manifest.xml`. The constructor's two zeroing stores (`title+0x158c84d`) are now patched (only if the original bytes match; `XB1_NO_COMPAT=1` disables it) to store a **null object**: a COM-shaped object whose methods all succeed and write nothing, except slot 39 (`Open(out, ...)`), which must write NULL into `out` because the caller does not initialise it. Callers then take their "not found" paths. This is a per-title compatibility hook, not a general solution.
- **`AcpHal.dll`** (the console audio processor HAL, a missing DLL whose imports were stubs returning 0): `ApuAlloc` allocates zeroed memory, `ApuCreateHeap`, `ApuFree`, `ApuIsVirtualAddressValid`, `ApuMapVirtualAddress` succeed, `AcpHalCreate(out)` returns a success-everywhere HAL object through its first argument (the title calls slot 0 on it as a method), and `AcpHalAllocateShapeContexts(config, 0x1c, 0x40, 0x40, ...)` fills pointer fields at `config+0x18`, `+0x30` and `+0x38` with zeroed blocks (the title then clears count*32, count*16 and count*64 bytes there). All signatures are inferred from call sites and logged (`[acp]`).
- **The display chain.** The embedded context function at `+0x6a0` (console index 88) is `(context, 1, Object** chain, Object** other)`: the title calls slot 9 of `chain` with an out-pointer to get a texture and slot 13 of that texture for its size, so `chain` is the display swap chain and slot 9 returns the back buffer. A 1920x1080 `R8G8B8A8_UNORM` render-target texture is now created on the desktop device behind a chain object whose slot 9 returns it. This is the first real display plane; the title's `CreateRenderTargetView` for it no longer sees NULL.
- **Created-object vtables.** The title calls console-only methods on shaders, states, input layouts and queries. Their creators (device slots 11 to 13, 15 to 18, 20 to 26) are wrapped and the objects get an extended vtable (their real slots, then logging stubs); one such call went through a forwarding thunk past the end of a real vtable and jumped to address 1.
- **Device extension slot 67**, `(device, desc, Object** out)`, same shape as slot 73: a stand-in object is returned.
- **The stand-in object vtable** has 300 slots instead of 128.
- **Diagnostics fixed:** the crash logger read past the end of a small TLS array and crashed while reporting, which showed up as an intermittent early crash in the layer.

Where it is now: alive in the main loop (the title polls `CoreWindow` slot 7 constantly), not drawing, never presenting. The most likely blockers are the same ones already identified: the `CPresentation` readiness flag and the window events the shell never raises (visibility, activation, size), plus what the title waits on in its loading screens.

New tools: `XB1_PATCH="0xRVA=hex;..."` (byte patches into the mapped title), `XB1_HWBP="rbx+0x88"` with a watch site (hardware write breakpoint on every thread), `XB1_PEEK` two-level reads (`reg+off>off2:len`), `XB1_VTABLES=1`, `XB1_NO_COMPAT=1`, and a log of every WinRT class the title activates that is not served by a stand-in.

## What the booted title is doing
`--watch=N` with `XB1_ALL_THREADS=1` samples every thread (instruction pointer and the title return addresses on each stack). With the title in its main loop: the thread that ran `IFrameworkView::Run` sits in a `Sleep(1)` loop (`title+0x156f9e0`) that drains a queue and calls `CoreWindow` slot 7 (`ProcessEvents`); about 28 job workers (`title+0x16ebb00`) are parked waiting for jobs; one thread is inside the audio start-up (`title+0x23f6xxx`, the code that calls the `AcpHal` functions) blocked on a wait; and one thread (`title+0x2e40cc0`, `0x17362d0`, `0x179178b`) is the only one not obviously parked. So the title is not hung in one place: it is idle because the work that would start rendering has not been triggered. What triggers it is not known. Candidates, none tested: a signal from the audio engine (the `AcpHal` emulation never raises interrupts or completes work), the window events the shell never raises (visibility, activation, size), a signed-in-user event (the `User` statics' event slots hand out tokens but the handlers are never called), or package/content events.

## Window events: raised, no effect
The shell now keeps every delegate the title registers on the console window (event-add calls on slots 15, 23, 25 and 35; four handlers) and, with `XB1_WINDOW_EVENTS=1`, invokes each as `Invoke(sender = the window, args)` before `Run`. The args object answers every getter with `XB1_WINDOW_EVENT_VALUE` (default 1; the first getter slot is read as a `bool Visible` or an enum, which one is unknown). All four handlers return `S_OK` and nothing crashes with values 0, 1 and 2. Values 1 and 2 change how the title idles (`ProcessEvents` is called about 2,600 times in 40 s instead of about 27,000 lines of spam), but the run is otherwise the same: no present, and the command stream is still only the two idle packet kinds. So the window events alone do not start rendering. Which of the four slots is which event (visibility, activation, size, close) is still unknown. Next candidates: the `User` events, and signals from the audio engine.

## User events: the title moves on to loading
The shell keeps the delegates the title registers on the `User` statics (event-add slots 10 and 12, registered during `Run`, so after the usual raise point) and, with `XB1_USER_EVENTS=1`, a helper thread raises each one about a second after it appears: `Invoke(sender = a user element, args)`, where `args` returns the signed-in user element from its first getter (a plain stand-in first made the title parse an object as a decimal user ID string and crash in `wcstoull`). **This changes what the title does.** After the events it creates well over a thousand more textures through the console texture-creation slot (1,238 in a 100 s run, against about 20 before), i.e. it starts its resource loading, stays up with no crash for the whole run, and still does not present (`DXGIXPresentArray` is never called) and still emits only the idle command packets in the first three flushed buffers (the dump covers only the earliest flushes). So the sign-in event is one of the missing triggers. Window events were raised too (previous section) and are independent. Still unknown: what the loading waits on next.

## After loading: out of memory
With the user events on by default (`XB1_NO_USER_EVENTS=1` disables them) the title loads about 1,000 to 1,240 textures, then stops writing GPU commands (one context only; its buffer stops growing after about 15 s) and spins retrying `CreateShaderResourceView` on a 32x32 cube map (6 faces, 6 mips, `R11G11B10_FLOAT`). **Cause found: `E_OUTOFMEMORY`, not a bad description.** Every candidate view description fails with `0x8007000E` while `GetDeviceRemovedReason` is 0, and the process holds about 13 GB of private memory. A scan of the running process's address space (`VirtualQueryEx`) shows one committed region of 8,394 MiB, which comes from a single `HeapAlloc(size = 8,802,732,911)` made by the title's own allocator from an asset-loading thread (`title+0x195e239`, call chain `0x16e3fd0`, `0x1b1576d`, `0x17376b8`, `0x1734c6d`, `0x17362d0`, `0x179b6a5`); the same thread was the one thread not parked in the all-thread samples, busy touching it (the working set follows the commit). On this machine the system commit limit is 34.9 GB with about 26.5 GB already used by other programs, so that one block pushes the commit charge over the limit and later allocations (including the driver's, behind the cube-map view) fail. The size does not depend on what `TitleMemoryStatus` reports (tested with 1, 2, 5 and 8 GiB) and is not a multiple of anything (`0x20caf0b6f`), so what computes it is unknown (a file-size or count value is the first guess). Refusing it (`XB1_MAX_ALLOC_MB`, off by default) makes the SRV failures disappear (0 in the run) but the title treats the failed allocation as fatal (a fail-fast after its own error log at `title+0x195e245`), so it does need that block. Next step if pursued: find what the 8.8 GB sizes (and whether the console version reserves it lazily), or reduce commit pressure by demand-committing.

Also done in this round: the forwarding of shifted console indices is now limited to the five with evidence (34, 46, 49, 59, 63); the debug layer showed the rest (for example `RSSetViewports` and `ExecuteCommandList`) receiving garbage. Diagnostics: `XB1_BIG_ALLOCS=1` (large `VirtualAlloc`, `XMemAlloc` and `HeapAlloc` calls with callers), periodic dumps of every context's live command buffer with `XB1_DUMP_CMD` (only one context exists), a global allocator logger for the layer's own 512 MiB+ allocations (none found), and `XB1_TITLE_MEM_MB`.

## Second test title: Sonic Mania
The folder given for Sonic Mania is a mounted Xbox package: `Mount\Sonic Mania.exe` (5 MB) plus `Data.rsdk` (208 MB), and an `EmbeddedXvd` tree holding the console OS image, which the layer does not use and the repository must not contain. It is far simpler than Forza: 421 imports resolve natively, only 7 are stubbed, and it uses the plain `D3D11CreateDevice` export of `d3d11_x.dll`, the standard `IDXGIFactory2`, and XAudio2. Running it needed, in order:
- **`d3d11_x.dll!D3D11CreateDevice`** forwarded to the real `d3d11.dll`, with the console SDK version (`131090`) replaced by `7` (it failed with `E_INVALIDARG` otherwise) and the returned device and context wrapped in the layer's proxies, because the title then queries console-only interfaces (`ID3D11DeviceX` `177700F9-...`, `ID3D11DeviceContextX` `48800095-...`).
- **`CoreApplication` extension `9FF3799E`**: Sonic Mania throws `InvalidCastException` without it (Forza tolerated `E_NOINTERFACE`). It now gets the null object (default on; `XB1_NO_COREAPP_9FF=1` restores `E_NOINTERFACE`); Forza was re-run and behaves the same.
- **`Windows.Xbox.Input.Gamepad`** slot 6: an empty list, like `Controller`.
- **`Windows.UI.Core.CoreWindow` statics** (`GetForCurrentThread`), which the real runtime answers with NULL: served by the shell, returning the console-layout window (slot 7 = dispatcher).
- **The swap chain** (new `src/display.rs`): the title calls `IDXGIFactory2::CreateSwapChainForCoreWindow`, which the real DXGI rejects for the shell's fake window. The shared vtable of the real factory is patched so slot 16 creates a native window on its own message-pump thread and calls `CreateSwapChainForHwnd` (a proxy device is unwrapped first; console-only flag bits such as `0x200000` are masked). The title asked for 1920x1080 `B8G8R8A8_UNORM`, 2 buffers, flip-sequential. This is the first real native window and swap chain the layer has made.
- **`DYNAMIC` textures** without CPU write access (the console allows it, the desktop does not) are now created with write access, like the buffers.
- **XAudio2**: the title loads XAudio2 dynamically and looks up `CreateXAudio2Object`, a console entry point; `GetProcAddress` is overridden so it resolves to a shim over the desktop `XAudio2Create` (assumed to have the same shape; it returns success).

Where it stops: the title now initialises (engine init returns success; audio and device are created), creates its views, enters its main loop and faults inside `IFrameworkView::Run` after a call on the console window object (slot 11), with a crash in `ntdll` (a read of `0x23`, from a call with `rcx=0x76`). Not analysed yet. Nothing is presented yet.

## Sonic Mania: it runs, presents, makes sound, and shows black
The crash after `IFrameworkView::Run` began was three separate bugs:
- **A heap overflow of mine.** For the console window's event-add slots the stand-in treated any two aligned pointers as `add_Event(handler, &token)` and wrote an 8-byte token through the second one. Sonic calls slot 11 with a small value there, so the write corrupted the heap and the process died later inside `RtlAllocateHeap` (`ntdll+0x328c7`, found by resolving the nearest export). The token is now written only when the first argument is a real delegate object.
- **A `LoadLibrary` of XAudio2 inside `Run`.** Loading a DLL with static TLS after the title's threads exist makes ntdll rebuild every thread's TLS vector (the same family of problem as before), so `xaudio2_9.dll` and `x3daudio1_7.dll` are now loaded at start-up, before any title thread exists.
- **`Present1` with NULL parameters.** The title calls `IDXGISwapChain1::Present1(1, 0, NULL)`; this DXGI build dereferences the parameters unguarded (`mov rax, [r9+0x10]` at `dxgi+0x194d9`), so the swap chain class's `Present1` slot (shared vtable) is patched to substitute an empty `DXGI_PRESENT_PARAMETERS`.

Result: Sonic Mania runs for the full 40 s test, calls `Present1` every frame (sync interval 1) and **plays sound** (reported by the user; the audio goes through the desktop XAudio2 behind the `CreateXAudio2Object` shim). The native window is **black**: nothing visible is drawn.

Why: the game draws through the console's embedded context functions (calls through function pointers stored in the context object), and only a few indices forward to the desktop context. From the arguments of the per-frame calls the console table is not simply the desktop one:
- indices 10, 11, 12, 14, 15, 16, 18, 19 and 23 match the desktop slots (`PSSetSamplers(0, 1, &s)`, `VSSetShader`, `DrawIndexed(54, ...)`, `PSSetConstantBuffers`, `IASetVertexBuffers(0, 1, &b)`, `IASetIndexBuffer(buffer, fmt, 0)`);
- index 8 is called as `(pointer, 0, ...)`, which is not `PSSetShaderResources(start, count, views)` (the desktop slot 8); it looks like a shader or resource setter with a pointer first;
- from index 32 up the console index is one below the desktop one: 32 is `OMSetRenderTargets(1, &rtv, dsv)` (desktop 33), 35 is `OMSetDepthStencilState(state, 1)` (desktop 36), 42 is `RSSetState` (desktop 43), 43 is `RSSetViewports(1, &vp)` (desktop 44);
- index 53 is `ClearDepthStencilView(dsv, 3, ...)` (flags 3), which is desktop slot 53, not 54, so the console table has one more method somewhere between 49 and 53 than the "one below" rule gives.
So the mapping has to be built per index from the arguments of real calls, and Sonic Mania is a good source: a small 2D renderer with about eleven different calls per frame. Next step: replace the range-based forwarding with an explicit console-to-desktop table, add the Sonic indices, and check what the screen shows.

## Tools added
`--watch-sites=0x...,0x...` plants one-shot breakpoints at title RVAs (report registers, text pointed at by the argument registers, and the title's call chain, then continue); `--watch-repeat` keeps them armed (re-arming by single-step) and prints one compact line per hit for the title's logger; `--patch-fastfail` was added earlier. The device's standard creation methods now log failures with a dump of the description and, for views, the resource's own type and description.

## Next investigation steps
0. Keep removing the startup stand-in gaps (each run stops in a different one now) until the first present, then build the swap chain.
1. Decode the command packet stream (the flush dumps), starting with the packet header layout and the common opcodes.
2. ~~Give the Xbox `User` element real methods~~ Shell-layer faults fixed (strings, events); the meaning of the user slots is still inferred.
3. ~~Find what corrupts the GPU driver's thread~~ Done (unverified context forwarding). Open: the `0x87D` exit under the debug layer.
4. Texture data: decode the console tiling and mirror placement textures.
5. The remaining decoding tasks listed earlier (`+0xA08`, `+0x4e0`, which context slot the console table lacks).
6. Pin down the console `ICoreWindow` layout: keep logging slots as the title uses more of them, and give stand-in behavior to a slot only where the call pattern justifies it.
7. Build the swap chain: `DXGIXPresentArray` and `DXGIXSetVLineNotification` exist as logging stubs, but the title never reaches a present, and its render-target view for the back buffer is still created with a NULL resource.
8. Work out the real shape of the `PackageTransferManager` interfaces and the `42A6A529` / `9FF3799E` `CoreApplication` extensions.
9. Provide a `G:` equivalent and inspect `media/` (encrypted content is out of scope).
10. Back the window with a native `HWND` and raise `CoreWindow` events.

## Sonic Mania: first output (explicit context table)

- The embedded context table is a constant one slot behind the desktop table from console index 32 on. `+0x580` is console 52 = `ClearDepthStencilView(dsv, 3, ...)` (desktop 53), not 53, so no extra method sits between 49 and 53; the missing method is somewhere between console 24 and 31.
- With the explicit table (`EMBED_EXTRA` in `src/gpu.rs`: 8→9, 10, 12, 16, 18, 19, 32→33, 35→36, 42→43, 43→44, 52→53) every context function Sonic calls now forwards to the desktop context.
- Result: the window is no longer black in most runs but shows a solid white frame (one run stayed black). Not yet known whether the white is a real game screen or a bad shader/texture binding (no shader resource view is bound: console index 8 is `PSSetShader`, so how textures are bound is still unidentified).
- Screen capture of the flip-model window needs it in the foreground; `PrintWindow` returns black.

### Sonic Mania: console indices 8 and 19 are not what the desktop names suggest

With `XB1_D3D_DEBUG=1`: forwarding console 8 as `PSSetShader` gives "First parameter is corrupt", and console 19 as `IASetIndexBuffer(buf, fmt, 0)` receives a pointer where the DXGI format should be (`0x1c02d575970`). Wrapping them (restoring the shader's desktop vtable, sanitising the format) ended in a NULL execute, so that attempt was reverted. These two (and the missing method between console 24 and 31) are likely the texture/shader binding that keeps the frame white: identify them from the raw arguments before forwarding.

### Sonic Mania: raw arguments of console 8 and 19 (log-only now)

- Console 8 (`+0x420`) is called as `(0x140f6f580, 0, 0x76b40)` from `title+0x214cbd`: a pointer into the title's own image, a zero and a constant that looks like a register base (`0x76b40`, same family as the PM4 register bases). It is not a desktop `PSSetShader`; it is a console "bind shader by register block" call, which fits the console compiling shaders to GCN microcode offline.
- Console 19 (`+0x478`) is called as `(NULL, ptr, 0)` from `title+0x214b47`, every frame. Not `IASetIndexBuffer`.
- Consequence to verify: if the shaders handed to `CreateVertexShader`/`CreatePixelShader` are GCN binaries rather than DXBC, the desktop cannot run them and a GCN-to-DXBC translator (or recognising the title's small fixed shader set) is needed for any picture. Check the bytecode header of the first `CreatePixelShader` call (DXBC starts with `DXBC`).
- Indices 8 and 19 now log instead of forwarding.

### Sonic Mania: the shaders are plain DXBC

The first bytes of every `CreateVertexShader`/`CreatePixelShader` bytecode are `DXBC`, and the desktop device creates them (`hr=0`). No GCN translator is needed for this title; the problem is only how the console binds them. Experiment: treating console 8 as `PSSetShader(*(a2))` made the debug layer accept a shader but report garbage class-instance counts, and the following draws had no vertex shader bound and primitive topology 0, then the title crashed (read of 0, execute of 0). So the shader/topology binding calls (console 8, 11 with real arguments, the topology call near 23 to 25) take arguments shaped differently from the desktop ones. The next step is to log the raw arguments of every index from 0 to 63 during the first frame (not only the unmapped ones) and decode them against the title's disassembly around `title+0x214b47` and `title+0x214cbd`.

`XB1_LOG_ALL=1` (src/gpu.rs) keeps only slots 0 to 3 of the embedded context table and logs every other index with its raw arguments. First attempt: Sonic then crashes with a NULL execute early (an index the title needs to return a result, probably Map/Unmap 14 and 15, now returns nothing), so the knob needs 14 and 15 kept forwarded before it yields the full first-frame call list.

### Sonic Mania: one frame of embedded context calls (decoded)

With `XB1_LOG_ALL=1` (Map/Unmap still forwarded), the same sequence repeats every frame. Index = (offset - 0x3E0) / 8; `*` = argument is a pointer to a variable holding the object.

| order | index | arguments | meaning |
|---|---|---|---|
| 1 | 32 | `(1, *rtv, dsv)` | `OMSetRenderTargets` |
| 2 | 43 | `(1, *viewport)` | `RSSetViewports` |
| 3 | 49 | `(rtv, color*)` | `ClearRenderTargetView` |
| 4 | 52 | `(dsv, 3, ..)` | `ClearDepthStencilView` |
| 5 | 42 | `(state)` | `RSSetState` |
| 6 | 35 | `(state, 1)` | `OMSetDepthStencilState` |
| 7 | 34 | `(state, NULL, 0xFFFFFFFF)` | `OMSetBlendState` |
| 8 | 10 | `(0, 1, *sampler)` | `PSSetSamplers` |
| 9 | 23 | `(0, this, *sampler)` | unknown (arguments look shifted by one) |
| 10 | 18 | `(0, 1, *buffer)` | `IASetVertexBuffers` |
| 11 | 19 | `(0, buffer, 0)` | index buffer: the first argument is `0`, the buffer is the second (a console format enum before the buffer, not a DXGI format after it) |
| 12 | 7 | `(0, 1, *cb)` | `VSSetConstantBuffers` |
| 13 | 16 | `(0, 1, *cb)` | `PSSetConstantBuffers` |
| 14 | 8 | `(*obj, 0, 0x76b40)` | shader or resource bind, object passed by address |
| 15 | 12 | `(0x600000000, 0, 0x76b40)` | draw (`edx` = 0; the high half is garbage) |

Notes: indices 34, 35, 42, 43, 49, 52 are one slot behind the desktop table (constant shift from 32 on). No `VSSetShader` (11), `IASetPrimitiveTopology` or `PSSetShaderResources` shows up in the first three frames, so shaders, topology and texture views are bound by index 8, 19, 23 and the draw itself, with console-specific argument shapes. The 0x76b40 constant on 8 and 12 is the same register-base family as the PM4 bases. Next: disassemble `title+0x214c8d..0x214f28` to see how index 8's argument is built, and treat index 19's first argument as the index format.

### Sonic Mania: console 8 and 19 identified

- Console 8 (`+0x420`) is `PSSetShaderResources`: its first argument is the address of a variable holding a real `ID3D11ShaderResourceView` (confirmed by `QueryInterface`). Forwarded as `PSSetShaderResources(0, 1, views)`.
- Console 19 (`+0x478`) is `IASetIndexBuffer`: the second argument is a real `ID3D11Buffer`, the first is a format enum (0 treated as R16_UINT). Forwarded with a DXGI format.
- With those two bound the debug layer's remaining draw errors are only "no vertex shader bound" and "primitive topology 0". Console 12 is `DrawIndexed(count = edx, start = r8)` (the high half of `rdx` is junk, e.g. `0x60000001e` = 30 indices).
- The vertex shader, input layout and topology are never set through any index seen in a frame (the per-frame set is 7, 8, 10, 12, 16, 18, 19, 23, 32, 34, 35, 42, 43, 49, 52). They must be bound in a call whose arguments hide them, probably index 23 `(0, <context>, &object)` or 10, whose `*a4` is a real D3D11 object that is not an SRV, PS, VS or buffer (a sampler or an input layout). Probing 7/16/18/23 together crashed the title (NULL execute), so probe them one at a time.

### Sonic Mania: console 23 raw arguments

Console 23 (`+0x498`) is called every draw as `(0, p, q)` where `p` points to 32 bytes `{ptr, ptr, 2, 1, 0, 0}` (two heap pointers, then the counts 2 and 1) and `q` is sometimes stack garbage (`0xffffffff66ff2fb0`, unreadable) and sometimes a real pointer to two heap pointers. So it takes at least a descriptor with two object pointers plus counts: a good candidate for the combined VS/layout/topology bind (two objects = vertex shader + input layout, or vertex shader + pixel shader). Next: dump the two pointers in `p` and compare them with the objects returned by `CreateVertexShader`/`CreateInputLayout` (`CREATED_KINDS` records them). Probing a non-COM object with `QueryInterface` crashes the title; read memory with `safe_dump` only. `ctx_probe::<23, 0>` is active (console 23 logs and does nothing).

### Sonic Mania: shaders are bound through the command buffer, not a context method

The two objects console 23 receives are not shaders or layouts (no creator recorded them). Across a whole run no context method that could bind a vertex shader, pixel shader, input layout or topology is ever called, yet draws happen. The constant `0x76b40` passed to console 8 and 12 is a PM4 register base (the same family as `0x6b40`/`0x7b40` in the command-buffer notes): the console API inlines shader and topology state as PM4 register writes into the context's own command buffer, and only some calls (views, buffers, render targets, states, draw) also go through the embedded table. So a picture needs the PM4 decoder after all, for the shader program address registers (map the address back to the `DXBC` object created for it, so each draw binds the right VS/PS), the vertex-input and topology registers, and the draw packets. `tools/analyze-cmdbuf.py` and `XB1_DUMP_CMD` already dump and parse the stream: run them on Sonic Mania, look for the SET_SH_REG packets whose register offsets match `0x76b40`, and find where the shader address comes from (probably a field of the object `CreateVertexShader` returns, which on the console holds the GPU address).

### Sonic Mania: the command buffer does fill, and the desktop device is removed

- A crash-time context dump shows `context field +0x50` (the command cursor) moving from 0 to `0x244ae2d0000` and `+0x58` to `0x244ae6d0000`: Sonic does write packets into the context's command buffer, which confirms the PM4 theory. `XB1_DUMP_CMD=<dir>` wrote nothing because `dump_all_contexts` only covers contexts in `CONTEXT_PROXIES` (the D3D11X path), not the contexts `wrap_created_device` wraps. Fix: register those contexts too, then run `tools/analyze-cmdbuf.py` on the dumps.
- After a few frames `Present1` returns `0x887a0005` (`DXGI_ERROR_DEVICE_REMOVED`; the debug layer says `DXGI_ERROR_DEVICE_HUNG`) following a caught C++ exception inside `d3d11.dll`/`nvwgf2umx.dll`. After that nothing can draw, which is why the window stays white or black. Something we forward (the first suspect is the SRV or index buffer bound with no vertex shader, or `DrawIndexed` with an invalid state) hangs the GPU; bisect by disabling forwards one by one.

### Correction: Sonic Mania writes no PM4 packets

The two sections above that say the command buffer fills (and that shaders are therefore bound by PM4 writes) are wrong. The `context field +0x50` value in the crash dump is the cursor our own `new_proxy` sets, equal to the buffer start. A real dump (after fixing `dump_all_contexts` to measure from the current buffer end) reports `cursor 0x24b49040000 end 0x24b49440000 cmd_base 0x24b49040000 used 0`: **no packets are written**. So the `0x76b40` constant is not evidence of PM4, and how Sonic Mania binds its vertex shader, input layout and topology is still unknown (candidates: the inlined bind goes through memory the title writes directly, e.g. into the objects we returned from `CreateVertexShader`, or through a device-level extension slot).

The device removal (`Present1` = `0x887a0005`) is still the immediate blocker and is unexplained.

### Sonic Mania: the inlined state cache (shaders and layout found)

The vertex shader, pixel shader and input layout are written by the title into the context object itself: the draw setup at `title+0x2149fe..0x214ae5` does `or [ctx+0x168], <dirty bits>` and `mov [ctx+0x178/0x180/0x188], object` (0x178 input layout, 0x180 vertex shader, 0x188 pixel shader; `+0x170` holds another packed state word, `0x9ffc0009e00`) and only calls the method (`+0x438`, `+0x428`, `+0x468`) when a flag at `+0x390` says the cache is off. So the console draw consumes that state. `apply_inlined_state` (src/gpu.rs, called from `sync_before_draw`) now binds those three objects (found through `CREATED_KINDS`) and a triangle-list topology before every draw. Result: the debug layer's "no vertex shader" and "topology 0" errors are gone and the device is no longer removed. What remains is a warning that vertex slot 0 is too small for the draw (DrawIndexed 30 indices), and the window is still black, so the next questions are what the vertex buffer holds (is it mirrored/uploaded?) and whether the game has really started drawing (it may still be on its black boot screen).

### Sonic Mania: first picture

The black frame was the draw's base vertex: console `DrawIndexed(count, start)` has no base-vertex argument, so `r9` still held `0x76b40` and the desktop read the vertices from far beyond the 320-byte vertex buffer. `ctx_draw_indexed` (console 12) now passes 0. With that the window shows Sonic Mania's intro artwork (Sonic's shoes), with no debug-layer warnings. Known glitch: the image appears four times in a 2x2 grid, so the viewport, the render-target size or the texture coordinates are still wrong (the title's viewport is `RSSetViewports(1, &vp)` with a console `D3D11_VIEWPORT`; check its values and the back buffer size). Buffers seen: vertex buffer 320 bytes (16 vertices, stride 20) and index buffer 78 bytes, both IMMUTABLE with initial data, plus two DYNAMIC constant buffers (`0xd00`, `0x80` bytes).

The 2x2 repetition is not the viewport (`RSSetViewports(1)` = 0,0,1920,1080, depth 0..1). The game's image is a 512x256 DYNAMIC texture in format 0x55 (`B5G6R5_UNORM`) that the title rewrites every frame, drawn as one scaled quad; each of the four copies shows the whole frame, so the texture coordinates span 0..2 (wrap sampler) or the constant buffer (`0xd00` bytes, DYNAMIC) that scales them is wrong. Next: log the constant buffer contents after `Unmap`, the sampler description and the vertex data (16 vertices, stride 20).

### Sonic Mania: the 2x2 repetition was the draw's packed arguments

The vertex buffer holds four 4-vertex quads: a full-screen quad, left/right half layouts and top/bottom ones for split-screen (UVs 0..0.828 x 0..0.9375 = 424x240 of the 512x256 texture). The index buffer picks which layout to draw, and the console `DrawIndexed` packs `(index count << 32) | first index` into one register (`0x600000000` = 6 indices from 0, `0x60000001e` = 6 from 30). The layer had treated the low half as the count (30 indices = five quads, i.e. every layout at once). `ctx_draw_indexed` now unpacks it and the intro artwork renders once, filling the window with no debug-layer warnings. (Test title was renamed `SonicMania.exe`.)

### Status: Sonic Mania boots

Sonic Mania (1.0.4.2, unpacked) now boots under the layer and runs through its intro, logos and menus with correct picture and sound (verified by the user watching the window, 2026-11-02). Input is the missing piece for playing it.

### Sonic Mania: first look at input

`XB1_GAMEPAD=1` makes the `Controllers` list contain one stand-in element. The title then calls `GetAt(0)`, calls slot 6 on the element through the `Controller` interface (`7400624F-...`; it takes an out-pointer, so it returns an object), queries the element for `9776FF56-9BFD-4581-AD45-B645BBA526D6` (the gamepad interface) and calls slot 6 on that too (also an out-pointer, probably `GetCurrentReading`), then subscribes to three controller events (slots 7, 9 and 11). After that, nothing touches the stand-ins again during the 12-second run, so the per-frame poll is either not reached with the generic stand-ins or goes through objects they returned. Next: give the two returned objects real shapes (a reading with the button bitmask, two trigger values and four thumbstick axes, filled from XInput and the keyboard) and log which slots the title calls on them. The switch is off by default because nothing is mapped yet.

#### Input, second pass (`XB1_GAMEPAD=1`)

- Once the list has an element, the title polls it **every frame** from `Run`: `QueryInterface(IGamepad 9776FF56-...)`, then slot 6 (`get_User`: it compares the result with an element of the user list, so `Users.GetAt(0)` and the controller's user must be the same object; the layer now hands out one shared user element), then slot 8 with an out-pointer. With the generic stand-in returned from slot 8 nothing was ever called on it afterwards; answering slot 8 with the constant `1` (a guessed UINT64 id) made the slot 8 call disappear from the log entirely, so the title's decision depends on what slots 6 and 8 return.
- Not yet found: where the button state is read (no method call on any returned object is seen, so it may be a struct returned through an out-pointer, which the generic stand-in cannot fill). Next: disassemble the per-frame poll from `IFrameworkView::Run` (the `QueryInterface` call site for `9776FF56`, found in the title's data at RVA `0x3b84a8` but not referenced by plain `lea`s, so look for it through the stack arguments), identify the reading layout and fill it from XInput and the keyboard.
- Whether the game responds to a button press has not been tested; the switch is off by default.

### Frame pacing

Titles are written for the console's 60 Hz output (sync interval 1 = 60 frames a second), but a desktop vsync only paces to the monitor, so Sonic Mania ran too fast on a faster display. `limit_frame_rate` (src/display.rs, after every `Present1`) now sleeps to the next 1/60 s slot (coarse sleep, then a short spin); measured 60 to 62 frames per second over 5 seconds. `XB1_FPS=<n>` sets another rate, `XB1_FPS=0` turns the limit off.

### Input works: the `GamepadReading` layout

The title's per-frame poll (`title+0x254e30`) queries the controller element for the gamepad interface, calls slot 6 (`get_Id`, a UINT64 hashed with xor of its halves and compared with the stored id), zeroes 36 bytes on its stack, calls slot 8 (`GetCurrentReading(GamepadReading*)`) and reads the struct directly:

| offset | field |
|---|---|
| 0x00 | timestamp (u64) |
| 0x08 | buttons (u32): Menu 4, View 8, A 16, B 32, X 64, Y 128, DPad up 256, down 512, left 1024, right 2048, left shoulder 4096, right shoulder 8192, left thumbstick 16384, right thumbstick 32768 |
| 0x0C / 0x10 | left / right trigger (f32, 0..1) |
| 0x14 / 0x18 | left stick x / y (f32, -1..1) |
| 0x1C / 0x20 | right stick x / y (f32) |

`fill_gamepad_reading` (src/winrt.rs) fills it from the first XInput pad and, while the layer's window has the focus, the keyboard: arrows = d-pad, Z / X / A / S = A / B / X / Y, Enter = Menu, Backspace = View, Q / W = shoulders. One stand-in controller is now always present (`XB1_NO_GAMEPAD=1` removes it). Confirmed by the user playing with the keyboard.

### Starting a game crashed: `SystemUI` async operation

Entering a game made the title call the `Windows.Xbox.UI.SystemUI` statics slot 10 as `(user, 0, Object** out)` (call site `title+0x26ac25`; it creates an `IAsyncOperation`-like object) and then slot 6 on the result, `put_Completed(handler)`. The generic stand-in treated slot 10 as an event subscription (out-pointer left NULL: crash at `title+0x26660c`), and then treated slot 6 as an event add too, writing a token through its second argument, which for that call is the object's own vtable pointer: vtable slot 0 became `0xb` (crash "execute of 0xb"). Now slot 10 returns a stand-in operation and `put_Completed` invokes the handler immediately with `AsyncStatus::Completed` (1). Result: Sonic Mania runs from the title screen into Green Hill Zone (score, time, rings, pause menu) with correct pictures.

Test hook for scripted runs: `XB1_AUTOPRESS="30:4:1;34:16:1"` presses Menu for 1 s at t = 30 s and A for 1 s at t = 34 s (button bits as in the reading table above).

Input note: the layer reads XInput slots 0 to 3 first; if none answers it reads the legacy joystick API (winmm), which sees HID pads such as a DualSense or DualShock (USB or Bluetooth), assuming the usual Windows layout (Cross = A, Circle = B, Square = X, Triangle = Y, Options = Menu, Create = View, L1/R1, L3/R3, hat = d-pad, X/Y and Z/R = sticks, L2/R2 as buttons). The winmm path was written without a pad to compare against: report any wrong button or axis.

#### DualSense through HID

The winmm entry for the user's Bluetooth DualSense never changed (centred axes, no buttons, whatever was pressed), so `src/dualsense.rs` reads the pad through the HID API: it finds VID 054C / PID 0CE6 (or the Edge 0DF2) with SetupAPI, opens it and reads input reports on a background thread. Reports understood: USB `0x01` (64 bytes), Bluetooth simple `0x01` (10 bytes) and Bluetooth full `0x31` (78 bytes, the one seen here), converted to the Xbox bits (Cross = A, Circle = B, Square = X, Triangle = Y, Options = Menu, Create = View, hat = d-pad, L1/R1, L3/R3; L2/R2 analog; sticks centred at 127.5). The layout follows the Linux `hid-playstation` driver and has not been checked against button presses by me. XInput first, then the DualSense reader, then winmm.

### Test report (user, 2026-11-02)

Sonic Mania plays from the title screen through gameplay with the DualSense over Bluetooth (confirmed working) and the keyboard, with no input lag, no crashes and no visible graphical glitches. The DualSense over USB uses the same reader (report `0x01`, 64 bytes) but has not been tested. Only the early part of the game has been played, so later zones, special stages and the 2-player split screen are untested.

## Full Forza Horizon 3 (Opus 1.0.125.0) first run

The full game's `forza_durango_release_final.exe` (85 MB, 17 sections) loads at its preferred base: 726 imports resolve, 15 stay stubbed (EtwPlus 4, `xg_x` 7, `D3DMapEsramMemory`, `DXGIXGetFrameStatistics`, `GetConsoleType`, `SetWasapiThreadAffinityMask`). `DXGIXPresentArray` and `DXGIXSetVLineNotification` are now resolved, unlike in the kiosk demo.

`--run` (with `XB1_FILES=1` to log file opens) gets through startup, mounts about 85 of the 1075 zips listed in `media\zipmanifest.xml` (the manifest paths use the console's `game:\` prefix, which the title translates to `.\media\...` itself), reads several physics and audio XML files, then crashes with a NULL read at title+`0x1b04f7f`:

- The function at title+`0x1b04d40` loops over a name table (`Global`, `Startup`, `PreRace_321`, ...) and opens `game:\<name>.xml` through title+`0x1ea29d0`. The result object is NULL when the open fails, and the code then dereferences `[rsp+0x40]` (0) without a check.
- No file API call is logged for that open, so the lookup fails inside the title's own zip layer, before any `CreateFile2`. `Global.xml` exists only inside zips (`stateflow.zip`, `renderscenarios.zip`), and neither zip had been opened when the crash happened.
- `Windows.Xbox.Management.Deployment.PackageTransferManager` (`slot 6` plus an event subscription) is polled about 1500 times before the crash, interleaved with the file loads. It may be gating zip mounting (install-state polling); that is a guess, not verified.

Open question: why the title mounts zips in a different order than the manifest and never reaches `stateflow.zip`. Next step: log the title's zip mount decisions (or read the object at `[rbp+0x18]` in that function) and check what the `PackageTransferManager` methods are expected to return.

### Full Forza Horizon 3: the failing open is `RenderScenarios\XboxOne\Global.xml`

- The path the crashing function (title+`0x1b04d40`) passes to the open routine (title+`0x1ea29d0`) was read with `--watch-sites=0x1b04eea` and `XB1_PEEK="rdx+0>0:32;rdx+0>0x20:16"`: `game:\media\RenderScenarios\XboxOne\Global.xml`. It exists only inside `media\renderscenarios.zip` (entry `XboxOne/Global.xml`). The title never opens that zip.
- `zipmanifest.xml` (1075 `game:\...zip` entries with directory offsets) is read first, then five worker threads are created suspended (title+`0x1a3f0c0`). The zips the title does open (85 of them: `ui`, `gametunablesettings`, `ui\*`, `drivers\*`, `stripped\*`) come from directory enumerations (`FindFirstFileExA(".\media\UI\*.zip")`), not from the manifest. Reads on them are overlapped (`ERROR_IO_PENDING`, completed through thread-pool I/O).
- **It is not a timing race:** `XB1_STALL_MS=20000` (new: holds the thread 20 s at a `--watch-sites` hit) opened no further zips and the crash is identical. So manifest-listed zips are not mounted eagerly and an on-demand mount for `RenderScenarios\...` does not happen.
- Also ruled out: the `PackageTransferManager` calls (about 1500) are just the title fetching the manager object (title+`0x1c357b0`, vtable slot 6 = static getter) and registering an event handler, not an install-state wait.
- Next: find the title's zip-lookup routine (called from title+`0x1ea29d0`) and see why the manifest entry for `renderscenarios.zip` is not matched; the manifest parser is title+`0x18dd6d0`.

### Full Forza Horizon 3: what else was ruled out

- The pool of five worker threads (title+`0x1a3f0c0`, created suspended by title+`0x1a35000`, resumed through a tail-called `ResumeThread`) does start; a `[thread] title thread started` line now logs each start. So the missing zip mounts are not caused by workers that never run.
- `XB1_FILES=1` now also logs `GetFileAttributesW`, `GetFileAttributesExW` and `FindFirstFileW`. The title makes no such call for `renderscenarios.zip` (or for any root-level zip such as `stateflow.zip`); it only enumerates `UI\`, `UI\Textures\`, `Drivers\` and `Stripped\` for `*.zip`. Reads on zips are overlapped (`ERROR_IO_PENDING`), completed through thread-pool I/O.
- Still unexplained: the code path that is meant to mount the root-level zips listed in `zipmanifest.xml`. The lookup (title+`0x1e9ed30`, `0x1ea9090`, both behind a global file manager and a critical section) finds nothing for the `RenderScenarios` folder, so the next step is to find where that path is matched against the manifest entries.

## Minecraft Xbox One Edition (Minecraft_1.2.0.0, `Minecraft.Client.exe`)

First pass on the Legacy Console Edition build (4J Studios, built with Visual C++ 2012):

- **Imports:** 477 resolve, 4 stub (EtwPlus). Graphics come from plain `D3D11CreateDevice` in `d3d11_x.dll` (like Sonic Mania, no `D3D11XCreateDeviceX`), so no PM4 command stream is expected. The runtime is `vccorlib110` / `MSVCR110` / `MSVCP110` (installed on the test PC), not the 2015 one.
- **Fixes it needed:** (1) the activation-factory hook only matched `vccorlib140`; it now matches `vccorlib110` too and falls through to that DLL. (2) `Windows.ApplicationModel.Store.CurrentApp` now gets the logging stand-in (the real one fails without package identity, and the title throws a `Platform::COMException` from `LicenseInformation`). Progress: the title now activates `GameTransportControls`, `User`, `DownloadableContentPackageManager`, `Store.Product`, `PartyConfig`, creates the window, device and swap chain (native window 1920x1080) and renders targets.
- **Older console DXGI ABI:** this title calls `GetAdapter` through vtable slot 8 of `IDXGIDevice1` (desktop 7), `GetParent` through slot 7 of the adapter (desktop 6) and `GetBuffer` through slot 10 of the swap chain (desktop 9). Desktop slot 8 is `CreateSurface`, which answered `DXGI_ERROR_INVALID_CALL` (0x887A0001) and made the title throw. So when the title uses `vccorlib110` the layer wraps DXGI objects (device, adapter, factory, swap chain) in forwarding objects whose slots 7 and up go to desktop slot n - 1 (`wrap_dxgi` in `src/gpu.rs`). The exact start of the shift (somewhere in slots 3 to 7) is inferred from these three calls, not known.
- **Where it stops now:** the title calls `GetDesc` on the back buffer texture through vtable slot 12 (desktop `ID3D11Texture2D::GetDesc` is 10), so the D3D11 resource interfaces are shifted by two in this SDK, and the crash is in `d3d11.dll` (title+`0x18818b`). Likely the whole D3D11.X object model of this older XDK differs from the Sonic Mania one (device, resources, views, context), so the Sonic-derived context table cannot be assumed to apply.

## Content drive bug

`map_content_drive` was given an empty folder whenever the title was started by its bare file name (`xb1layer.exe SonicMania.exe --run`), so the `DefineDosDevice` call failed with error 123 and `G:` stayed pointing at whatever an earlier run had left. The folder is now made absolute, and the failure message prints the folder and error. This is not what stops Forza: it still fails the same way with the right `G:`.

### Minecraft Xbox One Edition: D3D11 layout of the older XDK

- **Resources:** `ID3D11Texture2D::GetDesc` is called through vtable slot 12 (desktop 10), so resource interfaces are two slots longer from slot 9 on. `GetBuffer` results from the swap chain are now wrapped as such (`wrap_dxgi(.., 3)`: console slot n from 9 forwards to desktop n - 2) and `create_view` hands the real resource to the desktop (`unwrap_object`). The title's render-target and depth-stencil views now create.
- **Embedded context table is two slots behind the desktop one** (Sonic Mania's is one behind): console 31 is called as `(1, &rtv, dsv)` = `OMSetRenderTargets` (desktop 33) and console 42 as `(1, &viewport)` = `RSSetViewports` (desktop 44). `OLD_SDK_EXTRA` in `src/gpu.rs` forwards just these two when the title links `vccorlib110`; every other index only logs (use `XB1_LOG_ALL=1` to log all of them).
- **Where it stops now:** device slot 41 is called with `(&x, view, view)` (a console-only device method, still a logging stub) and then title+`0x56098b` calls a QueryInterface on a title-owned global object at `0x1409713d8` (the object whose vtable points at the title's own QI routine) that reads from address `-1`. Not resolved: the global is expected to be filled by the missing slot-41 method.
- Sonic Mania was re-run after these changes and presents frames as before.

### Minecraft Xbox One Edition: second pass (further into startup)

- **Device slot 41 is `GetImmediateContext(&out)`** in this SDK (desktop 40): the result becomes the title's context. Wired for `vccorlib110` titles.
- **Shaders are plain DXBC** (`44584243`), created through device slots 12 (vertex) and 15 (pixel), both unshifted, and they create fine.
- **Older-XDK embedded context table** (console index → desktop slot, from logged arguments): 13 → 14 `Map`, but with the console shape `Map(res, sub, type, flags, void** data)` that returns the data pointer itself (`ctx_map_old` converts from the desktop `D3D11_MAPPED_SUBRESOURCE`); 14 → 15 `Unmap`; 31 → 33 `OMSetRenderTargets`; 42 → 44 `RSSetViewports`; 87 → 89 `OMGetRenderTargets`. Note the offset grows (+1, +1, +2, +2, +2), so the table is missing methods at more than one place. Seen but not yet identified (log only): 6 `(0,10,&p)`, 7 `(0,1,&p)` (probably `VSSetConstantBuffers`, desktop 7), 9, 15 `(0,6,&p)`, 17, 18, 19, 20, 46.
- **View and resource wrappers:** `wrap_dxgi` kinds 3 (resource) and 4 (view); wrapper `QueryInterface` and `GetResource` results are wrapped too. View `GetResource` is called through slot 8 (desktop 7); resources: console 8..10 → desktop 7..9 and 12 and up → desktop n - 2.
- **Controller list:** the older title reads the controllers through `IVectorView::GetMany` (slot 9), now implemented. Controller slot 8 returns an object here, not Sonic Mania's 36-byte `GamepadReading`, so Sonic's reading writer is skipped for `vccorlib110` titles (it overwrote the title's stack).
- **Where it stops now (61 MiB committed, startup of Xbox Live, storage, party, networking and chat stand-ins ran):** a jump to address `0x10` after `Microsoft.Xbox.GameChat.ChatManager` slot 6 (title+`0x2c86f0`, which queries the object it gets for another interface). The package ships a real `Microsoft.Xbox.GameChat.dll`; a stand-in may be the wrong approach for it.

### Minecraft Xbox One Edition: third pass (the game now runs its main loop)

Fixes that took it from the GameChat crash to a main loop that runs for 30+ seconds without crashing (no frame is presented yet):

- **A stand-in's vtable was being overwritten.** `Storage.ConnectedStorageSpace#slot8` is an async operation (slot 6 is `put_Completed(handler)`, second register junk). The event-subscription path wrote a token into that junk, which was the address of the shared generic vtable, so slot 0 of every stand-in became `0x10` and the next `QueryInterface` jumped to `0x10`. Fixed two ways: that object now completes at once like the `SystemUI` operation, and a token is never written into the layer's own image (`in_layer_image`). A check at each generic call reports the first call that corrupts the vtable. Stand-ins are also never freed now (the title balances its own references on the assumption that the runtime owns the object).
- **Thread-local storage on title threads dropped the drivers' own.** `install_tls_for_current_thread` copied only 16 entries of the loader's TLS pointer array, so threads the title created lost the implicit TLS of later-loaded DLLs (the NVIDIA driver crashed in `CreateDeferredContext` with a read of `0x6c`; the same call worked on the main thread and on a plain helper thread). Now up to 64 entries are copied, and the loader's own array is put back when the title's thread routine returns, so ntdll's thread teardown frees what it allocated.
- **`Map` output struct was 16 bytes** (`D3D11_MAPPED_SUBRESOURCE` is 24): fixed.
- **Thread stacks:** console threads ask for 16 to 128 KB; they are raised to 1 MiB (reservation).
- **`CurrentApp.LicenseInformation`:** slot 7 `IsActive` = true, slot 8 `IsTrial` = false (as `boolean*`); the generic stand-in wrote pointers, so the game ran as a trial and polled the license in a loop.
- **Debug aids added:** the exception report scans for indirect calls too and dumps raw stack words on an execute fault; `XB1_NO_DEFERRED=1`, `XB1_TEST_DEFERRED=1`, `XB1_DEFERRED_ON_HELPER=1` (experiments for the deferred-context crash); the layer logs `CreateDeferredContext`; a linker map (`/MAP`) can symbolize layer frames.
- **Where it stands:** the title creates deferred contexts on several threads, runs XAudio2, polls the controller (slot 8 of the old `IGamepad` returns an object here, not a 36-byte reading) and loops; no `Present` yet and no draw call has been identified. About twenty embedded context indices are logged but not mapped.

### Minecraft Xbox One Edition: fourth pass (first present, black window)

- **The game now presents**: its swap chain (console `IDXGISwapChain::Present` is slot 9, desktop 8; wrapped with the frame limiter) is presented every frame into the layer window, which is black: no draw call has been seen yet and the render-target clear is not mapped.
- **Call trace aid:** `XB1_CTX_TRACE=n` logs every embedded context call in order (at most 10 per index), and names real D3D11 objects in the arguments (`{Buffer/-}`, `{DSV/-}`, ... first the pointer, then what it points to). Objects the layer created with extended vtables show `-`.
- **Older-XDK embedded indices identified from shapes and objects** (forwarded two behind the desktop slot): 33 `OMSetBlendState(state, factor {1,1,1,1}, 0xFFFFFFFF)` → 35, 34 `OMSetDepthStencilState(state, 0)` → 36, 41 `RSSetState` → 43, 43 `RSSetScissorRects(1, &rect)` → 45, 46 `UpdateSubresource(res, 0..4, NULL, ..)` → 48, 51 `ClearDepthStencilView(dsv, 3, ..)` → 53 (plus the earlier 13, 14, 31, 42, 87). Not forwarded yet (seen, shape known, desktop slot unsure): 6 `(0, 10, &array)` and 15 `(0, 6, &array)` (constant-buffer style), 7 `(0, 1, &null)` and 19 `(0, 1, &SRV)` (shader-resource style), 9 and 20 `(0, 1, &sampler)`, 17 `(0, 1, &null)` (vertex buffers?), 18 `(1, 0, 0)`, 108 `(1920, 0, 1080)` (console-only). The title code at 0x565090 sets 7 and 9 together and 19 and 20 together: two stages' (resources, samplers) pairs, 7/9 two apart like the desktop `PSSetShaderResources`/`PSSetSamplers` (8/10), but 19/20 are adjacent like the desktop `VSSetShaderResources`/`VSSetSamplers` (25/26), so the console table is not simply the desktop one minus a constant (which stage is which is unproven).
- **No draw index seen yet:** the first ten calls of every index contain no `Draw`/`DrawIndexed`; the game sits in a loop polling the gamepad (`IGamepad` slot 8 at title+`0x53d105`) and has not started rendering the world or menus. Next: find what the loop waits on (controller or user events, storage, a sign-in flow) and identify the draw indices once it renders.

### Minecraft Xbox One Edition: fifth pass (draws reach the driver, window still black)

- The user confirms **sound plays** (XAudio2 and the game's music), so the game logic runs.
- **Same inlined state cache as Sonic Mania:** at the first draws the context object holds a layout, a vertex shader and a pixel shader at `+0x178`, `+0x180` and `+0x188` (kinds 11, 12, 15 by creating device slot), so `apply_inlined_state` binds them unchanged. The `+0x168` dirty bits and `+0x170` state word are written too (topology may live there; the layer still forces a triangle list).
- **Draw indices (older XDK):** 10 `(0x27_0000_0000, 0, 0)` is a packed `DrawIndexed` (39 indices from 0, like Sonic Mania's 12), 11 `(4, 0, ..)` is `Draw`. Both are forwarded through `sync_before_draw`. Also forwarded: 17 `IASetVertexBuffers` (18), 18 `IASetIndexBuffer(format, buffer, offset)` in Sonic's shape (index buffer: 78 bytes, console format 0), 6 and 15 constant-buffer sets (7 and 16), 19 and 20 `(0, 1, &SRV)` / `(0, 1, &sampler)` as the pixel-stage resource and sampler sets (8 and 10), 7 as a guessed vertex-stage resource set (25). Which stage 19/20 and 7/9 belong to is a guess from the repeating pattern (the real SRV only ever goes through 19).
- **Result:** no debug-layer messages, no crash, `Present` every frame, but the window is still black after 75 seconds. Not yet checked: whether the draws go to the back buffer (the console `OMSetRenderTargets`/`OMGetRenderTargets` results), the render-target clear (console index unknown, probably the one before 51 `ClearDepthStencilView`: 48?), the viewport, and the shader constants (`Map`/`Unmap` writes are forwarded).

### Minecraft Xbox One Edition: sixth pass (the main menu renders)

The title screen draws: panorama background, logo, splash text, five buttons with text and the controller prompts (screenshot of the layer window). What it took after the black window:

- **The black window was an index arithmetic error of mine**, found by logging the draws per frame: the last draw had count 0. `+0x438` is index **11**, not 10 (`(0x438 - 0x3E0) / 8`), so the packed `DrawIndexed` is console 11 and the plain `Draw` is 12; the texture/sampler pairs at `+0x498`/`+0x4A0` are 23/24 (desktop 25/26, two ahead like the others) and `+0x418`/`+0x428` are 7/9 (desktop 8/10). The earlier note that listed them as 10/11 and 19/20 was wrong.
- **Aids that found it:** `XB1_READBACK=1` copies the back buffer to a staging texture before three Presents and counts non-black pixels (it went from 0 to 1.8 million); the frame log prints the draw count and the arguments of the last draw; the D3D debug layer showed 86,000 warnings that the pixel and vertex stages expected a sampler (valid, but it proved the draws ran).
- **Topology:** the title keeps it in console state the layer does not read yet. Four-vertex plain `Draw`s are quads as triangle strips (as lists they were one diagonal triangle, which cut the logo and clipped the button text), so a four-vertex `Draw` now uses a strip and everything else a list.
- **Also confirmed:** viewport and scissor are 1920x1080; the render targets the title sets are both views of the swap chain's buffer; state objects (`XB1_DEFAULT_STATES=1` replaces them with the defaults as a test) and the depth clear were not the problem.

### Minecraft Xbox One Edition: input (not working yet) and two layer bugs

- **`in_layer_image` never worked:** it used `GetModuleHandle(NULL)`, which is the *title* here (the bootstrap points the PEB image base at it), so the "never write a token or a child into the layer's own image" guards did nothing. It now finds the layer's module from the address of its own function. `is_writable` also refuses the layer's image now, which fixed the `Windows.Kinect.KinectSensor` call whose first argument was the generic vtable's own address (the title passes `this`'s vtable in a spare register) and was overwriting vtable slot 0 again.
- **Kinect:** the title's user-event handlers activate `Windows.Kinect.KinectSensor` (class not registered, 0x80040154, so the handler threw); `Windows.Kinect.*` now gets generic stand-ins.
- **Controller slot 8 (older `IGamepad`/`IController` interface `9776FF56`)** is called once per frame (4,200 calls a minute) with a pointer to an 8-byte local and the title keeps the returned object in the controller record (`+0xA0`, 0xA8-byte records, 8 of them) after releasing the previous one. It now returns the shared user (the accessor at title+`0x53c6cd` hands that object out as the controller's user). A returned child object is never called either, so slot 8 may instead be `GetCurrentReading` returning a reading object; not settled.
- **The menu does not react** to the keyboard (Z = A pressed in the focused layer window) or to XInput: no stand-in method is called when a key is pressed, so the title is not polling a reading object we provide. The screen still says "Select / Choose User". The next step is to find which object the per-frame code reads buttons from (the readers of the controller record around title+`0x53b800` to `0x53e400`; delegate invocations there use interface `082E402C` and `9DE1C534`/`9DE1C535`), or raise the controller events the title registered for (`Gamepad` slot 7 and 9, `Controller` slot 11).

### Minecraft Xbox One Edition: where the input does not come from (leads)

Static and dynamic checks on the older XDK input path (all on `Minecraft.Client.exe`, RVAs):

- The controller interface `9776FF56` is used in 21 places and only two of its methods are ever called: slot 6 (`get_Id`, a `UInt64` compared when finding a controller's record, title+`0x53d604`) and slot 8 (the object kept in the record at `+0xA0` and handed back by title+`0x53c69c` to code that then calls title+`0x5a7130`: a user-like object). The per-controller record is 0xA8 bytes, 8 of them; `+0x98` is the controller, `+0xA0` that object, `+9` an in-use flag, `+0xC` and `+0x14..+0x44` state fields that are only ever *reset* (by title+`0x53cd6c`, `0x53b824` and `0x53d604`); nothing in the title writes them from a reading.
- The element is queried for no interface but `9776FF56` (twice in a run) and no other stand-in method is called when a key is pressed, so the buttons are not read through any object the layer returns.
- Only three input events are registered, all on static factories: `Gamepad` slots 7 and 9 and `Controller` slot 11. The handlers (title+`0x53b824` and the delegate classes around `0x53ae00` to `0x53f400`) QI the *sender* (`082E402C`, `9DE1C534/5`), find the record by id (`0x53d604`) and set up or tear down records; they do not look like button events.
- So either the buttons come in through a path that does not touch WinRT objects the layer implements (a different class activated by a string the title builds, a shared memory block, or a CoreWindow event), or the reading is taken from the object in `+0xA0` once a real user is signed in and the "Choose User" state is left. The `Controllers` poll (`0x53cd6c`) runs fine and registers the controller, so the layer's list is accepted.
- Next to try: raise the registered `Gamepad`/`Controller` events with the element as sender (the handlers call `get_Id` and `slot 8`), check whether pressing X (Choose User) works with a keyboard key mapped through the CoreWindow, and diff the title's calls on a real console trace if one becomes available. `XB1_NO_GAMEPAD=1` and the new `element QueryInterface` log (first 200 queries) help to see what is asked.

### Minecraft Xbox One Edition: raising the input events

- The title registers three delegates on the input statics (`Gamepad` slots 7 and 9, `Controller` slot 11). They are now stored (`INPUT_HANDLERS`) and invoked once, 1.5 s after registration, on a helper thread that has the title's static TLS, with the single controller element as sender and as the answer to any getter on the (generic) arguments. Only done for titles on the older runtime (`vccorlib110`); `XB1_NO_INPUT_EVENTS=1` turns it off. The controller list now hands out that same element.
- The handlers turned out to be about **controller and user pairing**, not buttons: they ask the controller for its id (older-SDK slot 6, answered `1`), its type (slot 7, an HSTRING the title compares with `"Windows.Xbox.Input.Gamepad"` through `WindowsCompareStringOrdinal`; answering a stand-in object there crashed the compare), query `082E402C` (probably `IGamepad`), and read the user's gamertag (slot 9) and id (slot 15, a decimal string).
- The controller's runtime class name is now also `Windows.Xbox.Input.Gamepad`.
- **Still no button input** (Z = A in the layer window does nothing; the prompts still say "Select / Choose User"), no crash. The handlers run; the title never calls a method of `082E402C` afterwards, so a reading must come in some other way. Raising the CoreWindow handlers (`XB1_WINDOW_EVENTS=1`) crashed the process, so that experiment stays off.

### Minecraft Xbox One Edition: input, more negative results

- **The per-frame WinRT traffic is tiny**: `CoreWindow::GetForCurrentThread`, window slot 7 (`get_Dispatcher`), the license query (`IsActive`/`IsTrial`) and controller slot 8 (the user, about 60 calls a second, the count in the earlier log was capped). No reading is requested through any object the layer provides.
- **`IGamepad` is the same layout as the controller interface for slots 6 to 8.** Giving the `IGamepad` (`082E402C`) query result its own object showed the title calls only slots 6 (id) and 8 (user) on it, from the pairing event handler (title+`0x53b8a4`, `0x53b960`, `0x53ba23`). The `Platform::Array<IGamepad^>` the controller list fills is the only place the type appears.
- **The record's state is never written from a reading**: the only writers are zeroing code (poll, add handler, id lookup, static init), plus title+`0x53d94c`, which stores *pointers* into the record (`+0x50..+0x68`, selected by an index and a kind) for four values; nothing in the input area follows those pointers to write, so the producers are in code I have not located (or in another thread).
- **Raising the CoreWindow handlers is harmful**: the single registered window event (slot 19) is `Closed`; invoking it with the layer's window arguments made the title exit cleanly. Do not do that.
- **Open ideas**: gamepad input on this SDK may arrive as `CoreWindow` `KeyDown` events with gamepad `VirtualKey` values (the window registers only one handler so far, so the keyboard events may be registered on another object, for example the dispatcher or an `ICoreInputSource`); or the producers run only once the title is "signed in" past the "Choose User" state. A real console trace, or finding the callers of title+`0x53c0e8`/`0x53c69c` and the readers of the record's pointer fields, would settle it.

### Minecraft Xbox One Edition: input works (the solution)

- **The cause:** one stand-in object answered two different interfaces with one vtable. The older SDK's controller is held as `IGamepad^` (record `+0x98`); the pairing code calls `IController` methods on a *QueryInterface(9776FF56) result* (slot 6 id, 7 type, 8 user) while the per-frame update (title+`0x53e3a8`) calls **slot 8 on the `IGamepad` object itself, which is `GetCurrentReading(GamepadReading*)`**, filling a 40-byte buffer (time, buttons, two trigger floats, four stick floats) that title+`0x53da94` turns into button masks. The layer answered that slot with the user object, so every reading stayed zero.
- **The fix:** two objects. `controller_element()` is the gamepad (`IGamepad`, class `Windows.Xbox.Input.Gamepad (element)`, runtime class name `Windows.Xbox.Input.Gamepad`): the list element and the sender of the events, slot 8 = reading (same `fill_gamepad_reading` as Sonic Mania: XInput, DualSense, keyboard). `controller_view()` is the `IController` face: QueryInterface `9776FF56` returns it, slot 6 id `1`, 7 type, 8 the shared user. QueryInterface `082E402C` returns the gamepad. The pairing event's arguments hand out the `IController` view (`ControllerPairingChangedEventArgs.Controller`), the gamepad events' arguments the gamepad (`GamepadAddedEventArgs.Gamepad`).
- **How it was found:** a hardware write watchpoint on the record (`--watch-sites=0x53d1bf` with `XB1_HWBP="rdi+0x38"`, 8-byte aligned) showed the record rewritten every frame from title+`0x53dd6b` inside the converter at title+`0x53da94`, whose caller loops over the eight records and calls vtable slot `0x40`.
- **Verified:** pressing Down in the layer window moves the highlight from "Play Game" to "Leaderboards", and the "Select / Choose User" prompts disappear once the pairing events ran. Pressing A on "Play Game" was tried once and the menu stayed (not investigated: it may be loading or waiting for a signed-in user).

### Minecraft Xbox One Edition: Play Game (first steps)

Pressing A on "Play Game" (hold the key for about 200 ms; a 1 ms `SendKeys` press can fall between two per-frame polls) now runs the title's sign-in and Xbox Live code. What it needed, in order:

- `ConnectedStorageSpace` statics slot 6 is `GetForUser(user, ConnectedStorageSpace** out)` (a factory, not an event add; the token write over the out pointer crashed the title at `0x5a5c9c`).
- `IUser` booleans (older SDK) polled every frame: **slot 10 is `IsGuest` (false), slot 11 is `IsSignedIn` (true)**. The first order produced the dialog "Guest Xbox profiles cannot access this feature", the swapped one "You are not signed in"; both dialogs render, with the menu text drawn through them (depth or blend order not yet right).
- Xbox Live services operations start as `(a, b, c, int, Operation** out)` with the out pointer in the *fifth* argument (stack); the returned object gets a completion handler through slot 6 and is completed at once.
- **Heap corruption at about 50 seconds** (`0xC0000374` in the allocator): the two helper threads that raise user and input events ended after 200 iterations while holding the title's TLS array (installed with `ensure_tls_on_current_thread`); the thread teardown then freed memory it did not own. They now loop until the process ends (`restore_loader_tls()` exists for title threads). `Microsoft.Xbox.Services.Presence.PresenceData` slot 6 also receives a pointer into a heap object (a smaller field), so out-parameters not on the thread's stack are left alone for that class (`XB1_NO_HEAP_OUT=1` does it for every class).
- `GamepadAddedEventArgs`/`ControllerPairingChangedEventArgs` getters now both return the `IController` view (the handler calls `get_User` through it, and a 36-byte reading written over an 8-byte local was the earlier stack corruption).
- **Where it stops:** the title walks the downloadable-content package list (`Packages` queried for `356E4A78`, `GetAt` called with a garbage index) and crashes at title+`0x30ed1e` reading a NULL iterator/object: the package-list stand-in has the wrong shape (the call at title+`0x30ecda` is slot 6 with one argument, probably `First()` of an `IIterable`, not an indexed `GetAt`).

### Minecraft: Play Game reaches Start Game / Join Game

- The title crash at `title+0x30ed1e` was not the DLC package list. The loop at `0x30ec99` iterates the result collection of an Xbox Live asynchronous operation: slot 7 is `get_Size(uint*)`, slot 6 `GetAt`. The generic stand-in wrote an object pointer into the size, so the loop ran with a garbage size and called `GetAt` on a NULL element. Slot 7 of those results (`XboxLiveContext#...#slot8`) now writes 0 (verified: crash gone).
- The DLC call is `(filter, IVectorView** out)`, two arguments (verified by logging the arguments: `arg1 = 3`, `arg2` = a stack pointer). An earlier guess that it was a one-argument `First()` was wrong; no iterable stand-in is used for it, though one exists (`IIterable`/`IIterator`, empty) for the `356E4A78` interface if a title asks for it.
- Next crash, in `d3d11.dll` (call at `title+0x2c1fce`): the title calls slot 8 of a depth-stencil state, which is `GetDesc` on the older SDK (desktop slot 7). `restore_object_vtable` put the original 8-slot vtable back after the first bind, so slot 8 ran into data. State objects now get their extended vtable back after each bind and slot 8 is the desktop `GetDesc` (older SDK only).
- Result: pressing A on Play Game shows the Start Game / Join Game screen with background, title, prompts and the user name. The two panels' fills are wrong (large grey triangles and two squares where a world list and its icons should be), so some mesh or texture path of that screen is still unmapped. No world was entered yet.

### Minecraft: Start Game panels fixed (index buffer offset)

- Cause of the grey V-shaped fills: console index 18 is `IASetIndexBuffer(format, buffer, byte offset)` and the layer passed offset 0. Minecraft keeps many meshes in one index buffer and binds each at a byte offset (logged: 2700-byte buffer, draws of 663, 255, 228, 204 indices at offsets 0, 0x52e, 0x72c, 0x8f4, i.e. 16-bit indices back to back), so every mesh after the first read the wrong indices. The offset is now passed (verified: both panels draw as the real frames).
- Format constants: desktop 57 is `R16_UINT`, 42 `R32_UINT`; console format 0 is 16-bit (the layer already mapped it so; an attempt to "fix" it to 32-bit turned the whole picture black).
- Indexed draws now set a triangle list on the older SDK, because the plain 4-vertex `Draw` leaves a triangle strip behind. Not what caused the panels, but it was wrong.
- `+0x740` (index 104) is a call with only `this`, followed by a dirty-flag update in the title; it is log-only and harmless so far.
- Debug aids: `XB1_TRACE_FRAME=n` starts `XB1_CTX_TRACE` at the n-th Present, `XB1_DRAW_TRACE=n` logs the first n draws from there with the bound index buffer size, format and offset.
- Still wrong on that screen: the two small squares in each panel (probably unloaded or unmapped textures); not yet looked at.

### Minecraft: Start Game waits on the storage API

- The four squares in each panel are the loading spinner, not missing textures. It never finished because the title's save-game storage chain (4J's wrapper over `Windows.Xbox.Storage.ConnectedStorageSpace`) stalled.
- Call-site evidence (title `0x5a6263`): the object `GetForUser` returns is an asynchronous operation; the title calls its slot 6 with one handler argument, which is `put_Completed`. The layer took it for an event add, so the handler was never invoked. Storage objects' slot 6 with a layer-owned second register (the vtable pointer left there by a one-argument call) is now completed immediately; verified: the title goes on to use the result.
- The next calls are `(name, Object** out)` with a string that is not always recognizable as an HSTRING header; the string pointer was being treated as the out parameter and the real one left NULL (crash at `0x59c4c8`). Storage objects' slot 9 now writes the stand-in to the second argument.
- Current stop: crash at `title+0x53be0e`, in the title's input mapping code (`mov edx,[rcx+rdx*4]` with `rcx = table[3]` garbage), reached right after the storage chain. Not yet known whether a layer write corrupted that table or the table is never filled for a fourth pad; to check next.
- The storage objects are still generic stand-ins whose shapes are guessed from call sites; the result of `GetForUser`'s operation (the real space) and the container/blob calls need proper semantics (and a place to keep saves) for worlds to be created and listed.

### Minecraft: the input-table crash is bad profile settings

- The crash at `title+0x53be0e` reads `table[3]` of an input-mapping table that has 3 entries (count byte at `+0x585` of the input manager at `title+0xa74320`). The index comes from a mapping byte stored at `+0x548 + pad` by a setter at `0x53d8f4` (hardware write watch, `XB1_HWBP="rbx+0x548"` at a watch site on the reading instruction).
- The setter is called from the settings-changed switch at `0x7ce90` (case at `0x7d08a`) as `(settings[6] >> 4) & 3`, where `settings = [this + pad*8 + 0x1f8]` is the per-player profile settings block. A value of 3 is not a valid mapping, so that block holds data that was never initialized or loaded (the title does not apply its defaults), not a mapping the layer wrote.
- The block is the game's saved profile, which comes from the storage API whose operations are still stand-ins that complete without any data. Next step: find the profile read (container/blob read after the `(name, out)` calls) and either return "no data" in the shape the title handles (so it applies defaults) or back it with a real file store.
- Debug aid: the `XB1_HWBP` report now also lists title addresses found on the raw stack, because the unwinder stops in leaf thunks.

### Minecraft: the Start Game list works

- Start Game now shows its list: Create New World, Play Tutorial and the Halo, Mass Effect and Skyrim worlds (title-bundled), with "No Games Found" on the Join side. No exceptions in a 30 s run (verified by screenshot).
- The input-table crash was not profile data after all: the settings block held plausible values (`00 ea ea ff 68 54 ...`, mapping 1); the value 3 came from a thread race. The title expects an async operation's completion handler to run after `put_Completed` returns, on another thread; calling it inline let other threads read half-initialized state (several different crashes depending on timing). Handlers of storage operations now run on a short-lived thread 5 ms later (`complete_later`; `XB1_ASYNC_INLINE=1` goes back to inline).
- More storage shapes: `(name, out)` with a NULL (empty) name is also slot 11, not only 9; the result of a storage query is a collection whose slot 7 is `get_Size`, now 0 (nothing saved yet). `XB1_STORAGE_TRACE=1` logs every storage stand-in call with its title call site.
- Views created through the older SDK's `CreateShaderResourceView` and its siblings are now given a vtable shifted by one slot from 8 on, in place, since the console view has one more method before `GetDesc`. Before, the title's `GetResource` (console slot 8) ran the desktop `GetDesc`, returned nothing and crashed at `0x2b8f14`.
- Not done: nothing is saved (the storage stand-ins hold no data), Create New World not tried.

### Minecraft: Create New World opens its setup screen

- Create New World now reaches the setup screen (World Name, Game Mode, Difficulty, Texture Packs, Online Game, More Options, Create New World), verified by screenshot, no exception in the run.
- Fixes needed on the way: storage operations started as `(object, 0, Operation** out)` return their operation through the third argument (call site `0x5a8eee`); `Windows.Xbox.System.User` objects' `(NULL name, out)` calls (`0x2ca99a`) write the object to the second argument; `SystemUI` slot 11 (`0x539859`, the system keyboard) returns its operation through the fifth argument and its result (slot 8) is an HSTRING, now the default world name "New World".
- Startup crashes at `0x53be0e` and `0x2caa0e` appeared in roughly one run in six before the fixes above and did not occur in 12 later runs (6 with a 5 ms and 6 with a 300 ms async completion delay), so they are timing dependent and the delay is not their cause. Not fully understood.
- Debug aids: `XB1_NW_TRACE=1` prints the title call site of generic stand-in calls that write nothing; `XB1_ASYNC_DELAY_MS` sets the async completion delay.
- Not tried yet: pressing Create New World (the world itself).

### Minecraft: pressing Create New World (partial)

- Two more fixes found on the way to the world: an out-parameter that is not pointer-aligned is a value getter (`uint*`, `bool*`), so the generic stand-in writes 0 there instead of an object pointer (a misaligned pointer write panicked the layer: "misaligned pointer dereference"); `ChatManager` session slot 23 is a collection the title walks with `get_Size`/`GetAt` (loop at `0x2cc790`) and is now an empty vector view (before, the unwritten size made the loop run on garbage until the C runtime crashed at `ucrtbase+0xee2bb`).
- Not confirmed: after these fixes the sequence was interrupted by an unrelated startup crash (`0x53be0e`) and then by the machine being in use, so what Create New World does next has not been seen. Sonic Mania was not re-run after these two changes.
- The startup crash did not occur in 28 plain runs (waits of 13 to 25 s, with and without `XB1_NO_GAMEPAD`); it has only appeared in interactive sessions, so it is not tied to the pad reader.

### Minecraft: pressing Create New World, current stop

Pressing the button (verified by log, no screenshot of the result) now goes through these steps, each fixed in turn:
- the user's display information (`User` slot 7, then slot 6) is read as strings (slots 6, 8, 9, 10; slot 7 is a number, the gamerscore): they now return HSTRINGs; before, an object there crashed the C runtime's copy in a string concatenation (`ucrtbase+0xee2bb`, call site `0x2cae55`);
- the object `User` slot 7 returns is a one-item collection: slot 7 is `get_Size` (now 1), slot 6 `GetAt`; before, the size was an object pointer and the loop at `0x2ca99a` ran past the end (`0x2caa0e`);
- a storage container's slot 6 with `(object, 0, out)` starts an operation (call site `0x59ce2f`), and a storage operation's `put_Completed` may arrive with a NULL second register.

Current stop: crash at `title+0x2d4a86` (`mov r8,[rcx]` with `rcx = [wrapper+0x20]` NULL), in the title's continuation helper (`0x2d4a00`) that wraps a storage operation and hooks a handler on it. The operation it wraps is NULL, so some earlier storage call that should have returned an operation (or whose out pointer is elsewhere) returned nothing. The operation-start fix above fires (logged) and does not change this, so the missing operation comes from another call. Next: trace every storage call from the container creation to this point with `XB1_STORAGE_TRACE=1` and `XB1_NW_TRACE=1` and compare with the call sequence in the title around `0x59ce2f`.
Sonic Mania was last verified before the string fixes (3 Present1, no exceptions); it has not been re-run after them.

### Minecraft: a world starts and renders in 3D

- The crash at `0x2d4a86` came from the wrong out pointer: the container's slot 6 (call site `0x59ce2c`) is `(object, object, handle, 0, Operation** out)`, the out pointer being the fifth argument on the stack, not the third register. With it written, the title continues: a world loads and renders (verified by two screenshots 35 s apart and a run of 2791 draw calls per frame): terrain, sky, clouds, the player's hand, the inventory screen and a tutorial hint. It was the Play Tutorial world, because the key script's down presses landed on that entry; Create New World itself was not separately confirmed.
- The one exception in that run is `0xe06d7363` (a C++ throw inside `KERNELBASE`), apparently handled by the title; the run kept going.
- The `0x53be0e` crash only happens after the first key press (the controller mapping table is read when a button is pressed) and depends on a settings byte that differs from run to run, which is why plain runs without input never crash. It appeared in about one third of the interactive runs; the test script retries when it happens. Its cause is still not understood.
- Not checked yet: whether the picture is correct in detail (the player skin has a black background in the inventory, the sky and terrain look right), movement and camera with the controller, saving, and long runs.

### Minecraft: Create New World works; first-press crash fixed; FPS in the world

- **Create New World now loads a world and plays** (verified: a jungle and a hills world with sky, terrain, grass, the HUD with hearts, hunger and hotbar; the game also showed the "Preparing to Save Level" screen first and got through it). The crash on that path was `Texture2D::GetDesc`: on the older SDK it is console slot 12, and the layer only logged the call, so the title built its next texture description from uninitialized memory and `CreateTexture2D` failed (E_INVALIDARG, crash at `0x56022c`). Slot 12 now calls the real `GetDesc`. (Shifting slots 8 to 10 as well broke startup, because the layer itself calls slot 10 as `GetDesc` on these objects; they stay as they were.)
- **The first-button-press crash at `0x53be0e` is fixed** (0 crashes in 10 runs on the release build; it had hit about half of them): the title reads a per-player settings block that it expects to be zeroed. The block is read before it is filled, and desktop heap memory is reused and dirty (captured: all zero with one default byte in four runs, leftover bytes such as `7d` in the run that crashed: `(byte[6] >> 4) & 3 = 3`, past the end of the 3-entry mapping table). The layer now zeroes the blocks that the older runtime's `operator new`, `operator new[]` and `malloc` hand out (`xb_operator_new`, `xb_malloc`), as the console does.
- A regression of mine: slot 9 on the first storage helper object (`...#slot6#slot8#slot6`) uses its first argument as the out pointer; the `(name, out)` rule wrote to the second one, which happened to look writable on a release build, leaving NULL and crashing every first press. That object is excluded.
- **FPS in the world** (release build, from the timestamps in the `[gpu] frame N at Ts` log lines, 300 frames each): about 23 FPS standing still and 14 moving at roughly 2,500 to 3,300 draws per frame; after replacing per-call work (wrapped views now cached per object with one vtable per kind instead of 8 KB of thunks per call; the wrapper and created-kind lookups are hash maps instead of linear scans; two environment lookups per draw cached) about 41 and 32 standing and 22 to 25 moving at about 3,400 draws per frame. The menus run at 60. Still below 60; the debug build is much slower than release (the reported 5 FPS was on a debug build). Not profiled further yet: per-draw state and topology calls, the deferred-context serialization, and the D3D11 driver cost of 3,400 draws are the remaining suspects.
- Controller: the keyboard stick keys added for testing (I/K/J/L left stick, numpad 8/5/4/6 right stick, E and R triggers) change the camera/position in the world. A DualSense user reported that the left stick does not rotate the camera (in this game the right stick looks and the left moves; not clarified which stick does nothing for them): `XB1_DS_LOG=1` now prints the DualSense sticks and the raw bytes when one moves, to check which stick lands on which axis.

### Minecraft: the camera does not rotate because the player settings are never initialized

- Reported: the right stick (and the pad's "left" one, which the layer reads correctly) did not rotate the camera in the world. Checked with `XB1_DS_LOG=1`: the DualSense reports both sticks on the right axes (left at bytes 2 and 3, right at bytes 4 and 5 of the Bluetooth report, triggers 6 and 7), so input reaches the title. The keyboard stick keys moved the player (left stick) but the numpad (right stick) did not rotate the camera either, so the cause is in the game's state, not in the pad.
- The per-player settings block (at `[options + pad*8 + 0x1f8]`, applied by `0x7e5d0` through `0x7ce90`) is all zero apart from one byte (`[7] = 0x96`) at that point: the profile defaults are never applied, because the storage stand-ins return no profile and the title's default path does not run (or runs and leaves zeros). The camera's look sensitivity is one of the bytes (cases at `0x7e5d0`: bytes 1, 2, 4 and others scaled into options). Before the zeroed `operator new`, the same block held leftover heap bytes, which is why it sometimes worked by luck.
- Verified experiment: writing `0x80` to bytes 1 to 4 of the block when `0x7e5d0` is entered (`XB1_POKE="rcx+0x1f8>1=80;rcx+0x1f8>2=80;rcx+0x1f8>3=80;rcx+0x1f8>4=80"` with `--watch-sites=0x7e5d0`; `XB1_POKE` now follows a pointer with `reg+off>off2=value`) makes the same right-stick key rotate the camera (before and after screenshots differ clearly; the control prompts also appear).
- Not done: which bytes are which options, and the real fix (finding the default-options function and why it is not reached, or persisting the profile in a store the title can read back). The poke above is a per-build workaround, not a fix.

