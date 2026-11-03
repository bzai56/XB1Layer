//! Reads a Sony DualSense (USB or Bluetooth) straight through the HID API, because Windows does not
//! expose it as an XInput device and its winmm joystick entry does not change. A background thread
//! finds the device, reads input reports and keeps the latest state, already converted to the Xbox
//! `GamepadButtons` bits and axes the title expects.

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

#[repr(C)]
struct Guid {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}

#[repr(C)]
struct InterfaceData {
    size: u32,
    class: Guid,
    flags: u32,
    reserved: usize,
}

#[repr(C)]
struct HidAttributes {
    size: u32,
    vendor: u16,
    product: u16,
    version: u16,
}

#[link(name = "setupapi")]
unsafe extern "system" {
    fn SetupDiGetClassDevsW(class: *const Guid, enumerator: *const u16, parent: *mut c_void, flags: u32) -> *mut c_void;
    fn SetupDiEnumDeviceInterfaces(set: *mut c_void, info: *mut c_void, class: *const Guid, index: u32, data: *mut InterfaceData) -> i32;
    fn SetupDiGetDeviceInterfaceDetailW(set: *mut c_void, data: *mut InterfaceData, detail: *mut u8, size: u32, required: *mut u32, info: *mut c_void) -> i32;
    fn SetupDiDestroyDeviceInfoList(set: *mut c_void) -> i32;
}

#[link(name = "hid")]
unsafe extern "system" {
    fn HidD_GetHidGuid(guid: *mut Guid);
    fn HidD_GetAttributes(device: *mut c_void, attributes: *mut HidAttributes) -> u8;
}

unsafe extern "system" {
    fn CreateFileW(name: *const u16, access: u32, share: u32, security: *mut c_void, disposition: u32, flags: u32, template: *mut c_void) -> *mut c_void;
    fn ReadFile(file: *mut c_void, buffer: *mut u8, bytes: u32, read: *mut u32, overlapped: *mut c_void) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

/// Latest state: `(Xbox button bits, [left trigger, right trigger, left x, left y, right x, right y])`.
static STATE: Mutex<Option<(u32, [f32; 6])>> = Mutex::new(None);
static STARTED: AtomicBool = AtomicBool::new(false);

/// Starts the reader thread once.
pub fn start() {
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::Builder::new().name("dualsense".into()).spawn(reader).ok();
}

/// The latest state, or `None` when no DualSense is connected.
pub fn state() -> Option<(u32, [f32; 6])> {
    *STATE.lock().unwrap()
}

fn open_dualsense() -> Option<*mut c_void> {
    unsafe {
        let mut guid = Guid { d1: 0, d2: 0, d3: 0, d4: [0; 8] };
        HidD_GetHidGuid(&mut guid);
        let set = SetupDiGetClassDevsW(&guid, std::ptr::null(), std::ptr::null_mut(), 0x12);
        if set as isize == -1 {
            return None;
        }
        let mut found = None;
        for index in 0.. {
            let mut data = InterfaceData { size: std::mem::size_of::<InterfaceData>() as u32, class: Guid { d1: 0, d2: 0, d3: 0, d4: [0; 8] }, flags: 0, reserved: 0 };
            if SetupDiEnumDeviceInterfaces(set, std::ptr::null_mut(), &guid, index, &mut data) == 0 {
                break;
            }
            let mut need = 0u32;
            SetupDiGetDeviceInterfaceDetailW(set, &mut data, std::ptr::null_mut(), 0, &mut need, std::ptr::null_mut());
            let mut detail = vec![0u8; need as usize];
            detail[..4].copy_from_slice(&8u32.to_le_bytes()); // cbSize of SP_DEVICE_INTERFACE_DETAIL_DATA_W on x64
            if SetupDiGetDeviceInterfaceDetailW(set, &mut data, detail.as_mut_ptr(), need, &mut need, std::ptr::null_mut()) == 0 {
                continue;
            }
            let path = detail[4..].as_ptr() as *const u16;
            // 0xC0000000 = read + write, share read + write, OPEN_EXISTING
            let h = CreateFileW(path, 0xC000_0000, 3, std::ptr::null_mut(), 3, 0, std::ptr::null_mut());
            if h as isize == -1 {
                continue;
            }
            let mut a = HidAttributes { size: std::mem::size_of::<HidAttributes>() as u32, vendor: 0, product: 0, version: 0 };
            // DualSense 0x0CE6, DualSense Edge 0x0DF2
            if HidD_GetAttributes(h, &mut a) != 0 && a.vendor == 0x054C && (a.product == 0x0CE6 || a.product == 0x0DF2) {
                found = Some(h);
                break;
            }
            CloseHandle(h);
        }
        SetupDiDestroyDeviceInfoList(set);
        found
    }
}

/// Converts an input report to the Xbox layout. USB report 0x01 (64 bytes), Bluetooth simple report
/// 0x01 (10 bytes) and Bluetooth full report 0x31 (78 bytes) are understood.
fn parse(b: &[u8]) -> Option<(u32, [f32; 6])> {
    let (sticks, l2, r2, b1, b2) = match (*b.first()?, b.len()) {
        (0x01, n) if n >= 60 => (&b[1..5], b[5], b[6], b[8], b[9]),
        (0x01, n) if n >= 10 => (&b[1..5], b[8], b[9], b[5], b[6]),
        (0x31, n) if n >= 12 => (&b[2..6], b[6], b[7], b[9], b[10]),
        _ => return None,
    };
    let mut bits = 0u32;
    let mut set = |on: bool, bit: u32| {
        if on {
            bits |= bit;
        }
    };
    set(b1 & 0x20 != 0, 16); // Cross -> A
    set(b1 & 0x40 != 0, 32); // Circle -> B
    set(b1 & 0x10 != 0, 64); // Square -> X
    set(b1 & 0x80 != 0, 128); // Triangle -> Y
    let hat = b1 & 0x0F; // 0 N, 1 NE, 2 E, 3 SE, 4 S, 5 SW, 6 W, 7 NW, 8 none
    set(matches!(hat, 7 | 0 | 1), 256);
    set(matches!(hat, 1..=3), 2048);
    set(matches!(hat, 3..=5), 512);
    set(matches!(hat, 5..=7), 1024);
    set(b2 & 0x01 != 0, 4096); // L1
    set(b2 & 0x02 != 0, 8192); // R1
    set(b2 & 0x10 != 0, 8); // Create -> View
    set(b2 & 0x20 != 0, 4); // Options -> Menu
    set(b2 & 0x40 != 0, 16384); // L3
    set(b2 & 0x80 != 0, 32768); // R3
    let axis = |v: u8| (v as f32 - 127.5) / 127.5;
    let lt = if l2 == 0 && b2 & 0x04 != 0 { 1.0 } else { l2 as f32 / 255.0 };
    let rt = if r2 == 0 && b2 & 0x08 != 0 { 1.0 } else { r2 as f32 / 255.0 };
    Some((bits, [lt, rt, axis(sticks[0]), -axis(sticks[1]), axis(sticks[2]), -axis(sticks[3])]))
}

fn reader() {
    let mut logged = false;
    loop {
        let Some(h) = open_dualsense() else {
            *STATE.lock().unwrap() = None;
            std::thread::sleep(std::time::Duration::from_secs(2));
            continue;
        };
        eprintln!("[input] DualSense opened through HID");
        let mut buf = [0u8; 128];
        loop {
            let mut n = 0u32;
            if unsafe { ReadFile(h, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null_mut()) } == 0 || n == 0 {
                break;
            }
            if !logged {
                eprintln!("[input] DualSense report id {:#x}, {} bytes", buf[0], n);
                logged = true;
            }
            if let Some(s) = parse(&buf[..n as usize]) {
                // XB1_DS_LOG=1: print the sticks and triggers when one moves, to check which stick a pad's axes land on.
                static LOG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                if *LOG.get_or_init(|| std::env::var_os("XB1_DS_LOG").is_some()) {
                    static LAST: Mutex<[f32; 6]> = Mutex::new([0.0; 6]);
                    let mut last = LAST.lock().unwrap();
                    if s.1.iter().zip(last.iter()).any(|(a, b)| (a - b).abs() > 0.15) {
                        eprintln!("[ds] LT {:.2} RT {:.2} | left stick x {:.2} y {:.2} | right stick x {:.2} y {:.2} | raw bytes 1..8 = {:02x?}", s.1[0], s.1[1], s.1[2], s.1[3], s.1[4], s.1[5], &buf[1..9.min(n as usize)]);
                        *last = s.1;
                    }
                }
                *STATE.lock().unwrap() = Some(s);
            }
        }
        unsafe { CloseHandle(h) };
        *STATE.lock().unwrap() = None;
    }
}
