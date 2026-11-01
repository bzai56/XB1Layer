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

1. `CoreApplication::Run` â†’ `IFrameworkViewSource::CreateView` succeeds â†’ `IFrameworkView::Initialize` is entered and runs for a long time. `SetWindow`, `Load`, the `Activated` event and `Run` have not been reached yet.
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

## Where the run ends now
The title runs and its state is mostly set up, then it faults either in the user stand-in (above) or occasionally inside the GPU driver's own worker thread (`nvwgf2umx.dll`), which means something handed to Direct3D is corrupting driver state. That second fault is not understood.

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

## Tools added
`--watch-sites=0x...,0x...` plants one-shot breakpoints at title RVAs (report registers, text pointed at by the argument registers, and the title's call chain, then continue); `--watch-repeat` keeps them armed (re-arming by single-step) and prints one compact line per hit for the title's logger; `--patch-fastfail` was added earlier. The device's standard creation methods now log failures with a dump of the description and, for views, the resource's own type and description.

## Next investigation steps
1. Decode the command packet stream (the flush dumps), starting with the packet header layout and the common opcodes.
2. ~~Give the Xbox `User` element real methods~~ Shell-layer faults fixed (strings, events); the meaning of the user slots is still inferred.
3. Find what corrupts the GPU driver's thread, and what ends the run with `0x87D` under the debug layer. Use `XB1_D3D_DEBUG=1` and fix each message.
4. Texture data: decode the console tiling and mirror placement textures.
5. The remaining decoding tasks listed earlier (`+0xA08`, `+0x4e0`, which context slot the console table lacks).
6. Pin down the console `ICoreWindow` layout: keep logging slots as the title uses more of them, and give stand-in behavior to a slot only where the call pattern justifies it.
7. Implement `DXGIXSetVLineNotification`, `DXGIXPresentArray` and the display/swap-chain path (the title's render-target view creation still fails once on a NULL back buffer).
8. Work out the real shape of the `PackageTransferManager` interfaces and the `42A6A529` / `9FF3799E` `CoreApplication` extensions.
9. Provide a `G:` equivalent and inspect `media/` (encrypted content is out of scope).
10. Back the window with a native `HWND` and raise `CoreWindow` events.
