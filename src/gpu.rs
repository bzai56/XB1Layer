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
const EMBED_FILL_FROM: usize = 0x18;

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
const EMBED_SHIFTED_TO: usize = 113;

/// `(embedded index, entry)` pairs that forward to the real context. Built once.
fn forwarded_entries() -> &'static Vec<(usize, usize)> {
    static ENTRIES: OnceLock<Vec<(usize, usize)>> = OnceLock::new();
    let entries = ENTRIES.get_or_init(|| {
        let thunks = unsafe { VirtualAlloc(ptr::null_mut(), 4096, 0x3000, 0x40) } as *mut u8;
        assert!(!thunks.is_null(), "thunk allocation failed");
        let mut v = Vec::new();
        let index_of = |offset: usize| (offset - EMBED_BASE) / 8;
        for slot in 0..=EMBED_FORWARD_MAX {
            let entry = match slot {
                0 => proxy_qi as usize,
                1 => proxy_add_ref as usize,
                2 => proxy_release as usize,
                3 => context_get_device as usize,
                s => forward_thunk(unsafe { thunks.add(s * 16) }, s),
            };
            v.push((index_of(EMBED_TABLE + slot * 8), entry));
        }
        for console_slot in EMBED_SHIFTED_FROM..=EMBED_SHIFTED_TO {
            let thunk = forward_thunk(unsafe { thunks.add((64 + console_slot - EMBED_SHIFTED_FROM) * 16) }, console_slot + 1);
            v.push((index_of(EMBED_TABLE + console_slot * 8), thunk));
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
        v[i] = forward_thunk(unsafe { thunks.add(i * 16) }, i);
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
        // Creation methods decoded from the title's call sites.
        v[59] = device_ext_59 as usize;
        v[64] = device_ext_64 as usize;
        v[65] = device_ext_65 as usize;
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
    let result = unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
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
