//! `XB1_PROFILE=1`: a sampling profiler for the thread that presents frames. Every 2 ms it suspends that
//! thread, reads its instruction pointer and counts the module it is in; every 5 s it prints the shares and
//! the busiest addresses of the title and of the layer. Debugging aid for the frame rate.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

#[repr(C, align(16))]
struct Context([u8; 1232]);

unsafe extern "system" {
    fn OpenThread(access: u32, inherit: i32, tid: u32) -> *mut c_void;
    fn SuspendThread(h: *mut c_void) -> u32;
    fn ResumeThread(h: *mut c_void) -> u32;
    fn GetThreadContext(h: *mut c_void, ctx: *mut Context) -> i32;
    fn GetCurrentThreadId() -> u32;
    fn GetModuleHandleExA(flags: u32, addr: *const c_void, out: *mut *mut c_void) -> i32;
    fn GetModuleFileNameA(module: *mut c_void, buf: *mut u8, size: u32) -> u32;
    fn CloseHandle(h: *mut c_void) -> i32;
}

static STARTED: AtomicBool = AtomicBool::new(false);

/// Starts the sampler on the calling thread once, when `XB1_PROFILE` is set.
pub fn start_for_current_thread() {
    if std::env::var_os("XB1_PROFILE").is_none() || STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let tid = unsafe { GetCurrentThreadId() };
    std::thread::Builder::new().name("profile".into()).spawn(move || sampler(tid)).ok();
}

fn module_of(addr: usize) -> String {
    unsafe {
        let mut m: *mut c_void = std::ptr::null_mut();
        // GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | UNCHANGED_REFCOUNT
        if GetModuleHandleExA(0x4 | 0x2, addr as *const c_void, &mut m) == 0 || m.is_null() {
            return "(none)".into();
        }
        let mut buf = [0u8; 260];
        let n = GetModuleFileNameA(m, buf.as_mut_ptr(), 260) as usize;
        let path = String::from_utf8_lossy(&buf[..n]).to_string();
        path.rsplit(char::from(92u8)).next().unwrap_or("?").to_string()
    }
}

fn sampler(tid: u32) {
    // THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION
    let h = unsafe { OpenThread(0x2 | 0x8 | 0x40, 0, tid) };
    if h.is_null() {
        return;
    }
    let (base, size) = crate::image_range();
    let mut modules: HashMap<String, u32> = HashMap::new();
    let mut addrs: HashMap<(bool, usize), u32> = HashMap::new();
    let mut cache: HashMap<usize, String> = HashMap::new();
    let mut total = 0u32;
    let mut last_print = std::time::Instant::now();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(2));
        let mut ctx = Context([0; 1232]);
        // ContextFlags = CONTEXT_CONTROL (0x100001)
        ctx.0[0x30..0x34].copy_from_slice(&0x0010_0001u32.to_le_bytes());
        unsafe {
            if SuspendThread(h) == u32::MAX {
                break;
            }
            let ok = GetThreadContext(h, &mut ctx);
            ResumeThread(h);
            if ok == 0 {
                continue;
            }
        }
        let rip = u64::from_le_bytes(ctx.0[0xF8..0x100].try_into().unwrap()) as usize;
        let in_title = rip >= base && rip < base + size;
        let key = rip >> 16;
        let name = if in_title { "title".to_string() } else { cache.entry(key).or_insert_with(|| module_of(rip)).clone() };
        *modules.entry(name.clone()).or_default() += 1;
        if in_title {
            *addrs.entry((true, rip & !0xF)).or_default() += 1;
        } else if name.starts_with("xb1layer") {
            // The offset inside the layer's own image, for looking up in a linker map (`/MAP`).
            let mut m: *mut c_void = std::ptr::null_mut();
            unsafe { GetModuleHandleExA(0x4 | 0x2, rip as *const c_void, &mut m) };
            *addrs.entry((false, (rip - m as usize) & !0xF)).or_default() += 1;
        }
        total += 1;
        if last_print.elapsed().as_secs() >= 5 {
            let mut mv: Vec<_> = modules.iter().collect();
            mv.sort_by(|a, b| b.1.cmp(a.1));
            let shares: Vec<String> = mv.iter().take(7).map(|(m, c)| format!("{m} {:.0}%", **c as f32 * 100.0 / total as f32)).collect();
            eprintln!("[prof] {total} samples: {}", shares.join(", "));
            let mut av: Vec<_> = addrs.iter().collect();
            av.sort_by(|a, b| b.1.cmp(a.1));
            let top: Vec<String> = av.iter().take(6).map(|((t, a), c)| if *t { format!("title+{:#x} x{c}", a - base) } else { format!("xb1layer+{a:#x} x{c}") }).collect();
            eprintln!("[prof]   hottest: {}", top.join(", "));
            modules.clear();
            addrs.clear();
            total = 0;
            last_print = std::time::Instant::now();
        }
    }
    unsafe { CloseHandle(h) };
}
