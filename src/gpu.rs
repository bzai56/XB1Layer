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
        }
    }
    p
}

/// One logging stub per embedded slot; shared by every context proxy.
fn embedded_stubs() -> &'static Vec<usize> {
    static STUBS: OnceLock<Vec<usize>> = OnceLock::new();
    STUBS.get_or_init(|| {
        let mut arena = Box::new(crate::StubArena::new());
        let stubs = (0..EMBED_SLOTS)
            .map(|i| {
                let offset = EMBED_BASE + i * 8;
                if offset >= EMBED_FILL_FROM {
                    arena.make_trace(format!("[gpu] ID3D11DeviceContextX embedded function at +{offset:#x}"), ret_zero as usize as u64) as usize
                } else {
                    0
                }
            })
            .collect();
        Box::leak(arena);
        stubs
    })
}

static DEVICE_PROXY: AtomicUsize = AtomicUsize::new(0);
static CONTEXT_PROXY: AtomicUsize = AtomicUsize::new(0);

extern "system" fn ret_zero() -> usize {
    0
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
    let mut stubs = Box::new(crate::StubArena::new());

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
        v[i] = stubs.make_trace(format!("[gpu] {} extension slot {i}", kind_name(kind)), ret_zero as usize as u64) as usize;
    }
    Box::leak(stubs);
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
