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
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

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
    if n < 3 {
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
const EMBED_VERIFIED: [usize; 5] = [7, 11, 14, 15, 23];

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

/// `(embedded index, entry)` pairs that forward to the real context. Built once.
fn forwarded_entries() -> &'static Vec<(usize, usize)> {
    static ENTRIES: OnceLock<Vec<(usize, usize)>> = OnceLock::new();
    let entries = ENTRIES.get_or_init(|| {
        let thunks = unsafe { VirtualAlloc(ptr::null_mut(), 16384, 0x3000, 0x40) } as *mut u8;
        assert!(!thunks.is_null(), "thunk allocation failed");
        let mut v = Vec::new();
        let index_of = |offset: usize| (offset - EMBED_BASE) / 8;
        for slot in 0..=EMBED_FORWARD_MAX {
            // Only indices with direct evidence forward: 11 and 23 (SetShader with NULL), 14 and 15
            // (Map/Unmap). Index 8 was assumed to be PSSetShaderResources and is called as
            // (pointer, 0, 0x76b40), so the console table is not a plain copy of the desktop one.
            if slot > 3 && !EMBED_VERIFIED.contains(&slot) {
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
            let thunk = forward_context_thunk(unsafe { thunks.add((64 + console_slot - EMBED_SHIFTED_FROM) * 32) }, console_slot + 1);
            v.push((index_of(EMBED_TABLE + console_slot * 8), thunk));
        }
        // Draws and dispatches first mirror the placement buffers (see sync_before_draw).
        let synced = unsafe { VirtualAlloc(ptr::null_mut(), 4096, 0x3000, 0x40) } as *mut u8;
        assert!(!synced.is_null(), "thunk allocation failed");
        for (n, desktop_slot) in DRAW_SLOTS.iter().enumerate() {
            let console_slot = if *desktop_slot > EMBED_FORWARD_MAX { desktop_slot - 1 } else { *desktop_slot };
            let target = index_of(EMBED_TABLE + console_slot * 8);
            let thunk = synced_forward_thunk(unsafe { synced.add(n * 128) }, *desktop_slot);
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
            // A command buffer for the packets the title writes into the context, and the
            // function it calls when the buffer is full.
            let buffer = VirtualAlloc(ptr::null_mut(), CMD_BUFFER_BYTES, 0x3000, 0x04) as usize;
            assert!(buffer != 0, "command buffer allocation failed");
            (*p).cmd_base = buffer;
            (*p).embedded[(CMD_CURSOR - EMBED_BASE) / 8] = buffer;
            (*p).embedded[(CMD_END - EMBED_BASE) / 8] = buffer + CMD_BUFFER_BYTES;
            (*p).embedded[(CMD_GROW - EMBED_BASE) / 8] = context_grow_command_buffer as usize;
        }
    }
    p
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

/// Called by every argument-logging stub. Logs the first calls of each stub, then only
/// powers of two, so a function called every frame does not flood the log.
extern "system" fn arg_stub_hit(idx: u64, ret: u64, this: u64, a2: u64, a3: u64, a4: u64) {
    let (name, count) = {
        let Ok(mut v) = ARG_STUBS.try_lock() else { return };
        let e = &mut v[idx as usize];
        e.1 += 1;
        (e.0.clone(), e.1)
    };
    if count > 6 && !count.is_power_of_two() {
        return;
    }
    let (base, size) = crate::image_range();
    let mut line = crate::RawBuf::new();
    {
        use std::fmt::Write;
        let _ = write!(line, "{name} (call #{count}) this=");
    }
    show(this, &mut line);
    for (label, a) in [("a2", a2), ("a3", a3), ("a4", a4)] {
        use std::fmt::Write;
        let _ = write!(line, " {label}=");
        show(a, &mut line);
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
            unsafe { *out = real_out };
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
    let p = unsafe { &*(this as *const Proxy) };
    let f: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT =
        unsafe { std::mem::transmute(*((*(p.real as *const *const usize)).add(DEVICE_CREATE_DEFERRED_CONTEXT))) };
    let hr = unsafe { f(p.real, flags, out) };
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

const STAND_IN_SLOTS: usize = 128;

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
        let mut arena = ArgArena::new();
        let mut v = vec![0usize; STAND_IN_SLOTS];
        v[0] = stand_in_qi as usize;
        v[1] = stand_in_add_ref as usize;
        v[2] = stand_in_release as usize;
        for i in 3..STAND_IN_SLOTS {
            v[i] = arena.make(format!("[gpu] device-created object, vtable slot {i}"), ret_zero as usize as u64);
        }
        std::mem::forget(arena);
        v
    })
    .as_ptr()
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
extern "system" fn sync_before_draw(this: *mut c_void) {
    let real = unsafe { (*(this as *const Proxy)).real };
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
    let hr = unsafe { f(p.real, resource, desc, out) };
    if hr.0 == 0x80070057u32 as i32 && !desc.is_null() && !resource.is_null() {
        let retry = unsafe { f(p.real, resource, ptr::null(), out) };
        let n = FALLBACKS.fetch_add(1, Ordering::Relaxed);
        if n < 8 {
            glog!("[gpu] {name}: description rejected ({}), retry with no description -> {:#x}", safe_dump(desc as u64, 16), retry.0 as u32);
        }
        if retry.0 >= 0 {
            return retry;
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
        std::mem::forget(arena);
        // Buffers and textures also need their console-only MiscFlags bits removed.
        v[3] = device_create_buffer as usize;
        v[5] = device_create_texture2d as usize;
        v[6] = device_create_texture3d as usize;
        v[7] = device_create_srv as usize;
        v[8] = device_create_uav as usize;
        v[9] = device_create_rtv as usize;
        v[10] = device_create_dsv as usize;
        // Creation methods decoded from the title's call sites.
        v[59] = device_ext_59 as usize;
        v[64] = device_ext_64 as usize;
        v[65] = device_ext_65 as usize;
        v[62] = device_ext_62 as usize;
        v[73] = device_ext_73 as usize;
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
