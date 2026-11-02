//! Graphics: the console's D3D11.X, mapped onto desktop Direct3D 11.
//!
//! The console ships its own `d3d11_x.dll`, whose device and context interfaces
//! (`ID3D11DeviceX`, `ID3D11DeviceContextX`) extend the desktop ones with console-only
//! methods; the title's last fault was a call to vtable slot 321 of the context, far past the
//! ~115 slots of the desktop `ID3D11DeviceContext`.
//!
//! The title therefore gets proxy objects with long vtables:
//! - the standard slots (`ID3D11Device` 3..=42, `ID3D11DeviceContext` 3..=114) forward to the
//!   real desktop object through a tail-call thunk that swaps `this` and jumps, so every
//!   argument register and stack argument is preserved;
//! - the slots beyond that are the console's extensions. Their layout is unknown, so each one
//!   logs its slot number and returns 0 (S_OK / NULL / nothing).
//!
//! This assumes the console interfaces keep the desktop layout for the standard slots and add
//! their own methods after it. That is unverified; the logs show whether it holds.

use std::ffi::c_void;
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::core::*;

macro_rules! glog {
    ($($t:tt)*) => { crate::raw_log(format_args!($($t)*)) };
}

unsafe extern "system" {
    fn VirtualAlloc(addr: *mut c_void, size: usize, ty: u32, prot: u32) -> *mut c_void;
}

/// `ID3D11Device` has slots 0..=42; `ID3D11DeviceContext` has slots 0..=114.
const DEVICE_STANDARD_SLOTS: usize = 43;
const CONTEXT_STANDARD_SLOTS: usize = 115;
/// Total vtable length of the proxies. Calls beyond the standard slots hit logging stubs.
const PROXY_SLOTS: usize = 600;

const DEVICE_GET_DEVICE_SLOT_IN_CONTEXT: usize = 3;
const DEVICE_CREATE_DEFERRED_CONTEXT: usize = 27;
const DEVICE_GET_IMMEDIATE_CONTEXT: usize = 40;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Device,
    Context,
}

/// Number of 8-byte slots in the proxy's trailing region (see `Proxy::embedded`).
const EMBED_SLOTS: usize = 2048;
/// Offset in the object where `embedded` starts.
const EMBED_BASE: usize = 0x18;
/// Embedded function pointers are filled from this offset up. The title calls pointers at
/// offsets such as 0x438, 0x498, 0x5B8, 0x5D8 and 0xA08, and also reads and writes state
/// fields (0x168, 0x180, 0x390) in the same object, so every slot is filled; a slot that is
/// really data just holds a harmless pointer value.
const EMBED_FILL_FROM: usize = 0x3E0;

/// The context object's command-buffer cursor and end fields (title code writes GPU command
/// packets at the cursor and calls `+0xA68` for more space when it reaches the end).
const CMD_CURSOR: usize = 0x50;
const CMD_END: usize = 0x58;
const CMD_GROW: usize = 0xA68;
const CMD_BUFFER_BYTES: usize = 4 << 20;

/// A proxy for a desktop D3D11 device or context. The forwarding thunks read `real` at offset 8.
///
/// The title does not only call methods through the vtable: it also calls function pointers
/// stored inside the object itself (`call [obj + 0xA08]`), which is how the console's
/// low-overhead immediate context exposes some of its methods. `embedded` makes the object big
/// enough that such reads land on logging stubs instead of past the end of the allocation.
#[repr(C)]
struct Proxy {
    vtbl: *const usize,
    real: *mut c_void,
    refs: AtomicU32,
    kind: Kind,
    embedded: [usize; EMBED_SLOTS],
    /// Start of this context's command buffer (contexts only).
    cmd_base: usize,
}

static CMD_FLUSHES: AtomicU32 = AtomicU32::new(0);

/// `+0xA68`: called when the title has filled the command buffer. For now the contents are
/// logged (the first flushes, with a dump of the first bytes) and the cursor is reset to the
/// start, which discards the packets; interpreting them is the next step.
unsafe extern "system" fn context_grow_command_buffer(this: *mut c_void) -> usize {
    let p = unsafe { &mut *(this as *mut Proxy) };
    let cursor = p.embedded[(CMD_CURSOR - EMBED_BASE) / 8];
    let used = cursor.saturating_sub(p.cmd_base);
    let n = CMD_FLUSHES.fetch_add(1, Ordering::Relaxed);
    if n < 4 {
        glog!("[gpu] command buffer flush #{n}: {used} bytes at {:#x}; first bytes: {}", p.cmd_base, safe_dump(p.cmd_base as u64, 96));
    }
    if n == 0 && used >= 56 {
        // The second packet carries a 64-bit address; say what it is.
        let d = p.cmd_base as *const u32;
        let addr = unsafe { (*d.add(10) as u64) | ((*d.add(11) as u64) << 32) };
        let mut module: *mut c_void = ptr::null_mut();
        let mut name = String::from("(no module)");
        unsafe {
            if crate::GetModuleHandleExA(0x6, addr as *const i8, &mut module) != 0 {
                let mut buf = [0u8; 260];
                let len = crate::GetModuleFileNameA(module, buf.as_mut_ptr(), 260) as usize;
                name = format!("{}+{:#x}", String::from_utf8_lossy(&buf[..len]).rsplit('\\').next().unwrap_or(""), addr as usize - module as usize);
            }
        }
        glog!("[gpu] packet address {addr:#x}: {name}");
    }
    // XB1_DUMP_CMD=<dir> writes the first few flushed buffers there for offline decoding.
    if n < 16 {
        if let Some(dir) = std::env::var_os("XB1_DUMP_CMD") {
            let bytes = unsafe { std::slice::from_raw_parts(p.cmd_base as *const u8, used) };
            let path = std::path::Path::new(&dir).join(format!("cmd_flush_{n}.bin"));
            let _ = std::fs::write(&path, bytes);
            glog!("[gpu] wrote {} bytes to {}", used, path.display());
        }
    }
    p.embedded[(CMD_CURSOR - EMBED_BASE) / 8] = p.cmd_base;
    p.cmd_base
}

/// Offset in the context object where the title's calls suggest a copy of the desktop
/// `ID3D11DeviceContext` method table begins: slot `n` is at `EMBED_TABLE + 8n`.
///
/// Evidence (from logged calls, not documentation): `+0x450` and `+0x458` are called back to
/// back with a real D3D11 resource, subresource 0 and 4 (= `D3D11_MAP_WRITE_DISCARD`), which is
/// `Map`/`Unmap` (desktop slots 14 and 15); `+0x438` and `+0x498` are called with a NULL first
/// argument in a "reset shaders" sequence, matching `VSSetShader` and `GSSetShader` (slots 11
/// and 23). All four agree with a table base of `0x3E0`.
const EMBED_TABLE: usize = 0x3E0;

/// Embedded indices below 24 whose desktop meaning is backed by logged calls.
const EMBED_VERIFIED: [usize; 4] = [7, 11, 14, 15];

/// Highest desktop slot forwarded through the embedded table (`GSSetShader`).
const EMBED_FORWARD_MAX: usize = 23;

/// The console table is one slot behind the desktop one from index 34 on, so it lacks one method
/// somewhere between desktop slots 24 and 34. Evidence (logged calls, by console index):
/// 34 = `OMSetBlendState` (state, NULL, 0xFFFFFFFF), 46 = `CopyResource` (two resources),
/// 49 = `ClearRenderTargetView` (view, pointer to zeros), 59 and 63 = the hull and domain
/// `SetShader` calls in the NULL-argument reset sequence. Indexes 24 to 33 are not mapped
/// because it is not known which of them is the missing method.
const EMBED_SHIFTED_FROM: usize = 34;
/// Last console index forwarded (desktop `FinishCommandList` is 114, so console 113).
/// Only indexes up to 63 have direct evidence; the rest is extrapolated.
const EMBED_SHIFTED_TO: usize = 63;
/// Shifted console indices backed by logged calls: 34 `OMSetBlendState`, 46 `CopyResource`,
/// 49 `ClearRenderTargetView`, 59 and 63 the hull and domain `SetShader` (NULL). The debug layer
/// showed the rest of 34 to 63 receiving garbage (`RSSetViewports`, `ExecuteCommandList`).
const EMBED_VERIFIED_SHIFTED: [usize; 5] = [34, 46, 49, 59, 63];

/// Explicit `(console index, desktop slot)` pairs seen in Sonic Mania's per-frame calls (arguments
/// logged, not guessed): 8 is called as (shader, 0, ...) so it is `PSSetShader` (desktop 9);
/// 10 `PSSetSamplers(0,1,&s)`, 12 `DrawIndexed(54,..)`, 16 `PSSetConstantBuffers`,
/// 18 `IASetVertexBuffers(0,1,&b)`, 19 `IASetIndexBuffer(buf,fmt,0)`; from 32 the console index is
/// one behind: 32 `OMSetRenderTargets(1,&rtv,dsv)`, 35 `OMSetDepthStencilState(state,1)`,
/// 42 `RSSetState`, 43 `RSSetViewports(1,&vp)`; console 52 (offset +0x580) is
/// `ClearDepthStencilView(dsv,3,..)` = desktop 53, so the shift is a constant one from 32 on.
const EMBED_EXTRA: [(usize, usize); 11] =
    [(8, 8), (19, 19), (10, 10), (12, 12), (16, 16), (18, 18), (32, 33), (35, 36), (42, 43), (43, 44), (52, 53)];

/// `(embedded index, entry)` pairs that forward to the real context. Built once.
fn forwarded_entries() -> &'static Vec<(usize, usize)> {
    static ENTRIES: OnceLock<Vec<(usize, usize)>> = OnceLock::new();
    let entries = ENTRIES.get_or_init(|| {
        // XB1_LOG_ALL=1: forward nothing, so every embedded function logs its raw arguments.
        let log_all = std::env::var_os("XB1_LOG_ALL").is_some();
        let thunks = unsafe { VirtualAlloc(ptr::null_mut(), 16384, 0x3000, 0x40) } as *mut u8;
        assert!(!thunks.is_null(), "thunk allocation failed");
        let mut v = Vec::new();
        let index_of = |offset: usize| (offset - EMBED_BASE) / 8;
        // Titles of the older XDK (Visual C++ 2012 runtime) have another embedded table: it is two
        // slots behind the desktop one at the indices seen so far. Only evidenced indices forward.
        if crate::VCCORLIB110.load(Ordering::Relaxed) {
            if !log_all {
                for (console_slot, desktop_slot) in OLD_SDK_EXTRA.iter().copied() {
                    let thunk = forward_context_thunk(unsafe { thunks.add((128 + console_slot) * 32) }, desktop_slot);
                    v.push((index_of(EMBED_TABLE + console_slot * 8), thunk));
                }
                v.push((index_of(EMBED_TABLE + 13 * 8), ctx_map_old as usize));
                v.push((index_of(EMBED_TABLE + 11 * 8), ctx_draw_indexed as usize));
                v.push((index_of(EMBED_TABLE + 18 * 8), ctx_bind_object as usize));
                v.push((index_of(EMBED_TABLE + 42 * 8), ctx_viewports as usize));
                v.push((index_of(EMBED_TABLE + 33 * 8), ctx_state_old::<35> as usize));
                v.push((index_of(EMBED_TABLE + 34 * 8), ctx_state_old::<36> as usize));
                v.push((index_of(EMBED_TABLE + 41 * 8), ctx_state_old::<43> as usize));
                v.push((index_of(EMBED_TABLE + 51 * 8), ctx_clear_dsv_old as usize));
                v.push((index_of(EMBED_TABLE + 43 * 8), ctx_scissor_old as usize));
                v.push((index_of(EMBED_TABLE + 12 * 8), ctx_draw_old as usize));
                v.push((index_of(EMBED_TABLE + 31 * 8), ctx_om_set_render_targets_old as usize));
                v.push((index_of(EMBED_TABLE + 87 * 8), ctx_om_get_render_targets_old as usize));
            }
            return v;
        }
        for slot in 0..=EMBED_FORWARD_MAX {
            // Only indices with direct evidence forward: 11 and 23 (SetShader with NULL), 14 and 15
            // (Map/Unmap). Index 8 was assumed to be PSSetShaderResources and is called as
            // (pointer, 0, 0x76b40), so the console table is not a plain copy of the desktop one.
            if slot > 3 && ((log_all && slot != 14 && slot != 15) || !EMBED_VERIFIED.contains(&slot)) {
                continue;
            }
            let entry = match slot {
                0 => proxy_qi as usize,
                1 => proxy_add_ref as usize,
                2 => proxy_release as usize,
                3 => context_get_device as usize,
                8 => ps_set_shader_resources as usize,
                s => forward_context_thunk(unsafe { thunks.add(s * 32) }, s),
            };
            v.push((index_of(EMBED_TABLE + slot * 8), entry));
        }
        for console_slot in EMBED_SHIFTED_FROM..=EMBED_SHIFTED_TO {
            // Only indices with direct evidence forward (see EMBED_VERIFIED_SHIFTED); the others log
            // their arguments so they can be identified from the calls.
            if log_all || !EMBED_VERIFIED_SHIFTED.contains(&console_slot) {
                continue;
            }
            let thunk = forward_context_thunk(unsafe { thunks.add((64 + console_slot - EMBED_SHIFTED_FROM) * 32) }, console_slot + 1);
            v.push((index_of(EMBED_TABLE + console_slot * 8), thunk));
        }
        for (console_slot, desktop_slot) in EMBED_EXTRA.iter().copied().filter(|_| !log_all) {
            if console_slot == 8 {
                v.push((index_of(EMBED_TABLE + 8 * 8), ctx_ps_srv as usize));
                continue;
            }
            if console_slot == 43 {
                v.push((index_of(EMBED_TABLE + 43 * 8), ctx_viewports as usize));
                continue;
            }
            if console_slot == 12 {
                v.push((index_of(EMBED_TABLE + 12 * 8), ctx_draw_indexed as usize));
                continue;
            }
            if console_slot == 18 {
                v.push((index_of(EMBED_TABLE + 18 * 8), ctx_ia_vb as usize));
                continue;
            }
            if console_slot == 19 {
                v.push((index_of(EMBED_TABLE + 19 * 8), ctx_bind_object as usize));
                continue;
            }
            let thunk = forward_context_thunk(unsafe { thunks.add((128 + console_slot) * 32) }, desktop_slot);
            v.push((index_of(EMBED_TABLE + console_slot * 8), thunk));
        }
        // Draws and dispatches first mirror the placement buffers (see sync_before_draw).
        let synced = unsafe { VirtualAlloc(ptr::null_mut(), 4096, 0x3000, 0x40) } as *mut u8;
        assert!(!synced.is_null(), "thunk allocation failed");
        for (n, desktop_slot) in DRAW_SLOTS.iter().enumerate().filter(|_| !log_all) {
            let console_slot = if *desktop_slot > EMBED_FORWARD_MAX { desktop_slot - 1 } else { *desktop_slot };
            let target = index_of(EMBED_TABLE + console_slot * 8);
            let thunk = synced_forward_thunk(unsafe { synced.add(n * 128) }, *desktop_slot);
            if *desktop_slot == 12 {
                continue;
            }
            if let Some(entry) = v.iter_mut().find(|(i, _)| *i == target) {
                entry.1 = thunk;
            }
        }
        v
    });
    FORWARDED_INDEXES.get_or_init(|| entries.iter().map(|(i, _)| *i).collect());
    entries
}

/// Embedded indexes that forward to the real context, so state reports can skip them.
static FORWARDED_INDEXES: OnceLock<Vec<usize>> = OnceLock::new();

/// Allocates a zeroed proxy and initializes it. `embedded` stubs are filled only for contexts.
fn new_proxy(real: *mut c_void, kind: Kind) -> *mut Proxy {
    let layout = std::alloc::Layout::new::<Proxy>();
    let p = unsafe { std::alloc::alloc_zeroed(layout) } as *mut Proxy;
    assert!(!p.is_null(), "proxy allocation failed");
    unsafe {
        (*p).vtbl = vtable(kind);
        (*p).real = real;
        (*p).refs = AtomicU32::new(1);
        (*p).kind = kind;
        if kind == Kind::Context {
            for (i, slot) in embedded_stubs().iter().enumerate() {
                if EMBED_BASE + i * 8 >= EMBED_FILL_FROM {
                    (*p).embedded[i] = *slot;
                }
            }
            // The entries we understand forward to the real context instead of logging.
            for (index, entry) in forwarded_entries() {
                (*p).embedded[*index] = *entry;
            }
            // Embedded function +0x6a0 (title call site 0x1e3a06c): two objects through out-pointers.
            (*p).embedded[(0x6A0 - EMBED_BASE) / 8] = context_ext_212 as usize;
            // A command buffer for the packets the title writes into the context, and the
            // function it calls when the buffer is full.
            let buffer = VirtualAlloc(ptr::null_mut(), CMD_BUFFER_BYTES, 0x3000, 0x04) as usize;
            assert!(buffer != 0, "command buffer allocation failed");
            (*p).cmd_base = buffer;
            (*p).embedded[(CMD_CURSOR - EMBED_BASE) / 8] = buffer;
            (*p).embedded[(CMD_END - EMBED_BASE) / 8] = buffer + CMD_BUFFER_BYTES;
            (*p).embedded[(CMD_GROW - EMBED_BASE) / 8] = context_grow_command_buffer as usize;
            CONTEXT_PROXIES.lock().unwrap().push(p as usize);
        }
    }
    p
}

/// Every context proxy created, so their command buffers can be inspected while they are partly full.
static CONTEXT_PROXIES: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

/// Writes the used part of every context's command buffer to `<dir>/ctx<N>_<tag>.bin` (XB1_DUMP_CMD).
pub fn dump_all_contexts(dir: &std::path::Path, tag: &str) {
    let list = CONTEXT_PROXIES.lock().unwrap().clone();
    for (i, p) in list.iter().enumerate() {
        let p = unsafe { &*(*p as *const Proxy) };
        let cursor = p.embedded[(CMD_CURSOR - EMBED_BASE) / 8];
        // The title swaps in new buffers through the grow function, so measure from the current end.
        let end = p.embedded[(CMD_END - EMBED_BASE) / 8];
        let start = end.saturating_sub(CMD_BUFFER_BYTES);
        let used = cursor.saturating_sub(start);
        glog!("[gpu] context {i}: cursor {cursor:#x} end {end:#x} cmd_base {:#x} used {used}", p.cmd_base);
        if used == 0 || used > CMD_BUFFER_BYTES {
            continue;
        }
        let bytes = unsafe { std::slice::from_raw_parts(start as *const u8, used) };
        let _ = std::fs::write(dir.join(format!("ctx{i}_{tag}.bin")), bytes);
        glog!("[gpu] context {i}: {used} bytes of commands dumped ({tag})");
    }
}

static EMBEDDED_STUBS: OnceLock<Vec<usize>> = OnceLock::new();

/// One argument-logging stub per embedded slot; shared by every context proxy.
fn embedded_stubs() -> &'static Vec<usize> {
    EMBEDDED_STUBS.get_or_init(|| {
        // Two blocks of arena: the stubs for 2048 slots need 2048 * ARG_SLOT bytes.
        let mut arenas = vec![ArgArena::new(), ArgArena::new()];
        let stubs = (0..EMBED_SLOTS)
            .map(|i| {
                let offset = EMBED_BASE + i * 8;
                if offset >= EMBED_FILL_FROM {
                    let arena = if arenas[0].used + ARG_SLOT <= ARG_ARENA_BYTES { 0 } else { 1 };
                    arenas[arena].make(format!("[gpu] context embedded function +{offset:#x}"), ret_zero as usize as u64)
                } else {
                    0
                }
            })
            .collect();
        for a in arenas {
            std::mem::forget(a);
        }
        stubs
    })
}

static DEVICE_PROXY: AtomicUsize = AtomicUsize::new(0);
static CONTEXT_PROXY: AtomicUsize = AtomicUsize::new(0);

extern "system" fn ret_zero() -> usize {
    0
}

// ---------------------------------------------------------------------------------------
// Argument-logging stubs
// ---------------------------------------------------------------------------------------

unsafe extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn ReadProcessMemory(process: *mut c_void, addr: *const c_void, buf: *mut c_void, size: usize, read: *mut usize) -> i32;
}

/// Names and call counts of the argument-logging stubs, indexed by the stub's number.
static ARG_STUBS: std::sync::Mutex<Vec<(String, u32)>> = std::sync::Mutex::new(Vec::new());

/// A stub's slot size: the thunk below is about 135 bytes.
const ARG_SLOT: usize = 192;

/// Executable memory for the stubs, allocated in blocks.
struct ArgArena {
    mem: *mut u8,
    used: usize,
}

const ARG_ARENA_BYTES: usize = ARG_SLOT * 1024;

impl ArgArena {
    fn new() -> Self {
        let mem = unsafe { VirtualAlloc(ptr::null_mut(), ARG_ARENA_BYTES, 0x3000, 0x40) } as *mut u8;
        assert!(!mem.is_null(), "stub arena allocation failed");
        Self { mem, used: 0 }
    }

    /// A stub that logs its arguments and then jumps to `real`.
    ///
    /// The thunk preserves rcx, rdx, r8, r9 and xmm0-3, calls
    /// `arg_stub_hit(index, return_address, rcx, rdx, r8, r9)` and then tail-jumps to `real`.
    fn make(&mut self, label: String, real: u64) -> usize {
        assert!(self.used + ARG_SLOT <= ARG_ARENA_BYTES, "argument-stub arena full");
        let idx = {
            let mut v = ARG_STUBS.lock().unwrap();
            v.push((label, 0));
            v.len() - 1
        };
        let slot = unsafe { self.mem.add(self.used) };
        self.used += ARG_SLOT;
        let mut c: Vec<u8> = vec![
            0x51, // push rcx
            0x52, // push rdx
            0x41, 0x50, // push r8
            0x41, 0x51, // push r9
            0x48, 0x81, 0xEC, 0x88, 0x00, 0x00, 0x00, // sub rsp, 0x88
            0xF3, 0x0F, 0x7F, 0x44, 0x24, 0x40, // movdqu [rsp+40h], xmm0
            0xF3, 0x0F, 0x7F, 0x4C, 0x24, 0x50, // xmm1
            0xF3, 0x0F, 0x7F, 0x54, 0x24, 0x60, // xmm2
            0xF3, 0x0F, 0x7F, 0x5C, 0x24, 0x70, // xmm3
            0x48, 0x8B, 0x84, 0x24, 0x90, 0x00, 0x00, 0x00, // mov rax, [rsp+90h]  (original r8)
            0x48, 0x89, 0x44, 0x24, 0x20, // mov [rsp+20h], rax   (5th argument)
            0x48, 0x8B, 0x84, 0x24, 0x88, 0x00, 0x00, 0x00, // mov rax, [rsp+88h]  (original r9)
            0x48, 0x89, 0x44, 0x24, 0x28, // mov [rsp+28h], rax   (6th argument)
            0x4C, 0x8B, 0x8C, 0x24, 0x98, 0x00, 0x00, 0x00, // mov r9, [rsp+98h]   (original rdx)
            0x4C, 0x8B, 0x84, 0x24, 0xA0, 0x00, 0x00, 0x00, // mov r8, [rsp+0A0h]  (original rcx)
            0x48, 0x8B, 0x94, 0x24, 0xA8, 0x00, 0x00, 0x00, // mov rdx, [rsp+0A8h] (return address)
            0x48, 0xB9, // mov rcx, idx
        ];
        c.extend_from_slice(&(idx as u64).to_le_bytes());
        c.extend_from_slice(&[0x48, 0xB8]); // mov rax, arg_stub_hit
        c.extend_from_slice(&(arg_stub_hit as usize as u64).to_le_bytes());
        c.extend_from_slice(&[0xFF, 0xD0]); // call rax
        c.extend_from_slice(&[
            0xF3, 0x0F, 0x6F, 0x44, 0x24, 0x40, // movdqu xmm0, [rsp+40h]
            0xF3, 0x0F, 0x6F, 0x4C, 0x24, 0x50,
            0xF3, 0x0F, 0x6F, 0x54, 0x24, 0x60,
            0xF3, 0x0F, 0x6F, 0x5C, 0x24, 0x70,
            0x48, 0x81, 0xC4, 0x88, 0x00, 0x00, 0x00, // add rsp, 0x88
            0x41, 0x59, // pop r9
            0x41, 0x58, // pop r8
            0x5A, // pop rdx
            0x59, // pop rcx
            0x48, 0xB8, // mov rax, real
        ]);
        c.extend_from_slice(&real.to_le_bytes());
        c.extend_from_slice(&[0xFF, 0xE0]); // jmp rax
        assert!(c.len() <= ARG_SLOT, "stub is {} bytes", c.len());
        unsafe { ptr::copy_nonoverlapping(c.as_ptr(), slot, c.len()) };
        slot as usize
    }
}

/// Formats a value: if it points at readable memory, also shows the first qword there.
fn show(v: u64, out: &mut crate::RawBuf) {
    use std::fmt::Write;
    let mut first = 0u64;
    let mut got = 0usize;
    let readable = v > 0x10000
        && unsafe { ReadProcessMemory(GetCurrentProcess(), v as *const c_void, &mut first as *mut u64 as *mut c_void, 8, &mut got) } != 0
        && got == 8;
    if readable {
        let _ = write!(out, "{v:#x}->[{first:#x}]");
    } else {
        let _ = write!(out, "{v:#x}");
    }
}


/// Names a real D3D11 object (by asking it which interface it supports) for the call trace. Only
/// objects whose vtable lies inside `d3d11.dll` are asked; anything else is "-".
/// Presents so far.
static FRAMES: AtomicU32 = AtomicU32::new(0);

pub fn d3d_object_name(v: u64) -> &'static str {
    use windows::core::Interface;
    unsafe extern "system" {
        fn GetModuleHandleA(name: *const i8) -> *mut u8;
    }
    static RANGE: OnceLock<(usize, usize)> = OnceLock::new();
    let (lo, hi) = *RANGE.get_or_init(|| unsafe {
        let m = GetModuleHandleA(c"d3d11.dll".as_ptr());
        if m.is_null() {
            return (0, 0);
        }
        let size = *(m.add(*(m.add(0x3C) as *const u32) as usize + 0x50) as *const u32) as usize;
        (m as usize, m as usize + size)
    });
    let mut vt = 0u64;
    let mut got = 0usize;
    if v < 0x10000 || v % 8 != 0 || unsafe { ReadProcessMemory(GetCurrentProcess(), v as *const c_void, &mut vt as *mut u64 as *mut c_void, 8, &mut got) } == 0 {
        return "-";
    }
    if (vt as usize) < lo || (vt as usize) >= hi {
        return "-";
    }
    let obj = v as *mut c_void;
    let mut qi_addr = 0usize;
    if unsafe { ReadProcessMemory(GetCurrentProcess(), vt as *const c_void, &mut qi_addr as *mut usize as *mut c_void, 8, &mut got) } == 0 || qi_addr < lo || qi_addr >= hi {
        return "-";
    }
    let qi: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT = unsafe { std::mem::transmute(qi_addr) };
    let table: [(&str, GUID); 13] = [
        ("Buffer", ID3D11Buffer::IID),
        ("Tex2D", ID3D11Texture2D::IID),
        ("SRV", ID3D11ShaderResourceView::IID),
        ("RTV", ID3D11RenderTargetView::IID),
        ("DSV", ID3D11DepthStencilView::IID),
        ("Sampler", ID3D11SamplerState::IID),
        ("InputLayout", ID3D11InputLayout::IID),
        ("Blend", ID3D11BlendState::IID),
        ("DepthState", ID3D11DepthStencilState::IID),
        ("Raster", ID3D11RasterizerState::IID),
        ("VS", ID3D11VertexShader::IID),
        ("PS", ID3D11PixelShader::IID),
        ("Context", ID3D11DeviceContext::IID),
    ];
    for (name, iid) in table.iter() {
        let mut out = ptr::null_mut();
        if unsafe { qi(obj, iid, &mut out) }.0 >= 0 {
            let rel: unsafe extern "system" fn(*mut c_void) -> u32 = unsafe { std::mem::transmute(*((*(out as *const *const usize)).add(2))) };
            unsafe { rel(out) };
            return name;
        }
    }
    "other"
}

/// What `a` and what it points to are, for the call trace.
fn trace_kinds(a: u64) -> String {
    let mut first = 0u64;
    let mut got = 0usize;
    let pointee = if a > 0x10000 && unsafe { ReadProcessMemory(GetCurrentProcess(), a as *const c_void, &mut first as *mut u64 as *mut c_void, 8, &mut got) } != 0 { first } else { 0 };
    format!("{}/{}", d3d_object_name(a), d3d_object_name(pointee))
}

/// Called by every argument-logging stub. Logs the first calls of each stub, then only
/// powers of two, so a function called every frame does not flood the log.
extern "system" fn arg_stub_hit(idx: u64, ret: u64, this: u64, a2: u64, a3: u64, a4: u64) {
    let (name, count) = {
        let Ok(mut v) = ARG_STUBS.try_lock() else { return };
        let e = &mut v[idx as usize];
        e.1 += 1;
        (e.0.clone(), e.1)
    };
    // XB1_CTX_TRACE=n: log every call of the first n (in order), to read a frame's call sequence.
    static TRACE_LIMIT: OnceLock<u64> = OnceLock::new();
    static TRACE_SEQ: AtomicU64 = AtomicU64::new(0);
    let limit = *TRACE_LIMIT.get_or_init(|| std::env::var("XB1_CTX_TRACE").ok().and_then(|v| v.parse().ok()).unwrap_or(0));
    // XB1_TRACE_FRAME=n: the trace starts at the n-th Present (to reach a screen that is not the first).
    static TRACE_FROM: OnceLock<u32> = OnceLock::new();
    let from = *TRACE_FROM.get_or_init(|| std::env::var("XB1_TRACE_FRAME").ok().and_then(|v| v.parse().ok()).unwrap_or(0));
    let seq = if limit > 0 && FRAMES.load(Ordering::Relaxed) >= from { TRACE_SEQ.fetch_add(1, Ordering::Relaxed) } else { u64::MAX };
    let traced = seq < limit && (count <= 10 || from > 0);
    if !traced && count > 6 && !count.is_power_of_two() {
        return;
    }
    let (base, size) = crate::image_range();
    let mut line = crate::RawBuf::new();
    {
        use std::fmt::Write;
        if traced {
            let _ = write!(line, "seq {seq} ");
        }
        let _ = write!(line, "{name} (call #{count}) this=");
    }
    show(this, &mut line);
    for (label, a) in [("a2", a2), ("a3", a3), ("a4", a4)] {
        use std::fmt::Write;
        let _ = write!(line, " {label}=");
        show(a, &mut line);
        if traced {
            let _ = write!(line, "{{{}}}", trace_kinds(a));
        }
    }
    {
        use std::fmt::Write;
        let r = ret as usize;
        if r >= base && r < base + size {
            let _ = write!(line, " <- title+{:#x}", r - base);
        }
    }
    glog!("{}", line.as_str());
}

/// Prints the fields of the context proxy's embedded region that no longer hold their original
/// stub pointer, i.e. the state the title has written. Allocation-free (used from the crash
/// logger).
pub fn report_context_changes() {
    let p = CONTEXT_PROXY.load(Ordering::Acquire);
    if p == 0 {
        return;
    }
    let Some(stubs) = EMBEDDED_STUBS.get() else { return };
    let proxy = unsafe { &*(p as *const Proxy) };
    let forwarded = FORWARDED_INDEXES.get();
    let mut shown = 0;
    for i in 0..EMBED_SLOTS {
        if forwarded.is_some_and(|f| f.contains(&i)) {
            continue;
        }
        if proxy.embedded[i] != stubs[i] {
            glog!("  context field +{:#x} = {:#x} (was {:#x})", EMBED_BASE + i * 8, proxy.embedded[i], stubs[i]);
            shown += 1;
            if shown == 40 {
                break;
            }
        }
    }
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Device => "ID3D11DeviceX",
        Kind::Context => "ID3D11DeviceContextX",
    }
}

// ---------------------------------------------------------------------------------------
// IUnknown on the proxies
// ---------------------------------------------------------------------------------------

/// Interfaces of the D3D11 device / context family: a successful query for one of these is
/// answered with the proxy, so the console-only methods stay reachable.
fn is_d3d11_family(iid: &GUID, kind: Kind) -> bool {
    match kind {
        Kind::Device => [ID3D11Device::IID, ID3D11Device1::IID, ID3D11Device2::IID, ID3D11Device3::IID].contains(iid),
        Kind::Context => [ID3D11DeviceContext::IID, ID3D11DeviceContext1::IID, ID3D11DeviceContext2::IID, ID3D11DeviceContext3::IID].contains(iid),
    }
}

unsafe extern "system" fn proxy_qi(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    let p = unsafe { &*(this as *const Proxy) };
    let iid = unsafe { *iid };
    glog!("[gpu] {}: QueryInterface({iid:?})", kind_name(p.kind));
    if iid == IUnknown::IID {
        p.refs.fetch_add(1, Ordering::AcqRel);
        unsafe { *out = this };
        return HRESULT(0);
    }
    let real_qi: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT =
        unsafe { std::mem::transmute(*(*(p.real as *const *const usize))) };
    let mut real_out: *mut c_void = ptr::null_mut();
    let hr = unsafe { real_qi(p.real, &iid, &mut real_out) };
    if hr.0 == 0 {
        if is_d3d11_family(&iid, p.kind) {
            // Keep the proxy, drop the extra reference the real query took.
            let release: unsafe extern "system" fn(*mut c_void) -> u32 = unsafe { std::mem::transmute(*((*(real_out as *const *const usize)).add(2))) };
            unsafe { release(real_out) };
            p.refs.fetch_add(1, Ordering::AcqRel);
            unsafe { *out = this };
        } else {
            unsafe { *out = if iid == IID_IDXGI_DEVICE1 && crate::VCCORLIB110.load(Ordering::Relaxed) { wrap_dxgi(real_out, 1) } else { real_out } };
        }
        return HRESULT(0);
    }
    // The real object does not know this interface: it is a console-only one, so the proxy
    // (which carries the console extension slots) answers.
    glog!("[gpu] {}: QueryInterface({iid:?}) is console-only -> answered with the proxy", kind_name(p.kind));
    p.refs.fetch_add(1, Ordering::AcqRel);
    unsafe { *out = this };
    HRESULT(0)
}

unsafe extern "system" fn proxy_add_ref(this: *mut c_void) -> u32 {
    unsafe { (*(this as *const Proxy)).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

unsafe extern "system" fn proxy_release(this: *mut c_void) -> u32 {
    let left = unsafe { (*(this as *const Proxy)).refs.fetch_sub(1, Ordering::AcqRel) - 1 };
    if left == 0 {
        let real = unsafe { (*(this as *const Proxy)).real };
        let release: unsafe extern "system" fn(*mut c_void) -> u32 = unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(2))) };
        unsafe {
            release(real);
            std::alloc::dealloc(this as *mut u8, std::alloc::Layout::new::<Proxy>());
        }
    }
    left
}

// ---------------------------------------------------------------------------------------
// Methods that must return proxies instead of the raw desktop objects
// ---------------------------------------------------------------------------------------

/// Wraps `real` (taking over the caller's reference) in a proxy, or returns the existing one.
fn wrap(real: *mut c_void, kind: Kind) -> *mut c_void {
    let cache = match kind {
        Kind::Device => &DEVICE_PROXY,
        Kind::Context => &CONTEXT_PROXY,
    };
    let existing = cache.load(Ordering::Acquire);
    if existing != 0 {
        // The same object seen again: keep one proxy, drop the extra reference.
        let p = unsafe { &*(existing as *const Proxy) };
        if p.real == real {
            let release: unsafe extern "system" fn(*mut c_void) -> u32 = unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(2))) };
            unsafe { release(real) };
            p.refs.fetch_add(1, Ordering::AcqRel);
            return existing as *mut c_void;
        }
    }
    let proxy = new_proxy(real, kind) as *mut c_void;
    cache.store(proxy as usize, Ordering::Release);
    proxy
}

/// `ID3D11Device::GetImmediateContext(ID3D11DeviceContext**)`
unsafe extern "system" fn device_get_immediate_context(this: *mut c_void, out: *mut *mut c_void) {
    let p = unsafe { &*(this as *const Proxy) };
    let f: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) = unsafe { std::mem::transmute(*((*(p.real as *const *const usize)).add(DEVICE_GET_IMMEDIATE_CONTEXT))) };
    unsafe { f(p.real, out) };
    if !out.is_null() && unsafe { !(*out).is_null() } {
        unsafe { *out = wrap(*out, Kind::Context) };
    }
}

/// Drains the debug layer's message queue into the log (the layer otherwise writes to a debugger).
fn spawn_debug_message_pump(device: ID3D11Device) {
    use windows::core::Interface;
    struct SendDevice(ID3D11Device);
    unsafe impl Send for SendDevice {}
    let device = SendDevice(device);
    std::thread::spawn(move || {
        let device = device;
        let Ok(queue) = device.0.cast::<ID3D11InfoQueue>() else {
            glog!("[gpu] debug layer: no info queue (is the Graphics Tools feature installed?)");
            return;
        };
        loop {
            let count = unsafe { queue.GetNumStoredMessages() };
            for i in 0..count {
                let mut len = 0usize;
                unsafe {
                    if queue.GetMessage(i, None, &mut len).is_err() {
                        continue;
                    }
                }
                let mut buf = vec![0u8; len];
                let msg = buf.as_mut_ptr() as *mut D3D11_MESSAGE;
                if unsafe { queue.GetMessage(i, Some(msg), &mut len) }.is_ok() {
                    let m = unsafe { &*msg };
                    let text = unsafe { std::slice::from_raw_parts(m.pDescription as *const u8, m.DescriptionByteLength.saturating_sub(1)) };
                    glog!("[d3d-debug] severity {} id {}: {}", m.Severity.0, m.ID.0, String::from_utf8_lossy(text));
                }
            }
            unsafe { queue.ClearStoredMessages() };
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });
}

/// `ID3D11Device::CreateDeferredContext(UINT flags, ID3D11DeviceContext**)`
unsafe extern "system" fn device_create_deferred_context(this: *mut c_void, flags: u32, out: *mut *mut c_void) -> HRESULT {
    glog!("[gpu] CreateDeferredContext(flags {flags:#x}, out {out:p}) on {this:p}");
    let p = unsafe { &*(this as *const Proxy) };
    let f: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT =
        unsafe { std::mem::transmute(*((*(p.real as *const *const usize)).add(DEVICE_CREATE_DEFERRED_CONTEXT))) };
    if std::env::var_os("XB1_NO_DEFERRED").is_some() {
        return HRESULT(0x80004001u32 as i32);
    }
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = ONE_AT_A_TIME.lock().unwrap();
    // Experiment (XB1_DEFERRED_ON_HELPER=1): run the call on a plain thread of the layer.
    let hr = if std::env::var_os("XB1_DEFERRED_ON_HELPER").is_some() {
        let (real, outp) = (p.real as usize, out as usize);
        std::thread::spawn(move || {
            let f: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT = unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(DEVICE_CREATE_DEFERRED_CONTEXT))) };
            unsafe { f(real as *mut c_void, flags, outp as *mut *mut c_void) }
        })
        .join()
        .unwrap()
    } else {
        unsafe { f(p.real, flags, out) }
    };
    glog!("[gpu] CreateDeferredContext -> {:#x}", hr.0 as u32);
    if hr.0 >= 0 && !out.is_null() && unsafe { !(*out).is_null() } {
        // A deferred context is a separate object; give it its own proxy without caching.
        let proxy = new_proxy(unsafe { *out }, Kind::Context);
        unsafe { *out = proxy as *mut c_void };
    }
    hr
}

/// `ID3D11DeviceChild::GetDevice(ID3D11Device**)` on the context.
unsafe extern "system" fn context_get_device(this: *mut c_void, out: *mut *mut c_void) {
    let p = unsafe { &*(this as *const Proxy) };
    let f: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) = unsafe { std::mem::transmute(*((*(p.real as *const *const usize)).add(DEVICE_GET_DEVICE_SLOT_IN_CONTEXT))) };
    unsafe { f(p.real, out) };
    if !out.is_null() && unsafe { !(*out).is_null() } {
        unsafe { *out = wrap(*out, Kind::Device) };
    }
}

// ---------------------------------------------------------------------------------------
// Device creation methods (console extension slots)
// ---------------------------------------------------------------------------------------

/// Stand-in for objects whose type is not known: a COM-shaped object whose methods beyond
/// `QueryInterface`/`AddRef`/`Release` are argument-logging stubs that return 0.
#[repr(C)]
struct StandIn {
    vtbl: *const usize,
    refs: AtomicU32,
}

const STAND_IN_SLOTS: usize = 300;

unsafe extern "system" fn stand_in_qi(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    glog!("[gpu] stand-in object: QueryInterface({:?}) -> self", unsafe { *iid });
    unsafe {
        (*(this as *const StandIn)).refs.fetch_add(1, Ordering::AcqRel);
        *out = this;
    }
    HRESULT(0)
}

unsafe extern "system" fn stand_in_add_ref(this: *mut c_void) -> u32 {
    unsafe { (*(this as *const StandIn)).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

unsafe extern "system" fn stand_in_release(this: *mut c_void) -> u32 {
    let left = unsafe { (*(this as *const StandIn)).refs.fetch_sub(1, Ordering::AcqRel) - 1 };
    if left == 0 {
        drop(unsafe { Box::from_raw(this as *mut StandIn) });
    }
    left
}

fn stand_in_vtable() -> *const usize {
    static VTBL: OnceLock<Vec<usize>> = OnceLock::new();
    VTBL.get_or_init(|| {
        let mut arenas = vec![ArgArena::new(), ArgArena::new()];
        let mut v = vec![0usize; STAND_IN_SLOTS];
        v[0] = stand_in_qi as usize;
        v[1] = stand_in_add_ref as usize;
        v[2] = stand_in_release as usize;
        for i in 3..STAND_IN_SLOTS {
            let a = if arenas[0].used + ARG_SLOT <= ARG_ARENA_BYTES { 0 } else { 1 };
            v[i] = arenas[a].make(format!("[gpu] device-created object, vtable slot {i}"), ret_zero as usize as u64);
        }
        for a in arenas {
            std::mem::forget(a);
        }
        v
    })
    .as_ptr()
}

/// Names the vtables the layer hands out (for identifying an unknown object by its vtable).
pub fn log_vtable_identities() {
    glog!("[gpu] vtables: stand-in {:p}, device {:p}, context {:p}", stand_in_vtable(), vtable(Kind::Device), vtable(Kind::Context));
}

fn new_stand_in() -> *mut c_void {
    Box::into_raw(Box::new(StandIn { vtbl: stand_in_vtable(), refs: AtomicU32::new(1) })) as *mut c_void
}

fn hex_dump(p: *const u8, n: usize) -> String {
    let mut s = String::new();
    for i in 0..n {
        s.push_str(&format!("{:02x}", unsafe { *p.add(i) }));
        if i % 4 == 3 {
            s.push(' ');
        }
    }
    s
}

/// `ID3D11Device::CreateTexture2D` / `CreateTexture3D` of the real device, by vtable slot.
unsafe fn real_create_texture(real: *mut c_void, slot: usize, desc: *const u8, out: *mut *mut c_void) -> HRESULT {
    let f: unsafe extern "system" fn(*mut c_void, *const u8, *const c_void, *mut *mut c_void) -> HRESULT =
        unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(slot))) };
    unsafe { f(real, desc, ptr::null(), out) }
}

// ---------------------------------------------------------------------------------------
// Display chain (embedded context function +0x6a0)
// ---------------------------------------------------------------------------------------
//
// `call [ctx+0x6a0]` at title+0x1e3a06c is `(context, 1, Object** chain, Object** other)`. The title
// then calls slot 9 of `chain` with an out-pointer, gets a texture, and calls slot 13 of that
// texture for its size: so `chain` is the display swap chain (slot 9 = the back buffer, found from
// the call chain, not documented) and the texture is the back buffer. Slot meanings other than 9
// are unknown and log.

#[repr(C)]
struct DisplayChain {
    vtbl: *const usize,
    refs: AtomicU32,
    back_buffer: usize,
}

/// Display size and format used until the real window exists.
const DISPLAY_WIDTH: u32 = 1920;
const DISPLAY_HEIGHT: u32 = 1080;

unsafe extern "system" fn chain_qi(this: *mut c_void, _iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    unsafe {
        (*(this as *const DisplayChain)).refs.fetch_add(1, Ordering::AcqRel);
        *out = this;
    }
    HRESULT(0)
}
unsafe extern "system" fn chain_add_ref(this: *mut c_void) -> u32 {
    unsafe { (*(this as *const DisplayChain)).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}
unsafe extern "system" fn chain_release(this: *mut c_void) -> u32 {
    unsafe { (*(this as *const DisplayChain)).refs.fetch_sub(1, Ordering::AcqRel) - 1 }
}
/// Slot 9: `(chain, Texture** out)` returns the back buffer (the object is shared, so no AddRef
/// on a count the title never balances: it is kept alive by the chain).
unsafe extern "system" fn chain_get_back_buffer(this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    let chain = unsafe { &*(this as *const DisplayChain) };
    glog!("[gpu] display chain slot 9 -> back buffer {:#x}", chain.back_buffer);
    if !out.is_null() {
        unsafe { *out = chain.back_buffer as *mut c_void };
    }
    HRESULT(0)
}

fn chain_vtable() -> *const usize {
    static VTBL: OnceLock<Vec<usize>> = OnceLock::new();
    VTBL.get_or_init(|| {
        let mut arena = ArgArena::new();
        let mut v = vec![0usize; 64];
        v[0] = chain_qi as usize;
        v[1] = chain_add_ref as usize;
        v[2] = chain_release as usize;
        for i in 3..64 {
            v[i] = arena.make(format!("[gpu] display chain slot {i}"), ret_zero as usize as u64);
        }
        v[9] = chain_get_back_buffer as usize;
        std::mem::forget(arena);
        v
    })
    .as_ptr()
}

/// Creates the display back buffer on the desktop device behind `context_proxy`, as a
/// render-target-capable 2D texture with the console-style extended vtable.
fn create_back_buffer(context_proxy: *mut c_void) -> usize {
    let p = unsafe { &*(context_proxy as *const Proxy) };
    // ID3D11DeviceContext::GetDevice is slot 3.
    let get_device: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) = unsafe { std::mem::transmute(*((*(p.real as *const *const usize)).add(3))) };
    let mut device: *mut c_void = ptr::null_mut();
    unsafe { get_device(p.real, &mut device) };
    if device.is_null() {
        return 0;
    }
    // D3D11_TEXTURE2D_DESC: width, height, mips, array, format, {samples, quality}, usage, bind, cpu, misc.
    let desc: [u32; 11] = [
        DISPLAY_WIDTH, DISPLAY_HEIGHT, 1, 1, 28 /* R8G8B8A8_UNORM */, 1, 0, 0 /* DEFAULT */, 0x28 /* RENDER_TARGET | SHADER_RESOURCE (0x20|0x8) */, 0, 0,
    ];
    let create: unsafe extern "system" fn(*mut c_void, *const u32, *const c_void, *mut *mut c_void) -> HRESULT =
        unsafe { std::mem::transmute(*((*(device as *const *const usize)).add(5))) };
    let mut tex: *mut c_void = ptr::null_mut();
    let hr = unsafe { create(device, desc.as_ptr(), ptr::null(), &mut tex) };
    glog!("[gpu] display back buffer {DISPLAY_WIDTH}x{DISPLAY_HEIGHT}: {:#x} -> {:p}", hr.0 as u32, tex);
    if hr.0 >= 0 && !tex.is_null() {
        extend_resource(tex, true);
        return tex as usize;
    }
    0
}

/// Embedded context function `+0x6a0` (`call [ctx+0x6a0]` at title+0x1e3a06c):
/// `(context, 1, Object** chain, Object** other)`. `chain` is the display chain; `other` is a
/// stand-in (unknown).
unsafe extern "system" fn context_ext_212(this: *mut c_void, count: usize, a: *mut *mut c_void, b: *mut *mut c_void) -> HRESULT {
    glog!("[gpu] context +0x6a0: count {count} -> display chain and a stand-in");
    if !a.is_null() && (a as usize) % 8 == 0 {
        let back_buffer = create_back_buffer(this);
        let chain = Box::new(DisplayChain { vtbl: chain_vtable(), refs: AtomicU32::new(1), back_buffer });
        unsafe { *a = Box::into_raw(chain) as *mut c_void };
    }
    if !b.is_null() && (b as usize) % 8 == 0 {
        unsafe { *b = new_stand_in() };
    }
    HRESULT(0)
}

// ---------------------------------------------------------------------------------------
// Shaders, states, input layouts and queries: extended vtables
// ---------------------------------------------------------------------------------------
//
// The title calls console-only methods on these too. The wrapped creators call the real method and
// give the result an extended vtable: its real slots, then logging stubs. The number of real slots
// differs per type (ID3D11DeviceChild has 7; states and queries add a few).

static EXTENDED_OBJECT_VTABLES: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

/// Older XDK: the console's state objects have one more slot before `GetDesc` (8), so the extended
/// vtable's slot 8 is the desktop `GetDesc` (7).
fn older_sdk_state_get_desc(obj: *mut c_void) {
    if obj.is_null() {
        return;
    }
    let ext = unsafe { *(obj as *const usize) } as *mut usize;
    let orig = EXTENDED_OBJECT_VTABLES.lock().unwrap().iter().find(|(_, e)| *e == ext as usize).map(|(o, _)| *o);
    if let Some(orig) = orig {
        unsafe { *ext.add(8) = *(orig as *const usize).add(7) };
    }
}

fn extend_object(obj: *mut c_void, real_slots: usize) {
    if obj.is_null() {
        return;
    }
    let orig = unsafe { *(obj as *const usize) };
    let mut map = EXTENDED_OBJECT_VTABLES.lock().unwrap();
    let ext = if let Some((_, e)) = map.iter().find(|(k, _)| *k == orig) {
        *e
    } else {
        let mut arena = ArgArena::new();
        let mut v = vec![0usize; 96];
        for i in 0..real_slots {
            v[i] = unsafe { *((orig as *const usize).add(i)) };
        }
        for i in real_slots..96 {
            v[i] = arena.make(format!("[gpu] created-object extension slot {i} (real slots: {real_slots})"), ret_zero as usize as u64);
        }
        std::mem::forget(arena);
        let leaked: &'static [usize] = Vec::leak(v);
        map.push((orig, leaked.as_ptr() as usize));
        leaked.as_ptr() as usize
    };
    unsafe { *(obj as *mut usize) = ext };
}

/// `(object, creating device slot)` for shaders and input layouts, so a bind call can tell which
/// kind of object it was handed (the console binds them all through one method).
static CREATED_KINDS: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

fn restore_object_vtable(obj: *mut c_void) {
    if obj.is_null() || (obj as usize) % 8 != 0 {
        return;
    }
    let current = unsafe { *(obj as *const usize) };
    let map = EXTENDED_OBJECT_VTABLES.lock().unwrap();
    if let Some((orig, _)) = map.iter().find(|(_, e)| *e == current) {
        unsafe { *(obj as *mut usize) = *orig };
    }
}

unsafe fn real_context(this: *mut c_void) -> *mut c_void {
    unsafe { (*(this as *const Proxy)).real }
}

/// Console index 19 (called `(0, object, 0)` before each draw): binds a shader or an input layout.
/// Dispatches on the creating device slot: 11 layout, 12 VS, 13 GS, 15 PS, 16 HS, 17 DS.
unsafe extern "system" fn ctx_bind_object(this: *mut c_void, _a2: usize, obj: *mut c_void, _a4: usize) {
    let kind = CREATED_KINDS.lock().unwrap().iter().rev().find(|(o, _)| *o == obj as usize).map(|(_, k)| *k);
    static LOGS: AtomicU32 = AtomicU32::new(0);
    if LOGS.fetch_add(1, Ordering::Relaxed) < 12 {
        glog!("[gpu] bind object {obj:p} kind {kind:?} real {}", qi_kind(obj));
    }
    restore_object_vtable(obj);
    let real = unsafe { real_context(this) };
    let vt = unsafe { *(real as *const *const usize) };
    // desktop slots: IASetInputLayout 17, VSSetShader 11, GSSetShader 23, PSSetShader 9
    if kind.is_none() && qi_kind(obj) == "Buffer" {
        // IASetIndexBuffer(buffer, DXGI_FORMAT_R16_UINT, 0): the first console argument is a format enum
        let f: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, u32) = unsafe { std::mem::transmute(*vt.add(19)) };
        LAST_IB.store(buffer_desc(obj)[0], Ordering::Relaxed);
        LAST_IB_ARGS[0].store(_a2 as u32, Ordering::Relaxed);
        LAST_IB_ARGS[1].store(_a4 as u32, Ordering::Relaxed);
        static IBLOGS: AtomicU32 = AtomicU32::new(0);
        if IBLOGS.fetch_add(1, Ordering::Relaxed) < 3 {
            glog!("[gpu] index buffer {obj:p} desc {:x?}, console format {_a2}", buffer_desc(obj));
        }
        unsafe { f(real, obj, if _a2 == 0 { 57 } else { 42 }, _a4 as u32) }; // 57 is R16_UINT, 42 R32_UINT; the last argument is the byte offset
        return;
    }
    let slot = match kind {
        Some(11) => 17,
        Some(12) => 11,
        Some(13) => 23,
        Some(15) => 9,
        _ => return,
    };
    if slot == 17 {
        let f: unsafe extern "system" fn(*mut c_void, *mut c_void) = unsafe { std::mem::transmute(*vt.add(17)) };
        unsafe { f(real, obj) };
    } else {
        let f: unsafe extern "system" fn(*mut c_void, *mut c_void, *const c_void, u32) = unsafe { std::mem::transmute(*vt.add(slot)) };
        unsafe { f(real, obj, ptr::null(), 0) };
        if slot == 11 {
            // the draw follows: topology is a console-side draw argument, use triangle list
            let t: unsafe extern "system" fn(*mut c_void, u32) = unsafe { std::mem::transmute(*vt.add(24)) };
            unsafe { t(real, 4) };
        }
    }
}

/// Asks a real D3D11 object which interface it is (logging aid for the console's merged bind calls).
fn qi_kind(obj: *mut c_void) -> &'static str {
    if obj.is_null() || (obj as usize) % 8 != 0 {
        return "null";
    }
    let iids: [(&str, [u8; 16]); 4] = [
        ("SRV", [0xe0, 0x6f, 0xe0, 0xb0, 0x92, 0x81, 0x1a, 0x4e, 0xb1, 0xca, 0x36, 0xd7, 0x41, 0x47, 0x10, 0xb2]),
        ("PS", [0x0d, 0xe4, 0x82, 0xea, 0xdc, 0x51, 0x33, 0x4f, 0x93, 0xd4, 0xdb, 0x7c, 0x91, 0x25, 0xae, 0x8c]),
        ("VS", [0x64, 0x1d, 0x30, 0x3b, 0x78, 0xd6, 0x89, 0x42, 0x88, 0x97, 0x22, 0xf8, 0x92, 0x8b, 0x72, 0xf3]),
        ("Buffer", [0x85, 0x0b, 0x57, 0x48, 0xee, 0xd1, 0xcd, 0x4f, 0xa2, 0x50, 0xeb, 0x35, 0x07, 0x22, 0xb0, 0x37]),
    ];
    for (name, iid) in iids {
        let vt = unsafe { *(obj as *const *const usize) };
        let qi: unsafe extern "system" fn(*mut c_void, *const u8, *mut *mut c_void) -> i32 = unsafe { std::mem::transmute(*vt) };
        let mut out = ptr::null_mut();
        if unsafe { qi(obj, iid.as_ptr(), &mut out) } >= 0 {
            let rel: unsafe extern "system" fn(*mut c_void) -> u32 = unsafe { std::mem::transmute(*((*(out as *const *const usize)).add(2))) };
            unsafe { rel(out) };
            return name;
        }
    }
    "other"
}

/// `ID3D11Buffer::GetDesc` (slot 10): `[ByteWidth, Usage, BindFlags, CPUAccessFlags, MiscFlags, Stride]`.
fn buffer_desc(buf: *mut c_void) -> [u32; 6] {
    let mut d = [0u32; 6];
    if buf.is_null() || (buf as usize) % 8 != 0 {
        return d;
    }
    let vt = unsafe { *(buf as *const *const usize) };
    let f: unsafe extern "system" fn(*mut c_void, *mut u32) = unsafe { std::mem::transmute(*vt.add(10)) };
    unsafe { f(buf, d.as_mut_ptr()) };
    d
}

/// Console index 18: `IASetVertexBuffers(start, count, buffers, strides, offsets)`, forwarded; the first
/// calls log the buffer description and stride.
unsafe extern "system" fn ctx_ia_vb(this: *mut c_void, start: u32, count: u32, bufs: *const *mut c_void, strides: *const u32, offsets: *const u32) {
    static LOGS: AtomicU32 = AtomicU32::new(0);
    if LOGS.fetch_add(1, Ordering::Relaxed) < 4 && !bufs.is_null() && !strides.is_null() && !offsets.is_null() {
        let b = unsafe { *bufs };
        glog!("[gpu] IASetVertexBuffers({start}, {count}) buffer {b:p} desc {:x?} stride {} offset {}", buffer_desc(b), unsafe { *strides }, unsafe { *offsets });
    }
    let real = unsafe { real_context(this) };
    let f: unsafe extern "system" fn(*mut c_void, u32, u32, *const *mut c_void, *const u32, *const u32) =
        unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(18))) };
    unsafe { f(real, start, count, bufs, strides, offsets) };
}

/// Console index 12: `DrawIndexed(count, start)`. The console method has no base-vertex argument, so
/// `r9` still holds whatever the title left there (`0x76b40`); pass 0.
static LAST_IB: AtomicU32 = AtomicU32::new(0);
static LAST_IB_ARGS: [AtomicU32; 2] = [const { AtomicU32::new(0) }; 2];

/// `XB1_DRAW_TRACE=n` (with `XB1_TRACE_FRAME`): logs the first n draws from that Present on.
fn draw_trace(kind: &str, count: u32, start: u32) {
    static LIMIT: OnceLock<(u32, u32)> = OnceLock::new();
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let (limit, from) = *LIMIT.get_or_init(|| {
        let n = |k: &str| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        (n("XB1_DRAW_TRACE"), n("XB1_TRACE_FRAME"))
    });
    if limit > 0 && FRAMES.load(Ordering::Relaxed) >= from {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        if n < limit {
            glog!("[gpu] draw #{n} {kind} count {count} start {start} (index buffer bytes {:#x}, format arg {}, offset arg {:#x})", LAST_IB.load(Ordering::Relaxed), LAST_IB_ARGS[0].load(Ordering::Relaxed), LAST_IB_ARGS[1].load(Ordering::Relaxed));
        }
    }
}

unsafe extern "system" fn ctx_draw_indexed(this: *mut c_void, packed: u64) {
    // The console packs both numbers into one register: index count in the high half, first index
    // in the low half (`0x600000000` = 6 indices from 0, `0x60000001e` = 6 indices from 30).
    let (count, start) = ((packed >> 32) as u32, packed as u32);
    draw_trace("indexed", count, start);
    LAST_DRAW[0].store(1, Ordering::Relaxed);
    LAST_DRAW[1].store(count, Ordering::Relaxed);
    LAST_DRAW[2].store(start, Ordering::Relaxed);
    sync_before_draw(this);
    let real = unsafe { real_context(this) };
    // Console-side topology is not read (see `ctx_draw_old`, which leaves a strip behind): an indexed
    // draw is a triangle list.
    if crate::VCCORLIB110.load(Ordering::Relaxed) {
        let t: unsafe extern "system" fn(*mut c_void, u32) = unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(24))) };
        unsafe { t(real, 4) };
    }
    let f: unsafe extern "system" fn(*mut c_void, u32, u32, i32) =
        unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(12))) };
    unsafe { f(real, count, start, 0) };
}

/// Console index 43: `RSSetViewports(count, viewports)`; the first calls log the rectangles.
unsafe extern "system" fn ctx_viewports(this: *mut c_void, count: u32, vp: *const f32) {
    static LOGS: AtomicU32 = AtomicU32::new(0);
    if LOGS.fetch_add(1, Ordering::Relaxed) < 6 && !vp.is_null() && count >= 1 {
        let v = unsafe { std::slice::from_raw_parts(vp, 6) };
        glog!("[gpu] RSSetViewports({count}): x {} y {} w {} h {} depth {}..{}", v[0], v[1], v[2], v[3], v[4], v[5]);
    }
    let real = unsafe { real_context(this) };
    let f: unsafe extern "system" fn(*mut c_void, u32, *const f32) =
        unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(44))) };
    unsafe { f(real, count, vp) };
}

/// Console index 8: `(pointer to a variable holding a view, ..)` -> `PSSetShaderResources(0, 1, views)`.
unsafe extern "system" fn ctx_ps_srv(this: *mut c_void, views: *const *mut c_void) {
    if views.is_null() || (views as usize) % 8 != 0 {
        return;
    }
    static LOGS: AtomicU32 = AtomicU32::new(0);
    if LOGS.fetch_add(1, Ordering::Relaxed) < 6 {
        glog!("[gpu] console 8 object {:p} is {}", unsafe { *views }, qi_kind(unsafe { *views }));
        // SRV::GetResource (slot 7), then Texture2D::GetDesc (slot 10): [width, height, mips, array, format, ...]
        let srv = unsafe { *views };
        let vt = unsafe { *(srv as *const *const usize) };
        let get_res: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) = unsafe { std::mem::transmute(*vt.add(7)) };
        let mut res = ptr::null_mut();
        unsafe { get_res(srv, &mut res) };
        if !res.is_null() {
            let mut d = [0u32; 11];
            let rvt = unsafe { *(res as *const *const usize) };
            let gd: unsafe extern "system" fn(*mut c_void, *mut u32) = unsafe { std::mem::transmute(*rvt.add(10)) };
            unsafe { gd(res, d.as_mut_ptr()) };
            glog!("[gpu]   its texture desc {:x?}", d);
        }
    }
    restore_object_vtable(unsafe { *views });
    let real = unsafe { real_context(this) };
    let f: unsafe extern "system" fn(*mut c_void, u32, u32, *const *mut c_void) =
        unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(8))) };
    unsafe { f(real, 0, 1, views) };
}

/// Device slot -> (index of the out-pointer among the arguments after `this`, real vtable slots of
/// the created object).
const EXTENDED_CREATORS: [(usize, usize, usize); 14] = [
    (11, 5, 7),  // CreateInputLayout(descs, n, bytecode, len, out)
    (12, 4, 7),  // CreateVertexShader(bytecode, len, linkage, out)
    (13, 4, 7),  // CreateGeometryShader
    (15, 4, 7),  // CreatePixelShader
    (16, 4, 7),  // CreateHullShader
    (17, 4, 7),  // CreateDomainShader
    (18, 4, 7),  // CreateComputeShader
    (20, 2, 8),  // CreateBlendState(desc, out)
    (21, 2, 8),  // CreateDepthStencilState
    (22, 2, 8),  // CreateRasterizerState
    (23, 2, 8),  // CreateSamplerState
    (24, 2, 9),  // CreateQuery
    (25, 2, 9),  // CreatePredicate
    (26, 2, 9),  // CreateCounter
];

/// Wrapper for the creators above: `(device, a1..a5)`; the real method ignores the arguments it
/// does not use.
unsafe extern "system" fn device_create_extended<const SLOT: usize>(this: *mut c_void, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> HRESULT {
    let p = unsafe { &*(this as *const Proxy) };
    let f: unsafe extern "system" fn(*mut c_void, usize, usize, usize, usize, usize) -> HRESULT =
        unsafe { std::mem::transmute(*((*(p.real as *const *const usize)).add(SLOT))) };
    let hr = unsafe { f(p.real, a1, a2, a3, a4, a5) };
    static SHADER_LOGS: AtomicU32 = AtomicU32::new(0);
    if (12..=17).contains(&SLOT) && SHADER_LOGS.fetch_add(1, Ordering::Relaxed) < 12 {
        glog!("[gpu] device slot {SLOT} create shader/layout: hr={:#x} bytecode={}", hr.0 as u32, safe_dump(a1 as u64, 16));
    }
    if hr.0 >= 0 {
        if let Some((_, out_index, real_slots)) = EXTENDED_CREATORS.iter().find(|(s, _, _)| *s == SLOT) {
            let args = [a1, a2, a3, a4, a5];
            let out = args[out_index - 1] as *mut *mut c_void;
            if !out.is_null() && (out as usize) % 8 == 0 {
                extend_object(unsafe { *out }, *real_slots);
                if crate::VCCORLIB110.load(Ordering::Relaxed) && (20..=23).contains(&SLOT) {
                    older_sdk_state_get_desc(unsafe { *out });
                }
                CREATED_KINDS.lock().unwrap().push((unsafe { *out } as usize, SLOT));
            }
        }
    }
    hr
}

/// Device extension slot 67: `(device, desc*, Object** out)` (title call site 0x1a97c3a), the same
/// shape as slot 73: a description in, an object out, stored by the title in a member. Its meaning
/// is unknown, so a stand-in object is returned and the description is logged.
unsafe extern "system" fn device_ext_67(_this: *mut c_void, desc: *const u32, out: *mut *mut c_void) -> HRESULT {
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 6 {
        glog!("[gpu] device slot 67: desc = {} -> stand-in object", safe_dump(desc as u64, 48));
    }
    if !out.is_null() && (out as usize) % 8 == 0 {
        unsafe { *out = new_stand_in() };
    }
    HRESULT(0)
}

/// Device extension slot 59: `(device, const {u32 kind, u32 size}* desc, Object** out)`.
/// Called with kind 1 and 2, size 0x10000 each. What these objects are is not known, so a
/// stand-in COM object is returned and its use is logged.
unsafe extern "system" fn device_ext_59(_this: *mut c_void, desc: *const u32, out: *mut *mut c_void) -> HRESULT {
    let (kind, size) = unsafe { (*desc, *desc.add(1)) };
    glog!("[gpu] device slot 59: kind {kind}, size {size:#x} -> stand-in object");
    if !out.is_null() {
        unsafe { *out = new_stand_in() };
    }
    HRESULT(0)
}

/// Device extension slots 64 and 65. Six arguments, the shape of the console SDK's
/// `CreatePlacementTexture2D(desc, tileModeIndex, pitch, placementMemory, out)`:
/// `(device, desc*, tile mode, pitch, memory*, Texture** out)`. The placement memory and tile
/// mode have no desktop equivalent, so an ordinary texture is created from the description.
/// `desktop_create_slot` is the real device's `CreateTexture2D` (5) or `CreateTexture3D` (6).
unsafe fn create_placement_texture(
    which: u32,
    this: *mut c_void,
    desc: *const u8,
    tile_mode: u32,
    pitch: u32,
    memory: *mut c_void,
    out: *mut *mut c_void,
    desktop_create_slot: usize,
) -> HRESULT {
    let p = unsafe { &*(this as *const Proxy) };
    glog!(
        "[gpu] device slot {which}: tile mode {tile_mode:#x}, pitch {pitch}, memory {memory:p}, desc = {}",
        hex_dump(desc, 48)
    );
    // The description starts with the desktop layout (verified for sizes, formats and bind
    // flags in the dumps) followed by console-only fields that are not copied. Console-only
    // bits in MiscFlags (seen: 0x400 on a depth texture) make desktop creation fail with
    // E_INVALIDARG, so only the desktop-valid ones are kept.
    let mut desktop = [0u32; 11];
    let (copy_dwords, misc_index, misc_keep) = if desktop_create_slot == 5 {
        (11, 10, 0x1 | 0x4) // Texture2D: GENERATE_MIPS | TEXTURECUBE
    } else {
        (9, 8, 0x1) // Texture3D: GENERATE_MIPS
    };
    unsafe { ptr::copy_nonoverlapping(desc as *const u32, desktop.as_mut_ptr(), copy_dwords) };
    if desktop[misc_index] & !misc_keep != 0 {
        glog!("[gpu] device slot {which}: dropping console-only MiscFlags bits {:#x}", desktop[misc_index] & !misc_keep);
        desktop[misc_index] &= misc_keep;
    }
    let mut texture: *mut c_void = ptr::null_mut();
    let hr = unsafe { real_create_texture(p.real, desktop_create_slot, desktop.as_ptr() as *const u8, &mut texture) };
    if hr.0 >= 0 && !texture.is_null() {
        glog!("[gpu] device slot {which}: created a desktop texture {texture:p}");
        extend_resource(texture, desktop_create_slot == 5);
        if !out.is_null() {
            unsafe { *out = texture };
        }
        return HRESULT(0);
    }
    glog!("[gpu] device slot {which}: desktop texture creation failed ({:#x}) -> stand-in object", hr.0 as u32);
    if !out.is_null() {
        unsafe { *out = new_stand_in() };
    }
    HRESULT(0)
}

unsafe extern "system" fn device_ext_64(this: *mut c_void, desc: *const u8, tile_mode: u32, pitch: u32, memory: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    unsafe { create_placement_texture(64, this, desc, tile_mode, pitch, memory, out, 5) }
}

unsafe extern "system" fn device_ext_65(this: *mut c_void, desc: *const u8, tile_mode: u32, pitch: u32, memory: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    // The description observed for slot 65 starts {16, 16, ...}: it may be a 3D texture
    // (width, height, depth) or a small 2D one; treated as 3D until the dump says otherwise.
    unsafe { create_placement_texture(65, this, desc, tile_mode, pitch, memory, out, 6) }
}

// ---------------------------------------------------------------------------------------
// Placement memory mirroring
// ---------------------------------------------------------------------------------------
//
// A placement resource lives in memory the title chose, and the title writes into that memory
// directly because the console GPU reads it there. The desktop resource is separate, so before
// every draw or dispatch the contents of each placed buffer are compared with the last copy
// uploaded and, if they changed, uploaded again.

struct PlacedBuffer {
    resource: usize,
    memory: usize,
    shadow: Vec<u8>,
    scratch: Vec<u8>,
}

static PLACED_BUFFERS: std::sync::Mutex<Vec<PlacedBuffer>> = std::sync::Mutex::new(Vec::new());
static SYNC_UPLOADS: AtomicU32 = AtomicU32::new(0);

/// Registers a buffer created at `memory`. The reference the caller holds is kept alive for
/// the life of the process (the registry stores a raw pointer).
fn register_placed_buffer(resource: *mut c_void, memory: usize, size: usize) {
    // Keep the resource alive: AddRef through its own vtable.
    let add_ref: unsafe extern "system" fn(*mut c_void) -> u32 = unsafe { std::mem::transmute(*((*(resource as *const *const usize)).add(1))) };
    unsafe { add_ref(resource) };
    PLACED_BUFFERS.lock().unwrap().push(PlacedBuffer { resource: resource as usize, memory, shadow: vec![0u8; size], scratch: vec![0u8; size] });
}

/// Called in front of every draw and dispatch. `this` is the context proxy.
/// The title's inlined state cache writes the vertex shader, pixel shader and input layout straight
/// into the context object (`+0x178`, `+0x180`, `+0x188`, with dirty bits in `+0x168`) instead of
/// calling a bind method; applying them is the draw's job. Binds whichever of those fields holds an
/// object we created, by creating slot, and uses a triangle list.
fn apply_inlined_state(this: *mut c_void, real: *mut c_void) {
    static LOGS: AtomicU32 = AtomicU32::new(0);
    let vt = unsafe { *(real as *const *const usize) };
    let mut bound = [0usize; 3];
    for (i, off) in [0x178usize, 0x180, 0x188].iter().enumerate() {
        let obj = unsafe { *((this as usize + off) as *const usize) };
        if obj < 0x10000 || obj % 8 != 0 {
            continue;
        }
        let kind = CREATED_KINDS.lock().unwrap().iter().rev().find(|(o, _)| *o == obj).map(|(_, k)| *k);
        let Some(kind) = kind else { continue };
        restore_object_vtable(obj as *mut c_void);
        bound[i] = kind;
        match kind {
            11 => {
                let f: unsafe extern "system" fn(*mut c_void, *mut c_void) = unsafe { std::mem::transmute(*vt.add(17)) };
                unsafe { f(real, obj as *mut c_void) };
            }
            12 | 15 => {
                let slot = if kind == 12 { 11 } else { 9 };
                let f: unsafe extern "system" fn(*mut c_void, *mut c_void, *const c_void, u32) = unsafe { std::mem::transmute(*vt.add(slot)) };
                unsafe { f(real, obj as *mut c_void, ptr::null(), 0) };
            }
            _ => {}
        }
    }
    let t: unsafe extern "system" fn(*mut c_void, u32) = unsafe { std::mem::transmute(*vt.add(24)) };
    unsafe { t(real, 4) };
    if LOGS.fetch_add(1, Ordering::Relaxed) < 6 {
        glog!("[gpu] draw: inlined state kinds (+0x178,+0x180,+0x188) = {bound:?}");
    }
}

pub static DRAWS_SINCE_PRESENT: AtomicU32 = AtomicU32::new(0);
/// The arguments of the most recent draw (1 indexed or 0 plain, count, start), for the frame log.
static LAST_DRAW: [AtomicU32; 3] = [const { AtomicU32::new(0) }; 3];
extern "system" fn sync_before_draw(this: *mut c_void) {
    DRAWS_SINCE_PRESENT.fetch_add(1, Ordering::Relaxed);
    let real = unsafe { (*(this as *const Proxy)).real };
    apply_inlined_state(this, real);
    if env_flag("XB1_NO_MIRROR") {
        return;
    }
    let Ok(mut placed) = PLACED_BUFFERS.try_lock() else { return };
    // ID3D11DeviceContext::UpdateSubresource is slot 48.
    let update: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, *const c_void, *const c_void, u32, u32) =
        unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(48))) };
    for b in placed.iter_mut() {
        let mut got = 0usize;
        let ok = unsafe { ReadProcessMemory(GetCurrentProcess(), b.memory as *const c_void, b.scratch.as_mut_ptr() as *mut c_void, b.scratch.len(), &mut got) } != 0;
        if !ok || got != b.scratch.len() || b.scratch == b.shadow {
            continue;
        }
        unsafe { update(real, b.resource as *mut c_void, 0, ptr::null(), b.scratch.as_ptr() as *const c_void, 0, 0) };
        std::mem::swap(&mut b.shadow, &mut b.scratch);
        let n = SYNC_UPLOADS.fetch_add(1, Ordering::Relaxed);
        if n < 4 {
            glog!("[gpu] placement mirror: uploaded {} bytes from {:#x} to buffer {:#x}", b.shadow.len(), b.memory, b.resource);
        }
    }
}

/// A thunk that calls `sync_before_draw(this)` and then forwards to desktop context slot `slot`,
/// with every argument register and stack argument preserved.
fn synced_forward_thunk(mem: *mut u8, slot: usize) -> usize {
    let mut c: Vec<u8> = vec![
        0x51, // push rcx
        0x52, // push rdx
        0x41, 0x50, // push r8
        0x41, 0x51, // push r9
        0x48, 0x83, 0xEC, 0x68, // sub rsp, 0x68
        0xF3, 0x0F, 0x7F, 0x44, 0x24, 0x20, // movdqu [rsp+20h], xmm0
        0xF3, 0x0F, 0x7F, 0x4C, 0x24, 0x30,
        0xF3, 0x0F, 0x7F, 0x54, 0x24, 0x40,
        0xF3, 0x0F, 0x7F, 0x5C, 0x24, 0x50,
        0x48, 0xB8, // mov rax, sync_before_draw
    ];
    c.extend_from_slice(&(sync_before_draw as usize as u64).to_le_bytes());
    c.extend_from_slice(&[0xFF, 0xD0]); // call rax (rcx still holds `this`)
    c.extend_from_slice(&[
        0xF3, 0x0F, 0x6F, 0x44, 0x24, 0x20,
        0xF3, 0x0F, 0x6F, 0x4C, 0x24, 0x30,
        0xF3, 0x0F, 0x6F, 0x54, 0x24, 0x40,
        0xF3, 0x0F, 0x6F, 0x5C, 0x24, 0x50,
        0x48, 0x83, 0xC4, 0x68, // add rsp, 0x68
        0x41, 0x59, // pop r9
        0x41, 0x58, // pop r8
        0x5A, // pop rdx
        0x59, // pop rcx
        // forward: mov rax,[rcx+8]; mov rcx,rax; mov rax,[rax]; jmp [rax+slot*8]
        0x48, 0x8B, 0x41, 0x08, 0x48, 0x89, 0xC1, 0x48, 0x8B, 0x00, 0xFF, 0xA0,
    ]);
    c.extend_from_slice(&((slot * 8) as u32).to_le_bytes());
    assert!(c.len() <= 128, "thunk is {} bytes", c.len());
    unsafe { ptr::copy_nonoverlapping(c.as_ptr(), mem, c.len()) };
    mem as usize
}

// ---------------------------------------------------------------------------------------
// Logged forwarding of the device's standard creation methods
// ---------------------------------------------------------------------------------------

const LOGGED_CREATE_NAMES: [(usize, &str); 7] = [
    (3, "CreateBuffer"),
    (5, "CreateTexture2D"),
    (6, "CreateTexture3D"),
    (7, "CreateShaderResourceView"),
    (8, "CreateUnorderedAccessView"),
    (9, "CreateRenderTargetView"),
    (10, "CreateDepthStencilView"),
];

/// Called after a logged creation call. `rdx` and `r8` are the first two arguments after `this`
/// (a description and, depending on the method, a resource or initial data).
extern "system" fn creation_result(result: u64, a2: u64, a3: u64, which: u64) {
    let hr = result as u32 as i32;
    if hr >= 0 {
        return;
    }
    static FAILURES: AtomicU32 = AtomicU32::new(0);
    let n = FAILURES.fetch_add(1, Ordering::Relaxed);
    if n >= 40 {
        return;
    }
    let name = LOGGED_CREATE_NAMES.iter().find(|(s, _)| *s as u64 == which).map(|(_, n)| *n).unwrap_or("?");
    glog!("[gpu] {name} FAILED hr={:#x}: a2 = {} | a3 = {}", hr as u32, safe_dump(a2, 64), safe_dump(a3, 16));
    // For the view creators, `a2` is the resource: ask it what it is.
    if (7..=10).contains(&(which as usize)) && a2 > 0x10000 {
        let vtbl = unsafe { *(a2 as *const *const usize) };
        let mut got = 0usize;
        let mut v = 0usize;
        if unsafe { ReadProcessMemory(GetCurrentProcess(), vtbl as *const c_void, &mut v as *mut usize as *mut c_void, 8, &mut got) } != 0 && got == 8 {
            // ID3D11Resource::GetType (slot 7) returns the dimension: 1 buffer, 2 tex1d, 3 tex2d, 4 tex3d.
            let get_type: unsafe extern "system" fn(*mut c_void, *mut u32) = unsafe { std::mem::transmute(*vtbl.add(7)) };
            let mut dim = 0u32;
            unsafe { get_type(a2 as *mut c_void, &mut dim) };
            let mut desc = [0u32; 12];
            if dim == 3 || dim == 1 {
                let get_desc: unsafe extern "system" fn(*mut c_void, *mut u32) = unsafe { std::mem::transmute(*vtbl.add(10)) };
                unsafe { get_desc(a2 as *mut c_void, desc.as_mut_ptr()) };
            }
            glog!("[gpu]   the resource is dimension {dim} (1=buffer 3=2D 4=3D), its description: {}", hex_dump(desc.as_ptr() as *const u8, 48));
        }
    }
}

// ---------------------------------------------------------------------------------------
// Extended resource vtables
// ---------------------------------------------------------------------------------------
//
// The title calls console-only methods on the resources it creates (observed: slot 13 on a
// 2D texture, which reads two dwords that look like width and height). A real desktop resource
// has an 11-slot vtable (IUnknown 3, DeviceChild 4, Resource 3, GetDesc 1), so such a call
// reads past its end. Each resource gets an extended copy of its vtable (the 11 real entries,
// logging stubs after them, slot 13 implemented) and its object is pointed at it.

const RESOURCE_STANDARD_SLOTS: usize = 11;
const RESOURCE_EXT_SLOTS: usize = 64;

static EXTENDED_VTABLES: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

/// Slot 13 of a 2D texture: `(this, u32 out[2])` writes the width and height.
unsafe extern "system" fn texture_slot_13(this: *mut c_void, out: *mut u32) -> usize {
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let vtbl = unsafe { *(this as *const *const usize) };
    // Slot 10 (GetDesc) is still the real entry, copied into the extended vtable.
    let get_desc: unsafe extern "system" fn(*mut c_void, *mut u32) = unsafe { std::mem::transmute(*vtbl.add(10)) };
    let mut desc = [0u32; 12];
    unsafe { get_desc(this, desc.as_mut_ptr()) };
    if !out.is_null() {
        unsafe {
            *out = desc[0];
            *out.add(1) = desc[1];
        }
    }
    if COUNT.fetch_add(1, Ordering::Relaxed) < 3 {
        glog!("[gpu] texture extension slot 13: reported size {}x{}", desc[0], desc[1]);
    }
    0
}

/// Gives the real resource `obj` an extended vtable. `is_texture2d` selects the slot-13 behavior.
/// Diagnostic switches: `XB1_NO_EXTEND=1` leaves resource vtables alone, `XB1_NO_MIRROR=1` skips
/// the placement-buffer mirroring.
fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some()
}

fn extend_resource(obj: *mut c_void, is_texture2d: bool) {
    if obj.is_null() || env_flag("XB1_NO_EXTEND") {
        return;
    }
    let orig = unsafe { *(obj as *const usize) };
    let mut map = EXTENDED_VTABLES.lock().unwrap();
    let key = orig ^ (is_texture2d as usize); // 2D textures and others never share a vtable in practice
    let ext = if let Some((_, e)) = map.iter().find(|(k, _)| *k == key) {
        *e
    } else {
        let mut arena = ArgArena::new();
        let mut v = vec![0usize; RESOURCE_EXT_SLOTS];
        for i in 0..RESOURCE_STANDARD_SLOTS {
            v[i] = unsafe { *((orig as *const usize).add(i)) };
        }
        for i in RESOURCE_STANDARD_SLOTS..RESOURCE_EXT_SLOTS {
            v[i] = arena.make(format!("[gpu] resource extension slot {i}"), ret_zero as usize as u64);
        }
        if is_texture2d {
            v[13] = texture_slot_13 as usize;
        }
        std::mem::forget(arena);
        let leaked: &'static [usize] = Vec::leak(v);
        map.push((key, leaked.as_ptr() as usize));
        leaked.as_ptr() as usize
    };
    unsafe { *(obj as *mut usize) = ext };
}

/// Shared body of the sanitizing `CreateBuffer` / `CreateTexture2D` / `CreateTexture3D`
/// wrappers: copies the first `dwords` of the description, keeps only the desktop-valid bits of
/// the `MiscFlags` dword (console-only bits such as `0x20000` make desktop creation fail with
/// `E_INVALIDARG`), calls the real method and logs a failure.
unsafe fn create_sanitized(
    this: *mut c_void,
    slot: usize,
    name: &str,
    desc: *const u32,
    dwords: usize,
    misc_index: usize,
    misc_keep: u32,
    init: *const c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    static DROPPED: AtomicU32 = AtomicU32::new(0);
    let p = unsafe { &*(this as *const Proxy) };
    let mut d = [0u32; 11];
    unsafe { ptr::copy_nonoverlapping(desc, d.as_mut_ptr(), dwords) };
    let dropped = d[misc_index] & !misc_keep;
    if dropped != 0 {
        if DROPPED.fetch_add(1, Ordering::Relaxed) < 6 {
            glog!("[gpu] {name}: dropping console-only MiscFlags bits {dropped:#x}");
        }
        d[misc_index] &= misc_keep;
    }
    // Textures: DYNAMIC usage needs CPU write access on the desktop (the console does not ask for it).
    // 2D and 3D descriptions: usage at dword 7, CPU access flags at dword 9.
    if (slot == 5 || slot == 6) && dwords >= 10 && d[7] == 2 && d[9] == 0 {
        d[9] = 0x10000; // D3D11_CPU_ACCESS_WRITE
    }
    let f: unsafe extern "system" fn(*mut c_void, *const u32, *const c_void, *mut *mut c_void) -> HRESULT =
        unsafe { std::mem::transmute(*((*(p.real as *const *const usize)).add(slot))) };
    let hr = unsafe { f(p.real, d.as_ptr(), init, out) };
    if hr.0 < 0 {
        creation_result(hr.0 as u32 as u64, desc as u64, init as u64, slot as u64);
    } else if !out.is_null() {
        extend_resource(unsafe { *out }, slot == 5);
    }
    hr
}

unsafe extern "system" fn device_create_buffer(this: *mut c_void, desc: *const u32, init: *const c_void, out: *mut *mut c_void) -> HRESULT {
    // D3D11_BUFFER_DESC: 6 dwords, MiscFlags is the 5th. Desktop-valid: DRAWINDIRECT_ARGS, RAW_VIEWS, STRUCTURED.
    static LOGS: AtomicU32 = AtomicU32::new(0);
    if LOGS.fetch_add(1, Ordering::Relaxed) < 40 && !desc.is_null() {
        let d = unsafe { std::slice::from_raw_parts(desc, 6) };
        glog!("[gpu] CreateBuffer desc {:x?} initial data {:p}", d, init);
        if !init.is_null() && d[1] == 1 {
            let mem = unsafe { *(init as *const *const f32) };
            let n = (d[0] as usize / 4).min(100);
            if !mem.is_null() && d[2] == 1 {
                let f = unsafe { std::slice::from_raw_parts(mem, n) };
                for (i, v) in f.chunks(5).take(16).enumerate() {
                    glog!("[gpu]   vertex {i}: {v:?}");
                }
            }
        }
    }
    unsafe { create_sanitized(this, 3, "CreateBuffer", desc, 6, 4, 0x10 | 0x20 | 0x40, init, out) }
}

unsafe extern "system" fn device_create_texture2d(this: *mut c_void, desc: *const u32, init: *const c_void, out: *mut *mut c_void) -> HRESULT {
    // D3D11_TEXTURE2D_DESC: 11 dwords, MiscFlags is the 11th. Kept: GENERATE_MIPS, TEXTURECUBE.
    unsafe { create_sanitized(this, 5, "CreateTexture2D", desc, 11, 10, 0x1 | 0x4, init, out) }
}

unsafe extern "system" fn device_create_texture3d(this: *mut c_void, desc: *const u32, init: *const c_void, out: *mut *mut c_void) -> HRESULT {
    // D3D11_TEXTURE3D_DESC: 9 dwords, MiscFlags is the 9th. Kept: GENERATE_MIPS.
    unsafe { create_sanitized(this, 6, "CreateTexture3D", desc, 9, 8, 0x1, init, out) }
}

/// Shared body of the view creators `(this, resource, desc, out)`. If the description is
/// rejected with `E_INVALIDARG` (the console accepted descriptions that do not match their
/// resource, for example a plain 2D dimension on a multisampled texture, or an all-zero one),
/// the view is retried with no description, which makes D3D derive it from the resource.
unsafe fn create_view(this: *mut c_void, slot: usize, name: &str, resource: *mut c_void, desc: *const c_void, out: *mut *mut c_void) -> HRESULT {
    static FALLBACKS: AtomicU32 = AtomicU32::new(0);
    let p = unsafe { &*(this as *const Proxy) };
    if resource.is_null() {
        // The title has no display back buffer yet (no swap chain), so it asks for a view of NULL.
        // The debug layer crashes on this; the runtime would return E_INVALIDARG.
        let n = FALLBACKS.fetch_add(1, Ordering::Relaxed);
        if n < 8 {
            glog!("[gpu] {name}: NULL resource (display back buffer?) -> E_INVALIDARG");
        }
        return HRESULT(0x80070057u32 as i32);
    }
    let f: unsafe extern "system" fn(*mut c_void, *mut c_void, *const c_void, *mut *mut c_void) -> HRESULT =
        unsafe { std::mem::transmute(*((*(p.real as *const *const usize)).add(slot))) };
    let hr = unsafe { f(p.real, unwrap_object(resource), desc, out) };
    if hr.0 >= 0 && !out.is_null() {
        // Once per kind: the vtable of the created view, to identify unknown objects by vtable.
        static SEEN: [AtomicU32; 16] = [const { AtomicU32::new(0) }; 16];
        if SEEN[slot & 15].fetch_add(1, Ordering::Relaxed) == 0 {
            let obj = unsafe { *out };
            if !obj.is_null() {
                glog!("[gpu] {name} created: object {obj:p}, vtable {:#x}", unsafe { *(obj as *const usize) });
            }
        }
    }
    if hr.0 == 0x80070057u32 as i32 && !desc.is_null() && !resource.is_null() {
        let retry = unsafe { f(p.real, resource, ptr::null(), out) };
        let n = FALLBACKS.fetch_add(1, Ordering::Relaxed);
        if n < 8 {
            glog!("[gpu] {name}: description rejected ({}), retry with no description -> {:#x}", safe_dump(desc as u64, 16), retry.0 as u32);
        }
        if retry.0 >= 0 {
            return retry;
        }
        // Second fallback (shader resource views of 2D textures): a description built from the
        // resource's own description, as the title's console descriptions have a different layout
        // and even the NULL default failed for cube maps.
        if slot == 7 {
            let vtbl = unsafe { *(resource as *const *const usize) };
            let get_type: unsafe extern "system" fn(*mut c_void, *mut u32) = unsafe { std::mem::transmute(*vtbl.add(7)) };
            let mut dim = 0u32;
            unsafe { get_type(resource, &mut dim) };
            if dim == 3 {
                let get_desc: unsafe extern "system" fn(*mut c_void, *mut u32) = unsafe { std::mem::transmute(*vtbl.add(10)) };
                let mut rd = [0u32; 12];
                unsafe { get_desc(resource, rd.as_mut_ptr()) };
                let (mips, array, format, misc) = (rd[2], rd[3], rd[4], rd[10]);
                // D3D11_SHADER_RESOURCE_VIEW_DESC: format, dimension, then the union's dwords. Several
                // candidates, most faithful first; the first the driver accepts is used.
                let mut candidates: Vec<[u32; 6]> = Vec::new();
                if misc & 4 != 0 {
                    candidates.push([format, 9, 0, mips, 0, 0]); // TEXTURECUBE: MostDetailedMip, MipLevels
                }
                if array > 1 {
                    candidates.push([format, 5, 0, mips, 0, array]); // TEXTURE2DARRAY
                    candidates.push([format, 5, 0, 1, 0, array]); // TEXTURE2DARRAY, mip 0 only
                } else {
                    candidates.push([format, 4, 0, mips, 0, 0]); // TEXTURE2D
                }
                let mut r2 = HRESULT(0x8007000Eu32 as i32);
                let mut used = 0usize;
                for (i, c) in candidates.iter().enumerate() {
                    r2 = unsafe { f(p.real, resource, c.as_ptr() as *const c_void, out) };
                    used = i;
                    if r2.0 >= 0 {
                        break;
                    }
                }
                static BUILT: AtomicU32 = AtomicU32::new(0);
                if BUILT.fetch_add(1, Ordering::Relaxed) < 4 {
                    glog!("[gpu] CreateShaderResourceView: built descriptions from the resource (format {format:#x}, mips {mips}, array {array}, misc {misc:#x}): candidate {used} of {} -> {:#x}", candidates.len(), r2.0 as u32);
                }
                if r2.0 >= 0 {
                    return r2;
                }
            }
        }
    }
    if hr.0 < 0 {
        creation_result(hr.0 as u32 as u64, resource as u64, desc as u64, slot as u64);
    }
    hr
}

unsafe extern "system" fn device_create_srv(this: *mut c_void, res: *mut c_void, desc: *const c_void, out: *mut *mut c_void) -> HRESULT {
    unsafe { create_view(this, 7, "CreateShaderResourceView", res, desc, out) }
}
unsafe extern "system" fn device_create_uav(this: *mut c_void, res: *mut c_void, desc: *const c_void, out: *mut *mut c_void) -> HRESULT {
    unsafe { create_view(this, 8, "CreateUnorderedAccessView", res, desc, out) }
}
unsafe extern "system" fn device_create_rtv(this: *mut c_void, res: *mut c_void, desc: *const c_void, out: *mut *mut c_void) -> HRESULT {
    unsafe { create_view(this, 9, "CreateRenderTargetView", res, desc, out) }
}
unsafe extern "system" fn device_create_dsv(this: *mut c_void, res: *mut c_void, desc: *const c_void, out: *mut *mut c_void) -> HRESULT {
    unsafe { create_view(this, 10, "CreateDepthStencilView", res, desc, out) }
}

/// A thunk that calls desktop device slot `slot` and then `creation_result(result, a2, a3, slot)`,
/// returning the real result. Only for methods with at most three arguments after `this`, so
/// nothing is passed on the stack.
fn logged_create_thunk(mem: *mut u8, slot: usize) -> usize {
    let mut c: Vec<u8> = vec![
        0x48, 0x83, 0xEC, 0x48, // sub rsp, 0x48
        0x48, 0x89, 0x4C, 0x24, 0x20, // mov [rsp+20h], rcx
        0x48, 0x89, 0x54, 0x24, 0x28, // mov [rsp+28h], rdx
        0x4C, 0x89, 0x44, 0x24, 0x30, // mov [rsp+30h], r8
        0x4C, 0x89, 0x4C, 0x24, 0x38, // mov [rsp+38h], r9
        0x48, 0x8B, 0x41, 0x08, // mov rax, [rcx+8]    (real device)
        0x48, 0x89, 0xC1, // mov rcx, rax
        0x48, 0x8B, 0x00, // mov rax, [rax]      (real vtable)
        0xFF, 0x90, // call [rax + slot*8]
    ];
    c.extend_from_slice(&((slot * 8) as u32).to_le_bytes());
    c.extend_from_slice(&[
        0x48, 0x89, 0x44, 0x24, 0x40, // mov [rsp+40h], rax
        0x48, 0x89, 0xC1, // mov rcx, rax
        0x48, 0x8B, 0x54, 0x24, 0x28, // mov rdx, [rsp+28h]
        0x4C, 0x8B, 0x44, 0x24, 0x30, // mov r8, [rsp+30h]
        0x41, 0xB9, // mov r9d, slot
    ]);
    c.extend_from_slice(&(slot as u32).to_le_bytes());
    c.extend_from_slice(&[0x48, 0xB8]); // mov rax, creation_result
    c.extend_from_slice(&(creation_result as usize as u64).to_le_bytes());
    c.extend_from_slice(&[
        0xFF, 0xD0, // call rax
        0x48, 0x8B, 0x44, 0x24, 0x40, // mov rax, [rsp+40h]
        0x48, 0x83, 0xC4, 0x48, // add rsp, 0x48
        0xC3, // ret
    ]);
    assert!(c.len() <= 128, "thunk is {} bytes", c.len());
    unsafe { ptr::copy_nonoverlapping(c.as_ptr(), mem, c.len()) };
    mem as usize
}

/// Desktop context slots that read resources: `DrawIndexed`, `Draw`, `DrawIndexedInstanced`,
/// `DrawInstanced`, `DrawAuto`, `DrawIndexedInstancedIndirect`, `DrawInstancedIndirect`,
/// `Dispatch`, `DispatchIndirect`, and the copies `CopySubresourceRegion` and `CopyResource`.
const DRAW_SLOTS: [usize; 11] = [12, 13, 20, 21, 38, 39, 40, 41, 42, 46, 47];

/// Device extension slot 62: `(device, desc*, placementMemory*, Buffer** out)`, the shape of the
/// console SDK's `CreatePlacementBuffer`. The description is a `D3D11_BUFFER_DESC`:
/// `{ByteWidth, Usage, BindFlags, CPUAccessFlags, MiscFlags, StructureByteStride}`; the observed
/// calls are 1,168-byte constant buffers (`BindFlags = 4`) at addresses 0x700 apart.
///
/// An ordinary desktop buffer is created. The placement memory is NOT mirrored: a title that
/// writes constants straight into that memory (as the console GPU reads it) will not have those
/// writes reach this buffer. Keeping the two in sync is future work.
unsafe extern "system" fn device_ext_62(this: *mut c_void, desc: *const u32, memory: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let p = unsafe { &*(this as *const Proxy) };
    let mut d = [0u32; 6];
    unsafe { ptr::copy_nonoverlapping(desc, d.as_mut_ptr(), 6) };
    // Keep the desktop-valid MiscFlags (DRAWINDIRECT_ARGS 0x10, RAW_VIEWS 0x20, STRUCTURED 0x40).
    d[4] &= 0x10 | 0x20 | 0x40;
    // DYNAMIC needs CPU write access on the desktop and cannot be filled by UpdateSubresource (which
    // the placement mirror uses), so it becomes a DEFAULT buffer.
    if d[1] == 1 {
        d[1] = 0;
        d[3] = 0;
    }
    let mut buffer: *mut c_void = ptr::null_mut();
    let f: unsafe extern "system" fn(*mut c_void, *const u32, *const c_void, *mut *mut c_void) -> HRESULT =
        unsafe { std::mem::transmute(*((*(p.real as *const *const usize)).add(3))) };
    let hr = unsafe { f(p.real, d.as_ptr(), ptr::null(), &mut buffer) };
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 6 || hr.0 < 0 {
        glog!(
            "[gpu] device slot 62: buffer {} bytes, usage {}, bind {:#x}, misc {:#x}, placed at {memory:p} -> {:#x} {buffer:p}",
            d[0], d[1], d[2], d[4], hr.0 as u32
        );
    }
    if hr.0 >= 0 && !buffer.is_null() {
        if !memory.is_null() {
            register_placed_buffer(buffer, memory as usize, d[0] as usize);
        }
        extend_resource(buffer, false);
        if !out.is_null() {
            unsafe { *out = buffer };
        }
        return HRESULT(0);
    }
    if !out.is_null() {
        unsafe { *out = new_stand_in() };
    }
    HRESULT(0)
}

/// Safe hex dump of `n` bytes at `p`, or a note if the memory is not readable.
fn safe_dump(p: u64, n: usize) -> String {
    if p < 0x10000 {
        return format!("{p:#x}");
    }
    let mut buf = vec![0u8; n];
    let mut got = 0usize;
    let ok = unsafe { ReadProcessMemory(GetCurrentProcess(), p as *const c_void, buf.as_mut_ptr() as *mut c_void, n, &mut got) } != 0;
    if !ok && got == 0 {
        return format!("{p:#x} (unreadable)");
    }
    format!("{p:#x}: {}", hex_dump(buf.as_ptr(), got.min(n)))
}

// ---------------------------------------------------------------------------------------
// Display path (DXGIX*)
// ---------------------------------------------------------------------------------------

/// `DXGIXPresentArray`. The documented console shape is `(SyncInterval, PresentImmediateThreshold,
/// Flags, NumPlanes, ppPlanes, pParameters)`; this is assumed, so the first calls log the raw
/// arguments and what the plane pointers point at.
pub unsafe extern "system" fn present_array(a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) -> i32 {
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 6 {
        glog!("[gpu] DXGIXPresentArray #{n}: a1={a1:#x} a2={a2:#x} a3={a3:#x} a4={a4:#x} a5={} a6={}", safe_dump(a5, 48), safe_dump(a6, 64));
        if a4 > 0 && a4 <= 4 && a5 > 0x10000 {
            for i in 0..a4 as usize {
                let plane = unsafe { *((a5 as *const u64).add(i)) };
                glog!("[gpu]   plane {i}: {}", safe_dump(plane, 64));
            }
        }
    }
    0
}

/// `DXGIXSetVLineNotification(...)`: logged, reports success.
pub unsafe extern "system" fn set_vline_notification(a1: u64, a2: u64, a3: u64, a4: u64) -> i32 {
    glog!("[gpu] DXGIXSetVLineNotification({a1:#x}, {a2:#x}, {a3:#x}, {a4:#x})");
    0
}

/// Device extension slot 73: `(device, desc*, Object** out)`. The title zeroes `*out` (a COM-pointer
/// field of one of its resource wrappers), releases the old value, calls this, and expects the new
/// object in `*out`; leaving it NULL leaves the wrapper empty and makes later code dereference
/// NULL. The description (36 bytes, third dword often 0, the dword at +0xC an index 0/1/2, the one
/// at +0x10 a count 5/8/0xb that matches texture mip counts, `0x20000` at +4) is not decoded, so
/// a stand-in COM object is stored and its use is logged.
unsafe extern "system" fn device_ext_73(_this: *mut c_void, desc: *const u32, out: *mut *mut c_void) -> HRESULT {
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 6 {
        glog!("[gpu] device slot 73: desc = {} -> stand-in object", hex_dump(desc as *const u8, 36));
    }
    if !out.is_null() {
        unsafe { *out = new_stand_in() };
    }
    HRESULT(0)
}

/// Device slots 62 and 73: arguments unknown, so each call dumps what its pointer arguments
/// point at (64 bytes each) and returns 0. `(device, a2, a3, a4, a5, a6)`.
unsafe extern "system" fn device_ext_dump<const N: usize>(_this: *mut c_void, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) -> usize {
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 24 {
        glog!(
            "[gpu] device slot {N} call: a2 = {} | a3 = {} | a4 = {} | a5 = {} | a6 = {}",
            safe_dump(a2, 64),
            safe_dump(a3, 64),
            safe_dump(a4, 32),
            safe_dump(a5, 32),
            safe_dump(a6, 32)
        );
    }
    0
}

// ---------------------------------------------------------------------------------------
// vtable construction
// ---------------------------------------------------------------------------------------

/// Forwarding thunk for vtable slot `slot`: loads the real object from `[rcx + 8]`, makes it
/// `this`, and tail-jumps to the same slot of the real object's vtable.
///
/// ```text
/// mov rax, [rcx+8] ; mov rcx, rax ; mov rax, [rax] ; jmp [rax + slot*8]
/// ```
fn forward_thunk(mem: *mut u8, slot: usize) -> usize {
    let mut code = vec![0x48, 0x8B, 0x41, 0x08, 0x48, 0x89, 0xC1, 0x48, 0x8B, 0x00, 0xFF, 0xA0];
    code.extend_from_slice(&((slot * 8) as u32).to_le_bytes());
    unsafe { ptr::copy_nonoverlapping(code.as_ptr(), mem, code.len()) };
    mem as usize
}

/// Embedded/standard slot 8, `PSSetShaderResources(start, count, views)`. The debug layer reported
/// an out-of-range start slot, so a call with bad arguments is logged with its caller and dropped.
unsafe extern "system" fn ps_set_shader_resources(this: *mut c_void, start: u32, count: u32, views: *const c_void) {
    static BAD: AtomicU32 = AtomicU32::new(0);
    let real = unsafe { (*(this as *const Proxy)).real };
    if start >= 128 || count > 128 || start + count > 128 {
        let n = BAD.fetch_add(1, Ordering::Relaxed);
        if n < 6 {
            let mut frames = [ptr::null_mut::<c_void>(); 8];
            let got = unsafe { crate::RtlCaptureStackBackTrace(1, 8, frames.as_mut_ptr(), ptr::null_mut()) } as usize;
            glog!("[gpu] PSSetShaderResources with bad arguments (start {start:#x}, count {count:#x}, views {views:p}) from {:x?}; dropped", &frames[..got]);
        }
        return;
    }
    let f: unsafe extern "system" fn(*mut c_void, u32, u32, *const c_void) = unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(8))) };
    unsafe { f(real, start, count, views) };
}

/// Desktop context slots of the `XSSetShader` methods: `(shader, class instances, count)` on the
/// desktop but just the shader on the console (no class linkage), so the title leaves garbage in
/// the two extra arguments (the debug layer reported an out-of-range `NumClassInstances`).
const SET_SHADER_SLOTS: [usize; 6] = [9, 11, 23, 60, 64, 69];

/// Like `forward_thunk`, but for a context method that is a `SetShader`: zeroes `r8` and `r9`
/// (class instance array and count) first.
fn forward_context_thunk(mem: *mut u8, slot: usize) -> usize {
    if !SET_SHADER_SLOTS.contains(&slot) {
        return forward_thunk(mem, slot);
    }
    // xor r8d, r8d ; xor r9d, r9d ; then the usual forward
    let mut code = vec![0x45, 0x31, 0xC0, 0x45, 0x31, 0xC9];
    code.extend_from_slice(&[0x48, 0x8B, 0x41, 0x08, 0x48, 0x89, 0xC1, 0x48, 0x8B, 0x00, 0xFF, 0xA0]);
    code.extend_from_slice(&((slot * 8) as u32).to_le_bytes());
    unsafe { ptr::copy_nonoverlapping(code.as_ptr(), mem, code.len()) };
    mem as usize
}

fn build_vtable(kind: Kind) -> Vec<usize> {
    let standard = match kind {
        Kind::Device => DEVICE_STANDARD_SLOTS,
        Kind::Context => CONTEXT_STANDARD_SLOTS,
    };
    // Executable memory for the forwarding thunks (16 bytes each) and a private stub arena.
    let thunks = unsafe { VirtualAlloc(ptr::null_mut(), 4096 * 2, 0x3000, 0x40) } as *mut u8;
    assert!(!thunks.is_null(), "thunk allocation failed");
    let mut arenas = vec![ArgArena::new(), ArgArena::new()];

    let mut v = vec![0usize; PROXY_SLOTS];
    v[0] = proxy_qi as usize;
    v[1] = proxy_add_ref as usize;
    v[2] = proxy_release as usize;
    for i in 3..standard {
        v[i] = if kind == Kind::Context {
            forward_context_thunk(unsafe { thunks.add(i * 32) }, i)
        } else {
            forward_thunk(unsafe { thunks.add(i * 32) }, i)
        };
    }
    match kind {
        Kind::Device => {
            v[DEVICE_GET_IMMEDIATE_CONTEXT] = device_get_immediate_context as usize;
            v[DEVICE_CREATE_DEFERRED_CONTEXT] = device_create_deferred_context as usize;
        }
        Kind::Context => v[DEVICE_GET_DEVICE_SLOT_IN_CONTEXT] = context_get_device as usize,
    }
    // Console-only extension slots: log the slot, return 0.
    for i in standard..PROXY_SLOTS {
        let arena = if arenas[0].used + ARG_SLOT <= ARG_ARENA_BYTES { 0 } else { 1 };
        v[i] = arenas[arena].make(format!("[gpu] {} extension slot {i}", kind_name(kind)), ret_zero as usize as u64);
    }
    for a in arenas {
        std::mem::forget(a);
    }
    if kind == Kind::Device {
        // The standard creation methods log their failures (and the description) instead of
        // forwarding silently.
        let logged = unsafe { VirtualAlloc(ptr::null_mut(), 4096, 0x3000, 0x40) } as *mut u8;
        assert!(!logged.is_null(), "thunk allocation failed");
        for (n, (slot, _)) in LOGGED_CREATE_NAMES.iter().enumerate() {
            v[*slot] = logged_create_thunk(unsafe { logged.add(n * 128) }, *slot);
        }
        // Slots 41 and 42 are SetExceptionMode/GetExceptionMode on the desktop, but the title calls
        // slot 41 with a pointer (the debug layer: "Unrecognized RaiseFlags"), so they are
        // something else on the console. Log and succeed instead of forwarding.
        let mut arena = ArgArena::new();
        v[41] = arena.make("[gpu] device slot 41 (SetExceptionMode on the desktop)".to_string(), ret_zero as usize as u64);
        v[42] = arena.make("[gpu] device slot 42 (GetExceptionMode on the desktop)".to_string(), ret_zero as usize as u64);
        if crate::VCCORLIB110.load(Ordering::Relaxed) {
            // Older XDK: the title calls slot 41 as GetImmediateContext(&out) (the result becomes its context).
            v[41] = device_get_immediate_context as usize;
        }
        std::mem::forget(arena);
        // Buffers and textures also need their console-only MiscFlags bits removed.
        v[3] = device_create_buffer as usize;
        v[5] = device_create_texture2d as usize;
        v[6] = device_create_texture3d as usize;
        v[7] = device_create_srv as usize;
        v[8] = device_create_uav as usize;
        v[9] = device_create_rtv as usize;
        v[10] = device_create_dsv as usize;
        // Shaders, states, input layouts and queries get extended vtables.
        v[11] = device_create_extended::<11> as usize;
        v[12] = device_create_extended::<12> as usize;
        v[13] = device_create_extended::<13> as usize;
        v[15] = device_create_extended::<15> as usize;
        v[16] = device_create_extended::<16> as usize;
        v[17] = device_create_extended::<17> as usize;
        v[18] = device_create_extended::<18> as usize;
        v[20] = device_create_extended::<20> as usize;
        v[21] = device_create_extended::<21> as usize;
        v[22] = device_create_extended::<22> as usize;
        v[23] = device_create_extended::<23> as usize;
        v[24] = device_create_extended::<24> as usize;
        v[25] = device_create_extended::<25> as usize;
        v[26] = device_create_extended::<26> as usize;
        // Creation methods decoded from the title's call sites.
        v[59] = device_ext_59 as usize;
        v[64] = device_ext_64 as usize;
        v[65] = device_ext_65 as usize;
        v[62] = device_ext_62 as usize;
        v[73] = device_ext_73 as usize;
        v[67] = device_ext_67 as usize;
    }
    v
}

fn vtable(kind: Kind) -> *const usize {
    static DEVICE: OnceLock<Vec<usize>> = OnceLock::new();
    static CONTEXT: OnceLock<Vec<usize>> = OnceLock::new();
    match kind {
        Kind::Device => DEVICE.get_or_init(|| build_vtable(Kind::Device)).as_ptr(),
        Kind::Context => CONTEXT.get_or_init(|| build_vtable(Kind::Context)).as_ptr(),
    }
}

/// Starts the debug-message pump for a raw real device (XB1_D3D_DEBUG with the plain `D3D11CreateDevice` path).
pub unsafe fn start_debug_pump_raw(device: *mut c_void) {
    if device.is_null() {
        return;
    }
    let d: ID3D11Device = unsafe { Interface::from_raw_borrowed(&device) }.cloned().expect("device");
    spawn_debug_message_pump(d);
}

/// If `device` is one of the layer's proxy devices, returns the real device behind it.
pub unsafe fn unwrap_device(device: *mut c_void) -> *mut c_void {
    if device.is_null() {
        return device;
    }
    let vt = unsafe { *(device as *const *const usize) };
    if vt == vtable(Kind::Device) {
        return unsafe { (*(device as *const Proxy)).real };
    }
    device
}

/// Replaces the device and context a plain `D3D11CreateDevice` returned with the layer's proxies, so
/// console-only interfaces and extension slots work (same as `D3D11XCreateDeviceX`).
pub unsafe fn wrap_created_device(device: *mut *mut c_void, context: *mut *mut c_void) {
    if !device.is_null() && !unsafe { *device }.is_null() {
        unsafe { *device = wrap(*device, Kind::Device) };
    }
    if !context.is_null() && !unsafe { *context }.is_null() {
        unsafe { *context = wrap(*context, Kind::Context) };
    }
}

// ---------------------------------------------------------------------------------------
// D3D11XCreateDeviceX
// ---------------------------------------------------------------------------------------

/// `D3D11XCreateDeviceX(params, &device, &context)`.
///
/// The parameter struct is console-specific and not documented here, so its first 64 bytes
/// are logged and otherwise ignored; flags are not forwarded because console-only bits would
/// be rejected by desktop D3D11. The returned device and context are proxies (see the module
/// documentation).
pub extern "system" fn create_device_x(params: *const u8, out_device: *mut *mut c_void, out_context: *mut *mut c_void) -> i32 {
    if !params.is_null() {
        let mut hex = String::new();
        for i in 0..64 {
            hex.push_str(&format!("{:02x}", unsafe { *params.add(i) }));
            if i % 8 == 7 {
                hex.push(' ');
            }
        }
        glog!("[gpu] D3D11XCreateDeviceX params: {hex}");
    }

    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let mut got = D3D_FEATURE_LEVEL_11_0;
    // XB1_D3D_DEBUG=1 turns on the desktop debug layer and logs its validation messages.
    let debug = std::env::var_os("XB1_D3D_DEBUG").is_some();
    let create_flags = if debug { D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_DEBUG } else { D3D11_CREATE_DEVICE_BGRA_SUPPORT };
    let result = unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            create_flags,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut got),
            Some(&mut context),
        )
    };
    match result {
        Ok(()) => {
            glog!("[gpu] created a desktop D3D11 device (feature level {:#x}); returning proxies", got.0);
            if debug {
                if let Some(d) = device.clone() {
                    spawn_debug_message_pump(d);
                }
            }
            unsafe {
                if !out_device.is_null() {
                    *out_device = device.map_or(ptr::null_mut(), |d| wrap(d.into_raw(), Kind::Device));
                }
                if !out_context.is_null() {
                    *out_context = context.map_or(ptr::null_mut(), |c| wrap(c.into_raw(), Kind::Context));
                }
            }
            0
        }
        Err(e) => {
            glog!("[gpu] D3D11CreateDevice failed: {e}");
            e.code().0
        }
    }
}

/// `IDXGIDevice1`. The older console DXGI interfaces have one more method than the desktop's,
/// ahead of `GetParent`: Minecraft calls `GetAdapter(&out)` through vtable slot 8 of the device
/// (desktop 7; desktop slot 8 is `CreateSurface`, which answers `DXGI_ERROR_INVALID_CALL`) and
/// `GetParent(iid, &out)` through slot 7 of the adapter (desktop 6). So DXGI objects handed to the
/// title are wrapped: console slot n from 7 on forwards to desktop slot n - 1, and the objects
/// returned by `GetAdapter` / `GetParent` are wrapped the same way.
const IID_IDXGI_DEVICE1: GUID = GUID::from_u128(0x77db970f_6276_48ba_ba28_070143b4392c);

/// Layout of a wrapper: `[vtable, real object]`.
fn real_of(wrapper: *mut c_void) -> *mut c_void {
    unsafe { *((wrapper as *const usize).add(1)) as *mut c_void }
}

unsafe extern "system" fn dxgi_get_parent_wrapped(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    let real = real_of(this);
    let f: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT = unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(6))) };
    let hr = unsafe { f(real, iid, out) };
    if hr.0 == 0 {
        unsafe { *out = wrap_dxgi(*out, 0) };
    }
    hr
}

unsafe extern "system" fn dxgi_get_adapter_wrapped(this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    let real = real_of(this);
    let f: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT = unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(7))) };
    let hr = unsafe { f(real, out) };
    if hr.0 == 0 {
        unsafe { *out = wrap_dxgi(*out, 0) };
    }
    hr
}

unsafe extern "system" fn dxgi_get_buffer_wrapped(this: *mut c_void, index: u32, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    let real = real_of(this);
    let f: unsafe extern "system" fn(*mut c_void, u32, *const GUID, *mut *mut c_void) -> HRESULT = unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(9))) };
    let hr = unsafe { f(real, index, iid, out) };
    if hr.0 == 0 {
        glog!("[gpu] swap chain buffer {index} is the real resource {:p}", unsafe { *out });
        if index == 0 {
            BACKBUFFER.store(unsafe { *out } as usize, Ordering::Relaxed);
        }
        unsafe { *out = wrap_dxgi(*out, 3) };
    }
    hr
}

/// Wraps an object so the older console vtable layout reaches the desktop methods. `kind`: 0 plain
/// DXGI object (slots 7 and up shift by one), 1 the DXGI device (also `GetAdapter` at slot 8),
/// 2 a swap chain (also `GetBuffer` at slot 10, results wrapped as resources), 3 a D3D11 resource
/// (the resource interfaces carry two more methods than the desktop: slots 9 and up shift by two;
/// seen as `Texture2D::GetDesc` through slot 12).
pub fn wrap_dxgi(real: *mut c_void, kind: u8) -> *mut c_void {
    const SLOTS: usize = 40;
    let thunks = unsafe { VirtualAlloc(ptr::null_mut(), 4096 * 2, 0x3000, 0x40) } as *mut u8;
    assert!(!thunks.is_null(), "thunk allocation failed");
    let vtbl: &'static mut [usize; SLOTS] = Box::leak(Box::new([0usize; SLOTS]));
    for (s, entry) in vtbl.iter_mut().enumerate() {
        let desktop = match kind {
            3 if s >= 12 => s - 2,
            3 if (8..=10).contains(&s) => s - 1,
            3 => s,
            4 if s >= 8 => s - 1,
            4 => s,
            _ if s >= 7 => s - 1,
            _ => s,
        };
        *entry = forward_thunk(unsafe { thunks.add(s * 32) }, desktop);
    }
    if kind < 3 {
        vtbl[7] = dxgi_get_parent_wrapped as usize;
    }
    if kind == 1 {
        vtbl[8] = dxgi_get_adapter_wrapped as usize;
    }
    if kind == 2 {
        vtbl[10] = dxgi_get_buffer_wrapped as usize;
        vtbl[9] = dxgi_present_wrapped as usize;
    }
    vtbl[0] = wrapped_qi as usize;
    if kind == 4 {
        vtbl[8] = view_get_resource_wrapped as usize;
    }
    let obj: &'static mut [usize; 3] = Box::leak(Box::new([vtbl.as_ptr() as usize, real as usize, kind as usize]));
    WRAPPERS.lock().unwrap().push((obj.as_ptr() as usize, real as usize));
    glog!("[gpu] object {real:p} wrapped as kind {kind} (0 DXGI, 1 DXGI device, 2 swap chain, 3 resource)");
    obj.as_mut_ptr() as *mut c_void
}

/// Wrapper objects made by `wrap_dxgi`, mapped to the real object they forward to.
static WRAPPERS: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

/// The real object behind a wrapper, or the pointer itself when it is not one.
pub fn unwrap_object(p: *mut c_void) -> *mut c_void {
    WRAPPERS.lock().unwrap().iter().find(|(w, _)| *w == p as usize).map(|(_, r)| *r as *mut c_void).unwrap_or(p)
}

/// `(console index, desktop slot)` pairs seen in Minecraft Xbox One Edition (older XDK), from logged
/// calls: 31 is called as (1, &rtv, dsv) = `OMSetRenderTargets` (desktop 33); 42 as (1, &viewport) =
/// `RSSetViewports` (desktop 44); 33 `OMSetBlendState(state, factor, mask)` (35), 34 `OMSetDepthStencilState(state, ref)` (36),
/// 41 `RSSetState` (43), 43 `RSSetScissorRects` (45), 46 `UpdateSubresource` with the subresource counting 0 to 4 (48),
/// 51 `ClearDepthStencilView(dsv, 3, ..)` (53), all two behind the desktop; 17 `IASetVertexBuffers` (18), 6 and 15 the VS and PS `SetConstantBuffers` (7, 16), 19 and 20 the PS `SetShaderResources`/`SetSamplers` (8, 10) and 7 the VS resources (25; a guess), 18 the Sonic-shaped `IASetIndexBuffer(format, buffer, offset)`, 10 a packed `DrawIndexed`, 11 `Draw`; 13 is `Map` (see `ctx_map_old`, desktop 14) and 14 `Unmap(res, 0)` (desktop 15).
const OLD_SDK_EXTRA: [(usize, usize); 9] =
    [(14, 15), (46, 48), (17, 18), (6, 7), (15, 16), (7, 8), (9, 10), (23, 25), (24, 26)];

/// Older-XDK embedded index 13 (+0x448): `Map(resource, subresource, map type, flags, void** data)`. The
/// console returns the data pointer itself; the desktop fills a `D3D11_MAPPED_SUBRESOURCE`
/// (`[pData, RowPitch, DepthPitch]`). Evidence: called as `(res, 0, 4, 0, &p)` then `*p` is written
/// and index 14 (+0x450, `Unmap(res, 0)`, desktop 15) follows.
unsafe extern "system" fn ctx_map_old(this: *mut c_void, res: *mut c_void, sub: u32, map_type: u32, flags: u32, out: *mut *mut c_void) -> HRESULT {
    let real = unsafe { real_context(this) };
    let f: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, u32, u32, *mut [usize; 3]) -> HRESULT =
        unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(14))) };
    let mut mapped = [0usize; 3]; // D3D11_MAPPED_SUBRESOURCE: pData, RowPitch, DepthPitch
    let hr = unsafe { f(real, unwrap_object(res), sub, map_type, flags, &mut mapped) };
    if !out.is_null() {
        unsafe { *out = if hr.0 >= 0 { mapped[0] as *mut c_void } else { ptr::null_mut() } };
    }
    static LOGS: AtomicU32 = AtomicU32::new(0);
    if hr.0 < 0 && LOGS.fetch_add(1, Ordering::Relaxed) < 6 {
        glog!("[gpu] Map({res:p}, {sub}, {map_type}, {flags:#x}) FAILED hr={:#x}", hr.0 as u32);
    }
    hr
}

/// Older-XDK embedded index 87 (+0x698): `OMGetRenderTargets(n, &rtv, &dsv)` (desktop 89). The views
/// it returns are wrapped, because this SDK's view interfaces are one slot longer (`GetResource` is
/// called through slot 8).
unsafe extern "system" fn ctx_om_get_render_targets_old(this: *mut c_void, n: u32, rtvs: *mut *mut c_void, dsv: *mut *mut c_void) {
    let real = unsafe { real_context(this) };
    let f: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void, *mut *mut c_void) =
        unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(89))) };
    unsafe { f(real, n, rtvs, dsv) };
    if !rtvs.is_null() {
        for i in 0..n as usize {
            let slot = unsafe { rtvs.add(i) };
            if !unsafe { *slot }.is_null() {
                unsafe { *slot = wrap_dxgi(*slot, 4) };
            }
        }
    }
    if !dsv.is_null() && !unsafe { *dsv }.is_null() {
        unsafe { *dsv = wrap_dxgi(*dsv, 4) };
    }
}

/// Older-XDK embedded index 31 (+0x4D8): `OMSetRenderTargets(n, &rtv, dsv)` (desktop 33), with wrapped
/// views (from `ctx_om_get_render_targets_old`) turned back into the real ones.
unsafe extern "system" fn ctx_om_set_render_targets_old(this: *mut c_void, n: u32, rtvs: *const *mut c_void, dsv: *mut c_void) {
    let real = unsafe { real_context(this) };
    let f: unsafe extern "system" fn(*mut c_void, u32, *const *mut c_void, *mut c_void) =
        unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(33))) };
    let mut views = [ptr::null_mut::<c_void>(); 8];
    let count = (n as usize).min(8);
    static OMLOGS: AtomicU32 = AtomicU32::new(0);
    if OMLOGS.fetch_add(1, Ordering::Relaxed) < 12 {
        let first = if rtvs.is_null() { ptr::null_mut() } else { unsafe { *rtvs } };
        let real_view = unwrap_object(first);
        let mut res: *mut c_void = ptr::null_mut();
        if !real_view.is_null() {
            let get: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) = unsafe { std::mem::transmute(*((*(real_view as *const *const usize)).add(7))) };
            unsafe { get(real_view, &mut res) };
            if !res.is_null() {
                let rel: unsafe extern "system" fn(*mut c_void) -> u32 = unsafe { std::mem::transmute(*((*(res as *const *const usize)).add(2))) };
                unsafe { rel(res) };
            }
        }
        glog!("[gpu] OMSetRenderTargets({n}, rtv {first:p}, its resource {res:p}, dsv {dsv:p}) draws since last present {}", DRAWS_SINCE_PRESENT.load(Ordering::Relaxed));
    }
    if !rtvs.is_null() {
        for (i, v) in views.iter_mut().enumerate().take(count) {
            *v = unwrap_object(unsafe { *rtvs.add(i) });
        }
    }
    unsafe { f(real, n, if rtvs.is_null() { ptr::null() } else { views.as_ptr() }, unwrap_object(dsv)) };
}

fn kind_of(wrapper: *mut c_void) -> u8 {
    unsafe { *((wrapper as *const usize).add(2)) as u8 }
}

/// `QueryInterface` on a wrapper: the answer is wrapped the same way, because the title uses the
/// result with the same (older) vtable layout.
unsafe extern "system" fn wrapped_qi(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    let real = real_of(this);
    let f: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT = unsafe { std::mem::transmute(*(*(real as *const *const usize))) };
    let hr = unsafe { f(real, iid, out) };
    if hr.0 == 0 {
        let answer = unsafe { *out };
        if answer == real {
            // The same object: hand out the wrapper itself (one more reference was taken on the real one).
            unsafe { *out = this };
        } else {
            unsafe { *out = wrap_dxgi(answer, kind_of(this)) };
        }
    }
    hr
}

/// `ID3D11View::GetResource` as the older SDK calls it (console slot 8, desktop 7); the resource is
/// wrapped as a resource.
unsafe extern "system" fn view_get_resource_wrapped(this: *mut c_void, out: *mut *mut c_void) {
    let real = real_of(this);
    let f: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) = unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(7))) };
    unsafe { f(real, out) };
    if !out.is_null() && !unsafe { *out }.is_null() {
        unsafe { *out = wrap_dxgi(*out, 3) };
    }
}

/// `IDXGISwapChain::Present(sync, flags)` as the older SDK calls it (console slot 9, desktop 8).
unsafe extern "system" fn dxgi_present_wrapped(this: *mut c_void, sync: u32, flags: u32) -> HRESULT {
    let real = real_of(this);
    if std::env::var_os("XB1_READBACK").is_some() {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        if n == 100 || n == 700 || n == 1400 {
            readback_stats(&format!("before present {n}"));
        }
    }
    let f: unsafe extern "system" fn(*mut c_void, u32, u32) -> HRESULT = unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(8))) };
    let hr = unsafe { f(real, sync, flags) };
    static LOGS: AtomicU32 = AtomicU32::new(0);
    if LOGS.fetch_add(1, Ordering::Relaxed) < 5 {
        glog!("[gpu] Present(sync {sync}, flags {flags:#x}) -> {:#x}", hr.0 as u32);
    }
    let frame = FRAMES.fetch_add(1, Ordering::Relaxed);
    let drawn = DRAWS_SINCE_PRESENT.swap(0, Ordering::Relaxed);
    if frame < 6 || frame % 300 == 0 {
        glog!("[gpu] frame {frame}: {drawn} draw call(s) since the last Present (last: indexed {} count {} start {})", LAST_DRAW[0].load(Ordering::Relaxed), LAST_DRAW[1].load(Ordering::Relaxed), LAST_DRAW[2].load(Ordering::Relaxed));
    }
    if frame == 150 {
        report_context_changes();
    }
    crate::display::limit_frame_rate();
    hr
}

/// Older-XDK embedded index 12 (+0x440): `Draw(vertex count, first vertex)` (desktop 13), seen as (4, 0, ..).
unsafe extern "system" fn ctx_draw_old(this: *mut c_void, count: u32, start: u32) {
    static LOGS: AtomicU32 = AtomicU32::new(0);
    if LOGS.fetch_add(1, Ordering::Relaxed) < 4 {
        glog!("[gpu] Draw({count}, {start})");
    }
    draw_trace("plain", count, start);
    LAST_DRAW[0].store(0, Ordering::Relaxed);
    LAST_DRAW[1].store(count, Ordering::Relaxed);
    LAST_DRAW[2].store(start, Ordering::Relaxed);
    sync_before_draw(this);
    let real = unsafe { real_context(this) };
    // The topology is console-side state the layer does not read yet: a plain `Draw` of four vertices is a
    // quad as a triangle strip (the title and logo quads showed as one diagonal triangle as a list).
    {
        static COUNTS: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];
        if count < 8 && COUNTS[count as usize].fetch_add(1, Ordering::Relaxed) == 0 {
            glog!("[gpu] first plain Draw with {count} vertices");
        }
        let topology: u32 = if count == 4 { 5 } else { 4 }; // TRIANGLESTRIP or TRIANGLELIST
        let t: unsafe extern "system" fn(*mut c_void, u32) = unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(24))) };
        unsafe { t(real, topology) };
    }
    let f: unsafe extern "system" fn(*mut c_void, u32, u32) = unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(13))) };
    unsafe { f(real, count, start) };
}

/// The swap chain's buffer 0 as the title got it (a real desktop resource).
static BACKBUFFER: AtomicUsize = AtomicUsize::new(0);

/// Reads the back buffer back and reports how many pixels are not black and the first few values
/// (`XB1_READBACK=1`; a debugging aid to tell "the draws produced black" from "the picture is lost").
fn readback_stats(tag: &str) {
    use windows::core::Interface;
    let raw = BACKBUFFER.load(Ordering::Relaxed) as *mut c_void;
    if raw.is_null() {
        return;
    }
    let Some(tex) = (unsafe { ID3D11Texture2D::from_raw_borrowed(&raw) }) else { return };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { tex.GetDesc(&mut desc) };
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    desc.MiscFlags = 0;
    desc.SampleDesc.Count = 1;
    desc.SampleDesc.Quality = 0;
    let Ok(device) = (unsafe { tex.GetDevice() }) else { return };
    let mut staging: Option<ID3D11Texture2D> = None;
    if unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }.is_err() {
        glog!("[gpu] readback: could not create the staging texture");
        return;
    }
    let staging = staging.unwrap();
    let Ok(ctx) = (unsafe { device.GetImmediateContext() }) else { return };
    unsafe { ctx.CopyResource(&staging, tex) };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    if unsafe { ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.is_err() {
        glog!("[gpu] readback: Map failed");
        return;
    }
    let (w, h) = (desc.Width as usize, desc.Height as usize);
    let mut lit = 0usize;
    let mut sample = [0u8; 16];
    for y in 0..h {
        let row = unsafe { std::slice::from_raw_parts((mapped.pData as *const u8).add(y * mapped.RowPitch as usize), w * 4) };
        for x in 0..w {
            if row[x * 4] != 0 || row[x * 4 + 1] != 0 || row[x * 4 + 2] != 0 {
                if lit == 0 {
                    sample.copy_from_slice(&row[x * 4..x * 4 + 4].iter().chain(row[..12].iter()).copied().collect::<Vec<u8>>()[..16]);
                }
                lit += 1;
            }
        }
    }
    unsafe { ctx.Unmap(&staging, 0) };
    glog!("[gpu] readback {tag}: {w}x{h} format {} - {lit} non-black pixel(s), first {:02x?}", desc.Format.0, &sample[..4]);
}

/// Older-XDK embedded index 43: `RSSetScissorRects(count, rects)` (desktop 45); the first calls log the rectangles.
unsafe extern "system" fn ctx_scissor_old(this: *mut c_void, count: u32, rects: *const i32) {
    static LOGS: AtomicU32 = AtomicU32::new(0);
    if LOGS.fetch_add(1, Ordering::Relaxed) < 6 && !rects.is_null() && count >= 1 {
        let r = unsafe { std::slice::from_raw_parts(rects, 4) };
        glog!("[gpu] RSSetScissorRects({count}): left {} top {} right {} bottom {}", r[0], r[1], r[2], r[3]);
    }
    let real = unsafe { real_context(this) };
    let f: unsafe extern "system" fn(*mut c_void, u32, *const i32) = unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(45))) };
    unsafe { f(real, count, rects) };
}

/// Older-XDK embedded index 51: `ClearDepthStencilView(dsv, flags, depth, stencil)` (desktop 53). The depth
/// and stencil the title passes are not trusted yet (the fourth argument is a leftover pointer), so the
/// clear uses depth 1.0 and stencil 0, the usual values.
unsafe extern "system" fn ctx_clear_dsv_old(this: *mut c_void, dsv: *mut c_void, flags: u32, depth: f32, stencil: u64) {
    static LOGS: AtomicU32 = AtomicU32::new(0);
    if LOGS.fetch_add(1, Ordering::Relaxed) < 4 {
        glog!("[gpu] ClearDepthStencilView(dsv {dsv:p}, flags {flags:#x}, depth {depth}, stencil raw {stencil:#x})");
    }
    let real = unsafe { real_context(this) };
    let f: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, f32, u8) = unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(53))) };
    unsafe { f(real, unwrap_object(dsv), flags, 1.0, 0) };
}

/// Older-XDK state setters (blend 33 -> 35, depth-stencil 34 -> 36, rasterizer 41 -> 43): `(state, a3, a4)`.
/// `XB1_DEFAULT_STATES=1` replaces the state object with NULL (the desktop defaults) to test whether the
/// title's state objects are the reason nothing shows.
unsafe extern "system" fn ctx_state_old<const DESK: usize>(this: *mut c_void, state: *mut c_void, a3: usize, a4: usize) {
    let real = unsafe { real_context(this) };
    let state = if env_flag("XB1_DEFAULT_STATES") { ptr::null_mut() } else { unwrap_object(state) };
    if !state.is_null() {
        restore_object_vtable(state);
    }
    let f: unsafe extern "system" fn(*mut c_void, *mut c_void, usize, usize) = unsafe { std::mem::transmute(*((*(real as *const *const usize)).add(DESK))) };
    unsafe { f(real, state, a3, a4) };
    // The title keeps calling methods on the state object (GetDesc), so it gets its extended vtable back.
    if !state.is_null() {
        extend_object(state, 8);
        older_sdk_state_get_desc(state);
    }
}
