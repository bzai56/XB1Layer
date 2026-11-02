//! The display path for titles that create their swap chain with `IDXGIFactory2::CreateSwapChainForCoreWindow`.
//!
//! The shell's `CoreWindow` is not a real window, so the real DXGI rejects it. The shared vtable of
//! the real `IDXGIFactory2` (all factories use the same one) gets slot 16 patched: the call is
//! turned into `CreateSwapChainForHwnd` (slot 15) on a native window that this module creates on
//! its own message-pump thread. A proxy device passed in is unwrapped first.

use std::ffi::c_void;
use std::sync::OnceLock;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

#[link(name = "user32")]
unsafe extern "system" {
    fn CreateWindowExW(ex: u32, class: *const u16, name: *const u16, style: u32, x: i32, y: i32, w: i32, h: i32, parent: *mut c_void, menu: *mut c_void, inst: *mut c_void, param: *mut c_void) -> *mut c_void;
    fn GetMessageW(msg: *mut u8, hwnd: *mut c_void, min: u32, max: u32) -> i32;
    fn TranslateMessage(msg: *const u8) -> i32;
    fn DispatchMessageW(msg: *const u8) -> isize;
    fn DefWindowProcW(hwnd: *mut c_void, msg: u32, w: usize, l: isize) -> isize;
    fn RegisterClassW(wc: *const WndClass) -> u16;
    fn ShowWindow(hwnd: *mut c_void, cmd: i32) -> i32;
    fn LoadCursorW(inst: *mut c_void, name: usize) -> *mut c_void;
    fn AdjustWindowRect(rect: *mut [i32; 4], style: u32, menu: i32) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn VirtualProtect(addr: *mut c_void, size: usize, new: u32, old: *mut u32) -> i32;
    fn GetModuleHandleW(name: *const u16) -> *mut c_void;
}

#[repr(C)]
struct WndClass {
    style: u32,
    proc_: unsafe extern "system" fn(*mut c_void, u32, usize, isize) -> isize,
    cls_extra: i32,
    wnd_extra: i32,
    instance: *mut c_void,
    icon: *mut c_void,
    cursor: *mut c_void,
    background: *mut c_void,
    menu: *const u16,
    class_name: *const u16,
}

unsafe extern "system" fn wnd_proc(hwnd: *mut c_void, msg: u32, w: usize, l: isize) -> isize {
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The output window (created on first use), as a raw HWND value.
fn output_window(width: i32, height: i32) -> usize {
    static HWND_VALUE: OnceLock<usize> = OnceLock::new();
    *HWND_VALUE.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<usize>();
        std::thread::spawn(move || unsafe {
            let class = wide("XB1LayerWindow");
            let title = wide("XB1Layer");
            let inst = GetModuleHandleW(std::ptr::null());
            let wc = WndClass {
                style: 0x0003, // CS_HREDRAW | CS_VREDRAW
                proc_: wnd_proc,
                cls_extra: 0,
                wnd_extra: 0,
                instance: inst,
                icon: std::ptr::null_mut(),
                cursor: LoadCursorW(std::ptr::null_mut(), 32512), // IDC_ARROW
                background: std::ptr::null_mut(),
                menu: std::ptr::null(),
                class_name: class.as_ptr(),
            };
            RegisterClassW(&wc);
            let style = 0x00CF_0000; // WS_OVERLAPPEDWINDOW
            let mut rect = [0, 0, width, height];
            AdjustWindowRect(&mut rect, style, 0);
            let hwnd = CreateWindowExW(0, class.as_ptr(), title.as_ptr(), style, 100, 100, rect[2] - rect[0], rect[3] - rect[1], std::ptr::null_mut(), std::ptr::null_mut(), inst, std::ptr::null_mut());
            ShowWindow(hwnd, 5); // SW_SHOW
            let _ = tx.send(hwnd as usize);
            let mut msg = [0u8; 48];
            while GetMessageW(msg.as_mut_ptr(), std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(msg.as_ptr());
                DispatchMessageW(msg.as_ptr());
            }
        });
        rx.recv().unwrap_or(0)
    })
}

static ORIGINAL_FOR_HWND: OnceLock<usize> = OnceLock::new();
static ORIGINAL_PRESENT1: OnceLock<usize> = OnceLock::new();

/// The console presents with a sync interval of 1 on a 60 Hz output, so titles are written for a
/// locked 60 frames per second and run too fast on a faster monitor (a vsync only paces to the
/// monitor). Sleeps after each present until the next 1/60 s slot. `XB1_FPS=<n>` changes the rate,
/// `XB1_FPS=0` turns the limit off.
pub fn limit_frame_rate() {
    use std::time::{Duration, Instant};
    static FPS: OnceLock<f64> = OnceLock::new();
    static NEXT: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
    let fps = *FPS.get_or_init(|| std::env::var("XB1_FPS").ok().and_then(|v| v.parse().ok()).unwrap_or(60.0));
    if fps <= 0.0 {
        return;
    }
    let period = Duration::from_secs_f64(1.0 / fps);
    let mut next = NEXT.lock().unwrap();
    let now = Instant::now();
    let target = match *next {
        Some(t) if t > now.checked_sub(period * 2).unwrap_or(now) => t,
        _ => now,
    };
    if target > now {
        let wait = target - now;
        if wait > Duration::from_millis(2) {
            std::thread::sleep(wait - Duration::from_millis(2));
        }
        while Instant::now() < target {
            std::hint::spin_loop();
        }
    }
    *next = Some(target + period);
    // Frames per second, logged a few times so the pacing can be checked.
    static COUNT: std::sync::Mutex<(Option<Instant>, u32, u32)> = std::sync::Mutex::new((None, 0, 0));
    let mut c = COUNT.lock().unwrap();
    let start = *c.0.get_or_insert(now);
    c.1 += 1;
    if now.duration_since(start) >= Duration::from_secs(1) {
        if c.2 < 5 {
            eprintln!("[display] {} frames in the last second", c.1);
            c.2 += 1;
        }
        *c = (Some(now), 0, c.2);
    }
}

/// `IDXGISwapChain1::Present1(this, sync, flags, params)`: the title passes NULL parameters, which this
/// DXGI build dereferences unguarded (`mov rax, [r9+0x10]`), so an empty structure is substituted.
unsafe extern "system" fn present1(this: *mut c_void, sync: u32, flags: u32, params: *const c_void) -> i32 {
    // DXGI_PRESENT_PARAMETERS: DirtyRectsCount, pDirtyRects, pScrollRect, pScrollOffset (all zero).
    let empty = [0usize; 4];
    let p = if params.is_null() { empty.as_ptr() as *const c_void } else { params };
    let orig: unsafe extern "system" fn(*mut c_void, u32, u32, *const c_void) -> i32 =
        unsafe { std::mem::transmute(*ORIGINAL_PRESENT1.get().unwrap()) };
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let hr = unsafe { orig(this, sync, flags, p) };
    limit_frame_rate();
    if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 3 {
        eprintln!("[display] Present1(sync {sync}, flags {flags:#x}) -> {:#x}", hr as u32);
    }
    hr
}

/// Patches the swap chain class's `Present1` once (all instances share the vtable).
unsafe fn patch_present1(swap_chain: *mut c_void) {
    if ORIGINAL_PRESENT1.get().is_some() || swap_chain.is_null() {
        return;
    }
    let vtbl = unsafe { *(swap_chain as *const *mut usize) };
    let slot = unsafe { vtbl.add(22) };
    let _ = ORIGINAL_PRESENT1.set(unsafe { *slot });
    let mut old = 0u32;
    if unsafe { VirtualProtect(slot as *mut c_void, 8, 0x04, &mut old) } != 0 {
        unsafe { *slot = present1 as usize };
        unsafe { VirtualProtect(slot as *mut c_void, 8, old, &mut old) };
    }
}

/// `IDXGIFactory2::CreateSwapChainForCoreWindow(device, window, desc, restrict, out)`, redirected.
unsafe extern "system" fn create_for_core_window(this: *mut c_void, device: *mut c_void, _window: *mut c_void, desc: *const u32, restrict: *mut c_void, out: *mut *mut c_void) -> i32 {
    // DXGI_SWAP_CHAIN_DESC1: width, height, format, stereo, sample {count, quality}, usage, buffers, scaling, swap effect, alpha, flags.
    let (w, h) = if desc.is_null() { (1920, 1080) } else { unsafe { (*desc as i32, *desc.add(1) as i32) } };
    let hwnd = output_window(w.max(1).min(1920), h.max(1).min(1080));
    let real_device = unsafe { crate::gpu::unwrap_device(device) };
    eprintln!("[display] device {device:p} -> {real_device:p} (unwrapped: {})", device != real_device);
    let create_hwnd: unsafe extern "system" fn(*mut c_void, *mut c_void, usize, *const u32, *const c_void, *mut c_void, *mut *mut c_void) -> i32 =
        unsafe { std::mem::transmute(*ORIGINAL_FOR_HWND.get().unwrap()) };
    let mut copy: Vec<u32> = if desc.is_null() { vec![0; 12] } else { unsafe { std::slice::from_raw_parts(desc, 12).to_vec() } };
    let asked = copy.clone();
    // XB1_SWAP_EFFECT=0 (DISCARD) or 1 (SEQUENTIAL), 3/4 flip models: an experiment knob.
    if let Some(e) = std::env::var("XB1_SWAP_EFFECT").ok().and_then(|v| v.parse::<u32>().ok()) {
        copy[9] = e;
    }
    // CoreWindow-only options: premultiplied/straight alpha is for composition swap chains.
    copy[11] &= 0xFFFF; // console-only flag bits (the title asked for 0x200000) are not valid on the desktop
    if copy[10] > 3 {
        copy[10] = 0;
    }
    let mut hr = unsafe { create_hwnd(this, real_device, hwnd, copy.as_ptr(), std::ptr::null(), restrict, out) };
    let first = hr;
    for attempt in 0..3 {
        if hr >= 0 {
            break;
        }
        match attempt {
            0 => copy[8] = 0,     // Scaling STRETCH
            1 => copy[11] = 0,    // no flags at all
            _ => copy[9] = 4,     // FLIP_DISCARD
        }
        hr = unsafe { create_hwnd(this, real_device, hwnd, copy.as_ptr(), std::ptr::null(), restrict, out) };
    }
    eprintln!("[display] asked for {:x?}", asked);
    eprintln!("[display] CreateSwapChainForCoreWindow -> window {hwnd:#x} {w}x{h} fmt {:#x} buffers {} effect {}: {:#x} (first try {:#x})", copy[2], copy[7], copy[9], hr as u32, first as u32);
    if hr >= 0 && !out.is_null() {
        let sc = unsafe { *out };
        let vt = if sc.is_null() { 0 } else { unsafe { *(sc as *const usize) } };
        eprintln!("[display] swap chain object {sc:p}, vtable {vt:#x}");
        unsafe { patch_present1(sc) };
        if crate::VCCORLIB110.load(std::sync::atomic::Ordering::Relaxed) {
            unsafe { *out = crate::gpu::wrap_dxgi(sc, 2) };
        }
    }
    hr
}

/// Patches the real `IDXGIFactory2` vtable (shared by all factories). Safe to call repeatedly.
pub fn install() {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory2>() else {
            eprintln!("[display] could not create a DXGI factory to patch");
            return;
        };
        let vtbl = *(factory.as_raw() as *const *mut usize);
        let _ = ORIGINAL_FOR_HWND.set(*vtbl.add(15));
        let slot = vtbl.add(16);
        let mut old = 0u32;
        if VirtualProtect(slot as *mut c_void, 8, 0x04, &mut old) != 0 {
            *slot = create_for_core_window as usize;
            VirtualProtect(slot as *mut c_void, 8, old, &mut old);
            eprintln!("[display] IDXGIFactory2::CreateSwapChainForCoreWindow redirected to a native window");
        }
        std::mem::forget(factory);
    });
}
