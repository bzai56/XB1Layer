//! xb1layer: PE64 loader core for an Xbox One (GDK) compatibility layer.
//!
//! Maps an x64 PE image manually, applies relocations, resolves imports
//! against real Windows DLLs where possible, and routes everything else
//! (xgameruntime, GDK/XDK system libs) to logging stubs so we can see
//! which console APIs a title needs.
//!
//! Usage: xb1layer <file.exe> [--run]

mod display;
mod dualsense;
mod gpu;
mod profile;
mod tlsdll;
mod winrt;

use std::collections::{BTreeMap, HashSet};
use std::ffi::{c_void, CString};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::{env, fs, process, ptr};

unsafe extern "system" {
    fn VirtualAlloc(addr: *mut c_void, size: usize, ty: u32, prot: u32) -> *mut c_void;
    fn CreateFileA(name: *const i8, access: u32, share: u32, sec: *mut c_void, disp: u32, flags: u32, template: *mut c_void) -> *mut c_void;
    fn CreateFileMappingA(file: *mut c_void, sec: *mut c_void, prot: u32, hi: u32, lo: u32, name: *const i8) -> *mut c_void;
    fn MapViewOfFile(map: *mut c_void, access: u32, hi: u32, lo: u32, bytes: usize) -> *mut c_void;
    fn CreateFileW(name: *const u16, access: u32, share: u32, sec: *mut c_void, disp: u32, flags: u32, tmpl: *mut c_void) -> *mut c_void;
    fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
    fn CreateFile2(name: *const u16, access: u32, share: u32, disp: u32, params: *mut c_void) -> *mut c_void;
    fn FindFirstFileExA(name: *const i8, level: u32, data: *mut c_void, op: u32, filter: *mut c_void, flags: u32) -> *mut c_void;
    fn GetFileAttributesExA(name: *const i8, level: u32, data: *mut c_void) -> i32;
    fn GetFileAttributesW(name: *const u16) -> u32;
    fn GetFileAttributesExW(name: *const u16, level: u32, data: *mut c_void) -> i32;
    fn FindFirstFileW(name: *const u16, data: *mut c_void) -> *mut c_void;
    fn ReadFile(h: *mut c_void, buf: *mut c_void, n: u32, read: *mut u32, ov: *mut c_void) -> i32;
    fn GetFileInformationByHandleEx(h: *mut c_void, class: u32, buf: *mut c_void, size: u32) -> i32;
    fn GetLastError() -> u32;
    fn CreateThreadpoolIo(file: *mut c_void, callback: usize, context: *mut c_void, env: *mut c_void) -> *mut c_void;
    fn CreateThread(attrs: *mut c_void, stack: usize, routine: usize, param: *mut c_void, flags: u32, tid: *mut u32) -> *mut c_void;
    fn VirtualFree(addr: *mut c_void, size: usize, ty: u32) -> i32;
    fn TerminateProcess(process: *mut c_void, code: u32) -> i32;
    fn ExitProcess(code: u32) -> !;
    fn VirtualProtect(addr: *mut c_void, size: usize, new: u32, old: *mut u32) -> i32;
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> *mut c_void;
    fn Thread32First(snapshot: *mut c_void, entry: *mut u32) -> i32;
    fn Thread32Next(snapshot: *mut c_void, entry: *mut u32) -> i32;
    fn OpenThread(access: u32, inherit: i32, tid: u32) -> *mut c_void;
    fn SuspendThread(thread: *mut c_void) -> u32;
    fn ResumeThread(thread: *mut c_void) -> u32;
    fn SetThreadContext(thread: *mut c_void, ctx: *const c_void) -> i32;
    fn WaitForSingleObject(h: *mut c_void, ms: u32) -> u32;
    fn CreateEventW(attrs: *mut c_void, manual: i32, initial: i32, name: *const u16) -> *mut c_void;
    fn CreateEventA(attrs: *mut c_void, manual: i32, initial: i32, name: *const i8) -> *mut c_void;
    fn SetEvent(h: *mut c_void) -> i32;
    fn CreateSemaphoreExW(attrs: *mut c_void, initial: i32, max: i32, name: *const u16, flags: u32, access: u32) -> *mut c_void;
    fn ReleaseSemaphore(h: *mut c_void, count: i32, previous: *mut i32) -> i32;
    fn WaitForMultipleObjects(count: u32, handles: *const *mut c_void, all: i32, ms: u32) -> u32;
    fn WaitForSingleObjectEx(h: *mut c_void, ms: u32, alertable: i32) -> u32;
    fn SetProcessAffinityMask(process: *mut c_void, mask: usize) -> i32;
    fn RtlLookupFunctionEntry(pc: u64, image_base: *mut u64, history: *mut c_void) -> *mut c_void;
    fn RtlVirtualUnwind(kind: u32, image_base: u64, pc: u64, entry: *mut c_void, ctx: *mut c_void, handler_data: *mut *mut c_void, establisher: *mut u64, ctx_ptrs: *mut c_void) -> *mut c_void;
    fn CloseHandle(h: *mut c_void) -> i32;
    fn GetCurrentProcessId() -> u32;
    fn DefineDosDeviceW(flags: u32, name: *const u16, target: *const u16) -> i32;
    fn QueryDosDeviceW(name: *const u16, target: *mut u16, max: u32) -> u32;
    fn GetModuleHandleA(name: *const i8) -> *mut c_void;
    fn HeapCreate(options: u32, initial: usize, max: usize) -> *mut c_void;
    fn HeapValidate(heap: *mut c_void, flags: u32, mem: *const c_void) -> i32;
    fn GetProcessHeap() -> *mut c_void;
    fn HeapAlloc(heap: *mut c_void, flags: u32, size: usize) -> *mut c_void;
    fn HeapFree(heap: *mut c_void, flags: u32, p: *mut c_void) -> i32;
    fn HeapSize(heap: *mut c_void, flags: u32, mem: *const c_void) -> usize;
    fn VirtualQuery(addr: *const c_void, info: *mut c_void, len: usize) -> usize;
    fn GetStdHandle(which: u32) -> *mut c_void;
    fn WriteFile(h: *mut c_void, buf: *const u8, n: u32, written: *mut u32, overlapped: *mut c_void) -> i32;
    fn GetCurrentThreadId() -> u32;
    fn GetCurrentProcess() -> *mut c_void;
    fn ReadProcessMemory(process: *mut c_void, addr: *const c_void, buf: *mut c_void, size: usize, read: *mut usize) -> i32;
    fn GetThreadContext(thread: *mut c_void, ctx: *mut c_void) -> i32;
    fn GetModuleHandleExA(flags: u32, addr: *const i8, out: *mut *mut c_void) -> i32;
    fn GetModuleFileNameA(module: *mut c_void, buf: *mut u8, size: u32) -> u32;
    fn RtlCaptureStackBackTrace(skip: u32, count: u32, frames: *mut *mut c_void, hash: *mut u32) -> u16;
    fn RtlAddFunctionTable(table: *const c_void, count: u32, base: u64) -> u8;
    fn AddVectoredExceptionHandler(first: u32, handler: extern "system" fn(*const *const u8) -> i32) -> *mut c_void;
    fn LoadLibraryA(name: *const i8) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const i8) -> *mut c_void;
}

const MEM_COMMIT_RESERVE: u32 = 0x3000;
const PAGE_RWX: u32 = 0x40;

static IMG_BASE: AtomicUsize = AtomicUsize::new(0);
static IMG_SIZE: AtomicUsize = AtomicUsize::new(0);

static TITLE_TID: AtomicU32 = AtomicU32::new(0);
static WATCH_SECS: AtomicU32 = AtomicU32::new(0);
static HEAPCHECK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Addresses where `int 0x29` (fast-fail) was replaced by a breakpoint (`--patch-fastfail`).
static FASTFAIL_SITES: [AtomicUsize; 512] = [const { AtomicUsize::new(0) }; 512];

/// Finds `mov ecx, imm32 (imm32 < 256); int 0x29` sequences in the mapped image and replaces the
/// `int 0x29` with `int3; nop`, so the crash logger can report the site and the fast-fail code
/// instead of the process dying silently. Returns how many sites were patched.
unsafe fn patch_fastfail_sites(base: *mut u8, size: usize) -> usize {
    let mut n = 0;
    let mut i = 5;
    while i + 2 <= size && n < FASTFAIL_SITES.len() {
        let p = unsafe { base.add(i) };
        // B9 xx 00 00 00 CD 29
        if unsafe { *p == 0xCD && *p.add(1) == 0x29 && *p.sub(5) == 0xB9 && *p.sub(3) == 0 && *p.sub(2) == 0 && *p.sub(1) == 0 } {
            unsafe {
                *p = 0xCC;
                *p.add(1) = 0x90;
            }
            FASTFAIL_SITES[n].store(p as usize, Ordering::Relaxed);
            n += 1;
        }
        i += 1;
    }
    n
}

/// Parses `--watch-sites=0x1B10F53,0x1B12D1D` (title RVAs) from the command line.
fn args_watch_sites() -> Option<Vec<usize>> {
    // Forza Horizon 3 (full game): nothing in the layer sets the game object's "render enabled" request (`[r15+0x4cc]` in the
    // main tick, title+0x172e4d0); until its trigger is found, the layer writes it at the tick's first pass. `XB1_FH3_BOOT=0`
    // turns this off. See docs/FINDINGS.md.
    if env::args().any(|a| a.to_ascii_lowercase().contains("forza_durango_release_final")) && env::var("XB1_FH3_BOOT").as_deref() != Ok("0") && !env::args().any(|a| a.starts_with("--watch-sites=")) {
        if env::var_os("XB1_POKE").is_none() {
            unsafe { env::set_var("XB1_POKE", "r15+0x4cc=1") };
        }
        return Some(vec![0x172e521]);
    }
    let arg = env::args().find_map(|a| a.strip_prefix("--watch-sites=").map(str::to_owned))?;
    Some(arg.split(',').filter_map(|s| usize::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()).collect())
}

/// One-shot watchpoints (`--watch-sites=0x1B10F53,...`, title RVAs): the first byte of the
/// instruction is replaced with `int3`; on the first hit the crash logger reports the registers
/// and the title's call chain, restores the byte and re-executes the instruction.
static WATCH_ADDR: [AtomicUsize; 256] = [const { AtomicUsize::new(0) }; 256];
static WATCH_ORIG: [AtomicUsize; 256] = [const { AtomicUsize::new(0) }; 256];
static WATCH_PENDING: [std::sync::atomic::AtomicBool; 256] = [const { std::sync::atomic::AtomicBool::new(false) }; 256];
/// `--watch-repeat`: watchpoints stay armed and report every hit (up to a cap) instead of once.
static WATCH_REPEAT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static WATCH_HITS: AtomicUsize = AtomicUsize::new(0);
/// `rcx` at the last repeat-mode watch hit (for `XB1_WATCHDOG_PEEK=@+off:len`).
static LAST_HIT_RCX: AtomicUsize = AtomicUsize::new(0);

unsafe fn plant_watch_sites(base: *mut u8, rvas: &[usize]) {
    for (i, rva) in rvas.iter().take(WATCH_ADDR.len()).enumerate() {
        let p = unsafe { base.add(*rva) };
        WATCH_ORIG[i].store(unsafe { *p } as usize, Ordering::Relaxed);
        WATCH_ADDR[i].store(p as usize, Ordering::Relaxed);
        unsafe { *p = 0xCC };
    }
}

/// `(base, size)` of the mapped title image, for modules that log title addresses.
pub fn image_range() -> (usize, usize) {
    (IMG_BASE.load(Ordering::Relaxed), IMG_SIZE.load(Ordering::Relaxed))
}

static SEEN_FAULTS: [AtomicUsize; 64] = [const { AtomicUsize::new(0) }; 64];
static SEEN_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Fixed-size formatting buffer so crash reporting never touches the heap
/// (the title may have crashed inside it, or corrupted it).
struct RawBuf {
    b: [u8; 4096],
    n: usize,
}

impl RawBuf {
    fn new() -> Self {
        Self { b: [0; 4096], n: 0 }
    }
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.b[..self.n]).unwrap_or("?")
    }
}

impl std::fmt::Write for RawBuf {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let k = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
        self.n += k;
        Ok(())
    }
}

fn raw_log(args: std::fmt::Arguments) {
    let mut w = RawBuf::new();
    let _ = std::fmt::write(&mut w, args);
    if w.n < w.b.len() {
        w.b[w.n] = b'\n';
        w.n += 1;
    }
    let mut written = 0u32;
    unsafe { WriteFile(GetStdHandle(-12i32 as u32), w.b.as_ptr(), w.n as u32, &mut written, ptr::null_mut()) };
}

macro_rules! rlog {
    ($($t:tt)*) => { raw_log(format_args!($($t)*)) };
}

/// Writes `title+RVA` or `module+offset` for an address, without allocating.
fn describe(addr: usize, out: &mut RawBuf) {
    use std::fmt::Write;
    let (base, size) = (IMG_BASE.load(Ordering::Relaxed), IMG_SIZE.load(Ordering::Relaxed));
    if addr >= base && addr < base + size {
        let _ = write!(out, "title+{:#x}", addr - base);
        return;
    }
    let mut m: *mut c_void = ptr::null_mut();
    let mut name = [0u8; 260];
    unsafe {
        // GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | UNCHANGED_REFCOUNT
        if GetModuleHandleExA(0x4 | 0x2, addr as *const i8, &mut m) == 0 {
            let _ = write!(out, "{addr:#x}");
            return;
        }
        let n = GetModuleFileNameA(m, name.as_mut_ptr(), name.len() as u32) as usize;
        let full = &name[..n];
        let start = full.iter().rposition(|&c| c == b'\\').map_or(0, |i| i + 1);
        let _ = write!(out, "{}+{:#x}", std::str::from_utf8(&full[start..]).unwrap_or("?"), addr - m as usize);
    }
}

#[repr(C, align(16))]
struct Context([u8; 1232]);

/// Freezes the title thread and prints where it is: RIP plus any return-address-looking
/// values on its stack that point into the title.
fn dump_title_thread() {
    dump_thread(TITLE_TID.load(Ordering::Relaxed));
}

/// Samples every thread of the process (a short stack summary each): what is everyone doing?
fn dump_all_threads() {
    unsafe {
        let snap = CreateToolhelp32Snapshot(0x4, 0);
        if snap as isize == -1 {
            return;
        }
        let mut entry = [0u32; 7];
        entry[0] = 28;
        let (me, pid) = (GetCurrentThreadId(), GetCurrentProcessId());
        let mut ok = Thread32First(snap, entry.as_mut_ptr());
        while ok != 0 {
            if entry[3] == pid && entry[2] != me {
                rlog!("=== thread {}{}", entry[2], if entry[2] == TITLE_TID.load(Ordering::Relaxed) { " (title main thread)" } else { "" });
                if std::env::var_os("XB1_WATCHDOG_WALK").is_some() {
                    walk_thread(entry[2]);
                } else {
                    dump_thread(entry[2]);
                }
            }
            ok = Thread32Next(snap, entry.as_mut_ptr());
        }
        CloseHandle(snap);
    }
}

/// A real stack walk of another thread (unwind tables, not a scan of the raw stack): `XB1_WATCHDOG_WALK=1`.
fn walk_thread(tid: u32) {
    unsafe {
        let h = OpenThread(0x1F_FFFF, 0, tid);
        if h.is_null() {
            rlog!("  cannot open thread");
            return;
        }
        SuspendThread(h);
        let mut c = Context([0; 1232]);
        *(c.0.as_mut_ptr().add(0x30) as *mut u32) = 0x10_000B; // CONTEXT_FULL
        let ok = GetThreadContext(h, c.0.as_mut_ptr() as *mut c_void);
        if ok == 0 {
            ResumeThread(h);
            rlog!("  GetThreadContext failed");
            return;
        }
        walk_context(&mut c);
        ResumeThread(h);
    }
}

/// Unwinds a thread context record (`c`) with the unwind tables and logs the frames.
unsafe fn walk_context(c: &mut Context) {
    {
        let (base, size) = (IMG_BASE.load(Ordering::Relaxed), IMG_SIZE.load(Ordering::Relaxed));
        let mut line = RawBuf::new();
        use std::fmt::Write;
        for depth in 0..24 {
            let rip = *(c.0.as_ptr().add(0xF8) as *const u64);
            if rip == 0 {
                break;
            }
            let mut image: u64 = 0;
            let entry = RtlLookupFunctionEntry(rip, &mut image, ptr::null_mut());
            if (rip as usize) >= base && (rip as usize) < base + size {
                let _ = write!(line, " title+{:#x}", rip as usize - base);
            } else {
                let mut d = RawBuf::new();
                describe(rip as usize, &mut d);
                let _ = write!(line, " {}", d.as_str());
            }
            if entry.is_null() {
                // a leaf: the return address is at the stack pointer
                let rsp = *(c.0.as_ptr().add(0x98) as *const u64);
                let mut ret = 0u64;
                let mut g = 0usize;
                if ReadProcessMemory(GetCurrentProcess(), rsp as *const c_void, &mut ret as *mut u64 as *mut c_void, 8, &mut g) == 0 {
                    break;
                }
                *(c.0.as_mut_ptr().add(0xF8) as *mut u64) = ret;
                *(c.0.as_mut_ptr().add(0x98) as *mut u64) = rsp + 8;
            } else {
                let mut hd: *mut c_void = ptr::null_mut();
                let mut est: u64 = 0;
                RtlVirtualUnwind(0, image, rip, entry, c.0.as_mut_ptr() as *mut c_void, &mut hd, &mut est, ptr::null_mut());
            }
            if depth == 23 {
                break;
            }
        }
        rlog!("  walk:{}", line.as_str());
    }
}

fn dump_thread(tid: u32) {
    unsafe {
        let h = OpenThread(0x1F_FFFF, 0, tid);
        if h.is_null() {
            rlog!("watch: cannot open title thread");
            return;
        }
        rlog!("watch: opened thread");
        SuspendThread(h);
        rlog!("watch: suspended");
        let mut c = Context([0; 1232]);
        *(c.0.as_mut_ptr().add(0x30) as *mut u32) = 0x10_000B; // CONTEXT_FULL
        let ok = GetThreadContext(h, c.0.as_mut_ptr() as *mut c_void);
        let rip = *(c.0.as_ptr().add(0xF8) as *const usize);
        let rsp = *(c.0.as_ptr().add(0x98) as *const usize);
        let mut stack = [0usize; 1024];
        let mut want = 8192;
        let mut got = 0usize;
        while ok != 0 && want >= 64 {
            if ReadProcessMemory(GetCurrentProcess(), rsp as *const c_void, stack.as_mut_ptr() as *mut c_void, want, &mut got) != 0 {
                break;
            }
            want /= 2;
            got = 0;
        }
        ResumeThread(h);
        if ok == 0 {
            rlog!("watch: GetThreadContext failed");
            return;
        }
        let mut loc = RawBuf::new();
        describe(rip, &mut loc);
        rlog!("watch: rip = {}  rsp = {rsp:#x}", loc.as_str());
        let mut shown = 0;
        for (i, &v) in stack[..got / 8].iter().enumerate() {
            let (base, size) = (IMG_BASE.load(Ordering::Relaxed), IMG_SIZE.load(Ordering::Relaxed));
            if v >= base && v < base + size {
                let mut l = RawBuf::new();
                describe(v, &mut l);
                rlog!("  stack[{i}] -> {}", l.as_str());
                shown += 1;
                if shown == 40 {
                    break;
                }
            }
        }
    }
}

/// Logs fatal-looking exceptions as `title+RVA` so crashes can be matched to the binary.
/// Always continues the search so the title's own SEH handlers still run. Allocation-free.
extern "system" fn crash_logger(info: *const *const u8) -> i32 {
    unsafe {
        let rec = *info;
        let code = *(rec as *const u32);
        if code == 0x8000_0004 {
            // A hardware write breakpoint (XB1_HWBP) reports through DR6 bit 0.
            let ctx0 = *info.add(1);
            let dr6 = *(ctx0.add(0x68) as *const u64);
            if dr6 & 1 != 0 {
                *(ctx0.add(0x68) as *mut u64) = 0;
                let (base, size) = (IMG_BASE.load(Ordering::Relaxed), IMG_SIZE.load(Ordering::Relaxed));
                let rip = *(ctx0.add(0xF8) as *const usize);
                let new = *(*(ctx0.add(0x48) as *const usize) as *const u64);
                let mut at = RawBuf::new();
                describe(rip, &mut at);
                rlog!("[hwbp] write to the watched field, now {new:#x}, next instruction at {}", at.as_str());
                let mut frames = [ptr::null_mut::<c_void>(); 24];
                let n = RtlCaptureStackBackTrace(0, 24, frames.as_mut_ptr(), ptr::null_mut()) as usize;
                for f in frames[..n].iter().map(|f| *f as usize).filter(|f| *f >= base && *f < base + size).take(8) {
                    rlog!("  caller title+{:#x}", f - base);
                }
                // XB1_HWBP_FOLLOW=1: after the first report the watch moves to the object the written pointer points at
                // (its first eight bytes), to see who fills and overwrites it afterwards.
                static FOLLOWED: AtomicBool = AtomicBool::new(false);
                if std::env::var_os("XB1_HWBP_FOLLOW").is_some() && new > 0x10000 && new % 8 == 0 && !FOLLOWED.swap(true, Ordering::Relaxed) {
                    *(ctx0.add(0x48) as *mut u64) = new;
                    HWBP_ADDR.store(new as usize, Ordering::Relaxed);
                    arm_hwbp_all_threads(new as usize);
                    rlog!("[hwbp] now watching the object at {new:#x}");
                }
                // The unwinder stops early in leaf thunks: also list the title addresses found on the raw stack.
                let rsp = *(ctx0.add(0x98) as *const usize);
                let mut words = [0usize; 96];
                let mut got = 0usize;
                ReadProcessMemory(GetCurrentProcess(), rsp as *const c_void, words.as_mut_ptr() as *mut c_void, 96 * 8, &mut got);
                for (i, w) in words.iter().enumerate().take(got / 8) {
                    if *w >= base && *w < base + size {
                        rlog!("  stack+{:#x}: title+{:#x}", i * 8, *w - base);
                    }
                }
                return -1;
            }
            // Single-step after a repeating watchpoint: put the breakpoints back.
            for (i, pending) in WATCH_PENDING.iter().enumerate() {
                if pending.swap(false, Ordering::Relaxed) {
                    let a = WATCH_ADDR[i].load(Ordering::Relaxed);
                    if a != 0 {
                        *(a as *mut u8) = 0xCC;
                    }
                }
            }
            return -1;
        }
        if code == 0x8000_0003 {
            let addr = *(rec.add(16) as *const usize);
            // A one-shot watchpoint: report, restore the instruction byte and re-execute it.
            for (i, w) in WATCH_ADDR.iter().enumerate() {
                if w.load(Ordering::Relaxed) == addr && addr != 0 {
                    let ctx = *info.add(1);
                    let reg = |off: usize| *(ctx.add(off) as *const u64);
                    let (base, size) = (IMG_BASE.load(Ordering::Relaxed), IMG_SIZE.load(Ordering::Relaxed));
                    if WATCH_REPEAT.load(Ordering::Relaxed) {
                        // Compact line per hit: the logged file, line, category and message.
                        let hit = WATCH_HITS.fetch_add(1, Ordering::Relaxed);
                        static MAXHITS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
                        if hit < *MAXHITS.get_or_init(|| std::env::var("XB1_WATCH_MAX").ok().and_then(|v| v.parse().ok()).unwrap_or(400)) || std::env::var_os("XB1_WATCH_FILTER").is_some() {
                            let rsp = reg(0x98) as usize;
                            let mut out = RawBuf::new();
                            let put = |out: &mut RawBuf, label: &str, p: u64, max: usize| {
                                use std::fmt::Write;
                                let mut buf = [0u8; 200];
                                let mut got = 0usize;
                                if p >= 0x10000 && (ReadProcessMemory(GetCurrentProcess(), p as *const c_void, buf.as_mut_ptr() as *mut c_void, max, &mut got) != 0 || got > 0) {
                                    let n = buf[..got].iter().position(|&b| b == 0).unwrap_or(got);
                                    if n >= 2 && buf[..n].iter().all(|&b| b == b'\t' || b == b'\n' || (0x20..0x7f).contains(&b)) {
                                        let _ = write!(out, " {label}={:?}", std::str::from_utf8(&buf[..n]).unwrap_or("?"));
                                    }
                                }
                            };
                            if let Some(spec) = std::env::var("XB1_WATCH_Q").ok() {
                                // XB1_WATCH_Q="rbx+0x28:4,rbx+0x1e8:24": hex bytes at register+offset for every hit.
                                use std::fmt::Write;
                                for item in spec.split(',') {
                                    if !item.contains(':') {
                                        // A bare register name: its value.
                                        let slot = match item.trim() {
                                            "rax" => 0x78, "rcx" => 0x80, "rdx" => 0x88, "rbx" => 0x90, "rsi" => 0xA8, "rdi" => 0xB0,
                                            "r8" => 0xB8, "r9" => 0xC0, "r10" => 0xC8, "r11" => 0xD0, "r12" => 0xD8, "r13" => 0xE0,
                                            "r14" => 0xE8, "r15" => 0xF0, "rsp" => 0x98, "rbp" => 0xA0, _ => continue,
                                        };
                                        let _ = write!(out, " {}={:#x}", item.trim(), reg(slot));
                                        continue;
                                    }
                                    let Some((target, len)) = item.split_once(':') else { continue };
                                    let Some((rn, off)) = target.split_once('+') else { continue };
                                    let slot = match rn.trim() {
                                        "rax" => 0x78, "rcx" => 0x80, "rdx" => 0x88, "rbx" => 0x90, "rsi" => 0xA8, "rdi" => 0xB0,
                                        "r8" => 0xB8, "r9" => 0xC0, "r10" => 0xC8, "r11" => 0xD0, "r12" => 0xD8, "r13" => 0xE0,
                                        "r14" => 0xE8, "r15" => 0xF0, "rsp" => 0x98, "rbp" => 0xA0, _ => continue,
                                    };
                                    let (off, off2) = match off.split_once('>') { Some((a, b)) => (a, Some(b)), None => (off, None) };
                                    let off = usize::from_str_radix(off.trim().trim_start_matches("0x"), 16).unwrap_or(0);
                                    let len = len.trim().parse::<usize>().unwrap_or(8).min(64);
                                    let mut buf = [0u8; 64];
                                    let mut got = 0usize;
                                    let mut a = (reg(slot) as usize).wrapping_add(off);
                                    if let Some(o2) = off2 {
                                        let mut q = 0usize;
                                        let mut g2 = 0usize;
                                        ReadProcessMemory(GetCurrentProcess(), a as *const c_void, &mut q as *mut usize as *mut c_void, 8, &mut g2);
                                        a = q.wrapping_add(usize::from_str_radix(o2.trim().trim_start_matches("0x"), 16).unwrap_or(0));
                                    }
                                    if ReadProcessMemory(GetCurrentProcess(), a as *const c_void, buf.as_mut_ptr() as *mut c_void, len, &mut got) != 0 {
                                        let _ = write!(out, " [{item}]=");
                                        for b in &buf[..got] { let _ = write!(out, "{b:02x}"); }
                                    } else {
                                        let _ = write!(out, " [{item}]=?");
                                    }
                                }
                            }
                            let have_q = std::env::var_os("XB1_WATCH_Q").is_some();
                            if let Some(spec) = std::env::var("XB1_WATCH_STR").ok() {
                                // XB1_WATCH_STR="rdx,r8": for each register print the text found at it, or
                                // through the pointer stored there (a std::string with a heap buffer).
                                for rn in spec.split(',') {
                                    let slot = match rn.trim() {
                                        "rax" => 0x78, "rcx" => 0x80, "rdx" => 0x88, "rbx" => 0x90, "rsi" => 0xA8, "rdi" => 0xB0,
                                        "r8" => 0xB8, "r9" => 0xC0, "r10" => 0xC8, "r11" => 0xD0, "r12" => 0xD8, "r13" => 0xE0,
                                        "r14" => 0xE8, "r15" => 0xF0, "rsp" => 0x98, "rbp" => 0xA0, _ => continue,
                                    };
                                    let v = reg(slot);
                                    put(&mut out, rn.trim(), v, 120);
                                    let mut q = 0u64;
                                    let mut got = 0usize;
                                    if v >= 0x10000 && ReadProcessMemory(GetCurrentProcess(), v as *const c_void, &mut q as *mut u64 as *mut c_void, 8, &mut got) != 0 {
                                        put(&mut out, "*", q, 120);
                                        {
                                            use std::fmt::Write;
                                            let _ = write!(out, " {}->{:#x}", rn.trim(), q.wrapping_sub(IMG_BASE.load(Ordering::Relaxed) as u64));
                                        }
                                    }
                                }
                            } else if !have_q {
                                put(&mut out, "file", reg(0x88), 150);
                                {
                                    use std::fmt::Write;
                                    let _ = write!(out, " line={} cat={}", reg(0xB8) as u32, reg(0xC0) as u32);
                                }
                                put(&mut out, "msg", *((rsp + 0x28) as *const u64), 190);
                            }
                            if std::env::var_os("XB1_WATCH_STACK").is_some() {
                                // Title return addresses found in the first 96 stack words.
                                use std::fmt::Write;
                                let _ = write!(out, " stack:");
                                let (b, sz) = (IMG_BASE.load(Ordering::Relaxed), IMG_SIZE.load(Ordering::Relaxed));
                                let mut shown = 0;
                                for i in 0..96usize {
                                    let mut q = 0u64;
                                    let mut got = 0usize;
                                    if ReadProcessMemory(GetCurrentProcess(), (rsp + i * 8) as *const c_void, &mut q as *mut u64 as *mut c_void, 8, &mut got) == 0 { break; }
                                    let q = q as usize;
                                    if q >= b && q < b + sz && shown < 8 {
                                        let _ = write!(out, " {:#x}", q - b);
                                        shown += 1;
                                    }
                                }
                            }
                            LAST_HIT_RCX.store(reg(0x80) as usize, Ordering::Relaxed);
                            let keep = match std::env::var("XB1_WATCH_FILTER") { Ok(f) => out.as_str().to_ascii_lowercase().contains(&f.to_ascii_lowercase()), Err(_) => true };
                            if keep {
                                rlog!("[log #{hit}] @{:#x}{}", addr.wrapping_sub(IMG_BASE.load(Ordering::Relaxed)), out.as_str());
                            }
                        }
                        *(addr as *mut u8) = WATCH_ORIG[i].load(Ordering::Relaxed) as u8;
                        WATCH_PENDING[i].store(true, Ordering::Relaxed);
                        *(ctx.add(0x44) as *mut u32) |= 0x100;
                        *(ctx.add(0xF8) as *mut u64) = addr as u64;
                        return -1;
                    }
                    rlog!("[watch] hit title+{:#x} on thread {}", addr - base, GetCurrentThreadId());
                    rlog!(
                        "  rax={:#x} rcx={:#x} rdx={:#x} rbx={:#x} rsi={:#x} rdi={:#x} r8={:#x} r9={:#x}",
                        reg(0x78), reg(0x80), reg(0x88), reg(0x90), reg(0xA8), reg(0xB0), reg(0xB8), reg(0xC0)
                    );
                    // XB1_STALL_MS=n: hold the thread at a watch site (experiment: is a failure a race with
                    // another thread's work?).
                    if let Some(ms) = std::env::var("XB1_STALL_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
                        std::thread::sleep(std::time::Duration::from_millis(ms));
                    }
                    // XB1_PEEK="r13+0x24de:8;r15+0x230:2": memory at register+offset (hex bytes), for
                    // reading object fields at a watch site.
                    if let Some(spec) = PEEK_SPEC.get() {
                        for item in spec.split(';') {
                            let Some((target, len)) = item.split_once(':') else { continue };
                            let Some((rname, off)) = target.split_once('+') else { continue };
                            let slot = match rname {
                                "rax" => 0x78, "rcx" => 0x80, "rdx" => 0x88, "rbx" => 0x90, "rsi" => 0xA8, "rdi" => 0xB0,
                                "r8" => 0xB8, "r9" => 0xC0, "r10" => 0xC8, "r11" => 0xD0, "r12" => 0xD8, "r13" => 0xE0,
                                "r14" => 0xE8, "r15" => 0xF0, "title" => 0x00, _ => continue,
                            };
                            // "reg+off>off2": read the qword at reg+off first, then use it + off2.
                            let (off, off2) = match off.split_once('>') {
                                Some((a, b)) => (a, Some(b)),
                                None => (off, None),
                            };
                            let off = usize::from_str_radix(off.trim_start_matches("0x"), 16).unwrap_or(0);
                            let len = len.parse::<usize>().unwrap_or(8).min(32);
                            // "title+0xRVA:len": a fixed address inside the title image.
                            let regval = if rname == "title" { IMG_BASE.load(Ordering::Relaxed) } else { reg(slot) as usize };
                            let mut p = regval.wrapping_add(off);
                            if let Some(off2) = off2 {
                                let mut q = 0usize;
                                let mut got0 = 0usize;
                                if ReadProcessMemory(GetCurrentProcess(), p as *const c_void, &mut q as *mut usize as *mut c_void, 8, &mut got0) == 0 {
                                    rlog!("  peek {item}: first pointer unreadable at {p:#x}");
                                    continue;
                                }
                                p = q.wrapping_add(usize::from_str_radix(off2.trim_start_matches("0x"), 16).unwrap_or(0));
                            }
                            let mut buf = [0u8; 32];
                            let mut got = 0usize;
                            if ReadProcessMemory(GetCurrentProcess(), p as *const c_void, buf.as_mut_ptr() as *mut c_void, len, &mut got) != 0 {
                                let mut line = RawBuf::new();
                                for b in &buf[..got] {
                                    use std::fmt::Write;
                                    let _ = write!(line, "{b:02x} ");
                                }
                                rlog!("  peek {item} = {}", line.as_str());
                            } else {
                                rlog!("  peek {item} unreadable at {p:#x}");
                            }
                        }
                    }
                    // XB1_HWBP="rbx+0x88": arm hardware write breakpoint 0 on that field of this thread.
                    if let Some(spec) = HWBP_SPEC.get() {
                        if let Some((rname, off)) = spec.split_once('+') {
                            let slot = match rname {
                                "rax" => 0x78, "rcx" => 0x80, "rdx" => 0x88, "rbx" => 0x90, "rsi" => 0xA8, "rdi" => 0xB0,
                                "r8" => 0xB8, "r9" => 0xC0, "r12" => 0xD8, "r13" => 0xE0, "r14" => 0xE8, "r15" => 0xF0, _ => 0x80,
                            };
                            // "reg+off>off2": the pointer stored at reg+off, plus off2.
                            let (off, off2) = match off.split_once('>') { Some((a, b)) => (a, Some(b)), None => (off, None) };
                            let off = usize::from_str_radix(off.trim_start_matches("0x"), 16).unwrap_or(0);
                            let mut target = (reg(slot) as usize).wrapping_add(off);
                            if let Some(o2) = off2 {
                                target = *(target as *const usize) + usize::from_str_radix(o2.trim_start_matches("0x"), 16).unwrap_or(0);
                            }
                            *(ctx.add(0x48) as *mut u64) = target as u64; // Dr0
                            *(ctx.add(0x70) as *mut u64) = 1 | (1 << 16) | (3 << 18); // L0, write, 8 bytes
                            arm_hwbp_all_threads(target);
                            rlog!("  hardware write breakpoint armed on {target:#x} (all threads)");
                        }
                    }
                    // XB1_POKE="r13+0x3224=1": experiment only, writes one byte at register+offset.
                    if POKE_SPEC.get().map_or(false, |p| p.contains("r15+0x4cc")) && std::env::var_os("XB1_BOOT_WAIT").is_some() {
                        // Opt-in experiment (XB1_BOOT_WAIT=1; it did not fix the crash): the render request set only once the graphics start-up has finished (the
                        // swap chain exists and its views are created); set too early, the main tick draws while another thread is
                        // still creating them (NULL render target view, crash at title+0x1cd6a0b).
                        let mut waited = 0u32;
                        while crate::display::SWAP_CHAIN.load(Ordering::Acquire) == 0 && waited < 20_000 {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                            waited += 5;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(1500));
                        rlog!("  boot flag: waited {waited} ms for the swap chain (+1.5 s)");
                    }
                    if let Some(spec) = POKE_SPEC.get() {
                        for item in spec.split(';') {
                            let Some((target, val)) = item.split_once('=') else { continue };
                            let Some((rname, off)) = target.split_once('+') else { continue };
                            let slot = match rname {
                                "rax" => 0x78, "rcx" => 0x80, "rdx" => 0x88, "rbx" => 0x90, "rsi" => 0xA8, "rdi" => 0xB0,
                                "r8" => 0xB8, "r9" => 0xC0, "r10" => 0xC8, "r11" => 0xD0, "r12" => 0xD8, "r13" => 0xE0,
                                "r14" => 0xE8, "r15" => 0xF0, "rsp" => 0x98, "rbp" => 0xA0, _ => continue,
                            };
                            // "reg+off>off2=v": read the qword at reg+off first, then write at that value + off2.
                            let (off, off2) = match off.split_once('>') {
                                Some((a, b)) => (a, Some(b)),
                                None => (off, None),
                            };
                            let off = usize::from_str_radix(off.trim_start_matches("0x"), 16).unwrap_or(0);
                            let v = u8::from_str_radix(val.trim_start_matches("0x"), 16).unwrap_or(0);
                            let mut p = (reg(slot) as usize).wrapping_add(off);
                            if let Some(o2) = off2 {
                                p = *(p as *const usize) + usize::from_str_radix(o2.trim_start_matches("0x"), 16).unwrap_or(0);
                            }
                            *(p as *mut u8) = v;
                            rlog!("  poked {item}");
                        }
                    }
                    let rsp = reg(0x98) as usize;
                    // Any readable text the argument registers and the fifth stack argument point at.
                    let text_at = |p: u64| -> Option<([u8; 160], usize)> {
                        let mut buf = [0u8; 160];
                        let mut got = 0usize;
                        if p < 0x10000 || ReadProcessMemory(GetCurrentProcess(), p as *const c_void, buf.as_mut_ptr() as *mut c_void, 159, &mut got) == 0 && got == 0 {
                            return None;
                        }
                        let n = buf[..got].iter().position(|&b| b == 0).unwrap_or(got);
                        if n >= 3 && buf[..n].iter().all(|&b| b == b'\t' || b == b'\n' || (0x20..0x7f).contains(&b)) { Some((buf, n)) } else { None }
                    };
                    // At a function entry the fifth argument is at rsp+0x28; at a call site, at rsp+0x20.
                    let fifth = *((rsp + 0x28) as *const u64);
                    let fifth_at_call = *((rsp + 0x20) as *const u64);
                    for (name, v) in [("rcx", reg(0x80)), ("rdx", reg(0x88)), ("r8", reg(0xB8)), ("r9", reg(0xC0)), ("arg5 (entry)", fifth), ("arg5 (call site)", fifth_at_call)] {
                        if let Some((buf, n)) = text_at(v) {
                            rlog!("  {name} text: {}", std::str::from_utf8(&buf[..n]).unwrap_or("?"));
                        }
                    }
                    let stack_base: usize;
                    std::arch::asm!("mov {}, gs:[8]", out(reg) stack_base, options(nostack, readonly, preserves_flags));
                    let mut shown = 0;
                    for k in 0..768 {
                        if rsp + k * 8 + 8 > stack_base {
                            break;
                        }
                        let v = *((rsp + k * 8) as *const usize);
                        if v >= base + 8 && v < base + size && (*((v - 5) as *const u8) == 0xE8 || (*((v - 6) as *const u8) == 0xFF && *((v - 5) as *const u8) == 0x15)) {
                            rlog!("  stack+{:#x}: return address title+{:#x}", k * 8, v - base);
                            shown += 1;
                            if shown == 14 {
                                break;
                            }
                        }
                    }
                    *(addr as *mut u8) = WATCH_ORIG[i].load(Ordering::Relaxed) as u8;
                    if WATCH_REPEAT.load(Ordering::Relaxed) {
                        // Stay armed: single-step the restored instruction, then re-plant.
                        WATCH_PENDING[i].store(true, Ordering::Relaxed);
                        *(ctx.add(0x44) as *mut u32) |= 0x100; // EFlags.TF
                    } else {
                        w.store(0, Ordering::Relaxed);
                    }
                    *(ctx.add(0xF8) as *mut u64) = addr as u64; // RIP: re-execute the restored instruction
                    return -1; // EXCEPTION_CONTINUE_EXECUTION
                }
            }
            // A patched fast-fail site: report it, then end the process the way the original would.
            if FASTFAIL_SITES.iter().any(|s| s.load(Ordering::Relaxed) == addr) {
                let ctx = *info.add(1);
                let ecx = *(ctx.add(0x80) as *const u64);
                let (base, _) = (IMG_BASE.load(Ordering::Relaxed), 0);
                rlog!("[fast-fail] at title+{:#x}, fast-fail code (ecx) = {ecx:#x}", addr - base);
                let mut frames = [ptr::null_mut::<c_void>(); 32];
                let n = RtlCaptureStackBackTrace(0, 32, frames.as_mut_ptr(), ptr::null_mut()) as usize;
                for f in frames[..n].iter().map(|f| *f as usize).filter(|f| *f >= base && *f < base + IMG_SIZE.load(Ordering::Relaxed)).take(8) {
                    rlog!("  caller title+{:#x}", f - base);
                }
                let rsp = *(ctx.add(0x98) as *const usize);
                let stack_base: usize;
                std::arch::asm!("mov {}, gs:[8]", out(reg) stack_base, options(nostack, readonly, preserves_flags));
                let (base, size) = (IMG_BASE.load(Ordering::Relaxed), IMG_SIZE.load(Ordering::Relaxed));
                let mut shown = 0;
                for i in 0..512 {
                    if rsp + i * 8 + 8 > stack_base {
                        break;
                    }
                    let v = *((rsp + i * 8) as *const usize);
                    if v >= base + 8 && v < base + size && (*((v - 5) as *const u8) == 0xE8 || (*((v - 6) as *const u8) == 0xFF && *((v - 5) as *const u8) == 0x15)) {
                        rlog!("  stack+{:#x}: return address title+{:#x}", i * 8, v - base);
                        shown += 1;
                        if shown == 12 {
                            break;
                        }
                    }
                }
                TerminateProcess(GetCurrentProcess(), 0xC000_0409);
            }
            return 0;
        }
        // Fatal-looking codes, plus MSVC C++ throws (0xE06D7363), which are routine but
        // worth seeing while a title is failing: they show where it throws.
        static CPP_LOGGED: AtomicUsize = AtomicUsize::new(0);
        let is_cpp = code == 0xE06D_7363 && CPP_LOGGED.fetch_add(1, Ordering::Relaxed) < 30;
        if code & 0xF000_0000 != 0xC000_0000 && !is_cpp {
            return 0;
        }
        let addr = *(rec.add(16) as *const usize);
        let seen = SEEN_COUNT.load(Ordering::Relaxed).min(SEEN_FAULTS.len());
        if !is_cpp && SEEN_FAULTS[..seen].iter().any(|s| s.load(Ordering::Relaxed) == addr) {
            return 0;
        }
        if !is_cpp && seen < SEEN_FAULTS.len() {
            SEEN_FAULTS[SEEN_COUNT.fetch_add(1, Ordering::Relaxed).min(SEEN_FAULTS.len() - 1)].store(addr, Ordering::Relaxed);
        }
        let mut loc = RawBuf::new();
        describe(addr, &mut loc);
        rlog!("[exception] {code:#x} at {}", loc.as_str());
        if BACKTRACE_ON_CRASH.load(Ordering::Relaxed) && code == 0xC000_0005 {
            // Allocates, so only on request (XB1_BT=1): the heap is usually fine for faults in the layer.
            rlog!("{}", std::backtrace::Backtrace::force_capture());
        }
        rlog!("  committed by the title so far: {} MiB", COMMITTED_BYTES.load(Ordering::Relaxed) >> 20);
        gpu::report_context_changes();
        {
            let teb: usize;
            std::arch::asm!("mov {}, gs:[0x30]", out(reg) teb, options(nostack, readonly, preserves_flags));
            let arr = *((teb + 0x58) as *const usize);
            let idx = match NTDLL_TLS_INDEX.load(Ordering::Relaxed) {
                usize::MAX => TLS_TITLE_INDEX,
                i => i,
            };
            let entry = if arr > 0x10000 { *((arr + idx * 8) as *const usize) } else { 0 };
            rlog!("  thread {}: TLS array {arr:#x}, entry[{idx}] = {entry:#x}", GetCurrentThreadId());
            // ThreadQuerySetWin32StartAddress = 9
            let mut start = 0usize;
            if NtQueryInformationThread(GetCurrentThread(), 9, &mut start as *mut usize as *mut c_void, 8, ptr::null_mut()) == 0 {
                let mut s = RawBuf::new();
                describe(start, &mut s);
                rlog!("  thread start address: {}", s.as_str());
            }
            if arr > 0x10000 {
                let mut q = [0u64; 8];
                if VirtualQuery(arr as *const c_void, q.as_mut_ptr() as *mut c_void, 48) != 0 {
                    rlog!("  array region base={:#x} size={:#x} alloc_base={:#x}", q[0], q[3], q[1]);
                }
                let mut shown = 0;
                // Only entries inside the array's own memory region (it can end before 160 slots).
                let region_end = if q[3] != 0 { q[0] as usize + q[3] as usize } else { arr + 8 };
                for i in 0..160 {
                    if arr + i * 8 + 8 > region_end {
                        break;
                    }
                    let v = *((arr + i * 8) as *const usize);
                    if v != 0 && shown < 14 {
                        rlog!("    TLS[{i}] = {v:#x}");
                        shown += 1;
                    }
                }
            }
        }
        let ctx = *info.add(1);
        let reg = |off: usize| *(ctx.add(off) as *const u64);
        rlog!(
            "  rax={:#x} rcx={:#x} rdx={:#x} rbx={:#x} rsi={:#x} rdi={:#x} r8={:#x} r9={:#x} rsp={:#x}",
            reg(0x78), reg(0x80), reg(0x88), reg(0x90), reg(0xA8), reg(0xB0), reg(0xB8), reg(0xC0), reg(0x98)
        );
        // For each register that holds a readable pointer, show who owns the first qword it
        // points at. For a COM object that is its vtable, which identifies the implementation.
        for (name, off) in [("rcx", 0x80), ("rdx", 0x88), ("rbx", 0x90), ("rsi", 0xA8), ("rdi", 0xB0), ("r8", 0xB8), ("r14", 0xF0), ("r15", 0xF8)] {
            let p = reg(off) as usize;
            let mut first = 0usize;
            let mut got = 0usize;
            if p > 0x10000
                && ReadProcessMemory(GetCurrentProcess(), p as *const c_void, &mut first as *mut usize as *mut c_void, 8, &mut got) != 0
                && got == 8
                && first > 0x10000
            {
                let mut owner = RawBuf::new();
                describe(first, &mut owner);
                let shell_name = winrt::identify_vtable(first).unwrap_or("");
                rlog!("  {name}={p:#x} -> [{first:#x}] {} {shell_name} {}", owner.as_str(), gpu::d3d_object_name(p as u64));
            }
        }
        if code == 0xC000_0005 {
            let kind = *(rec.add(32) as *const usize);
            let target = *(rec.add(40) as *const usize);
            let kind = match kind { 0 => "read", 1 => "write", _ => "execute" };
            rlog!("  access violation: {kind} of {target:#x}");
            {
                // Unwind the faulting thread from the exception context (a copy; the original is left alone).
                let mut copy = Context([0; 1232]);
                ptr::copy_nonoverlapping(ctx as *const u8, copy.0.as_mut_ptr(), 1232);
                rlog!("  fault rip {:#x}", reg(0xF8));
                walk_context(&mut copy);
            }
            if kind == "execute" || std::env::var_os("XB1_DUMP_STACK").is_some() {
                let rsp = reg(0x98) as usize;
                let mut words = [0usize; 24];
                let mut got = 0usize;
                ReadProcessMemory(GetCurrentProcess(), rsp as *const c_void, words.as_mut_ptr() as *mut c_void, 24 * 8, &mut got);
                let base = IMG_BASE.load(Ordering::Relaxed);
                let me = GetModuleHandleA(std::ptr::null()) as usize;
                for (i, w) in words.iter().enumerate().take(got / 8) {
                    rlog!("  rsp+{:#x} = {:#x} (title+{:#x}, layer+{:#x})", i * 8, w, w.wrapping_sub(base), w.wrapping_sub(me));
                }
            }
            // MEMORY_BASIC_INFORMATION: BaseAddress, AllocationBase, AllocationProtect(+pad), RegionSize, State, Protect, Type
            let mut mbi = [0u64; 8];
            if VirtualQuery(target as *const c_void, mbi.as_mut_ptr() as *mut c_void, 48) != 0 {
                rlog!(
                    "  region base={:#x} alloc_base={:#x} size={:#x} state={:#x} protect={:#x} type={:#x}",
                    mbi[0], mbi[1], mbi[3], mbi[4] & 0xFFFF_FFFF, mbi[4] >> 32, mbi[5] & 0xFFFF_FFFF
                );
            }
            let mut mbi2 = [0u64; 8];
            let buf = reg(0x78) as usize;
            if VirtualQuery(buf as *const c_void, mbi2.as_mut_ptr() as *mut c_void, 48) != 0 {
                rlog!(
                    "  rax region base={:#x} alloc_base={:#x} size={:#x} state={:#x} protect={:#x}",
                    mbi2[0], mbi2[1], mbi2[3], mbi2[4] & 0xFFFF_FFFF, mbi2[4] >> 32
                );
            }
        }
        let (base, size) = (IMG_BASE.load(Ordering::Relaxed), IMG_SIZE.load(Ordering::Relaxed));
        let mut frames = [ptr::null_mut::<c_void>(); 32];
        let n = RtlCaptureStackBackTrace(0, 32, frames.as_mut_ptr(), ptr::null_mut()) as usize;
        for f in frames[..n].iter().map(|f| *f as usize).filter(|f| *f >= base && *f < base + size).take(8) {
            rlog!("  caller title+{:#x}", f - base);
        }
        // Return-address-like values pointing into any loaded module's executable code, named
        // by module. Shows system and driver frames the title-only scan below cannot.
        {
            let rsp = reg(0x98) as usize;
            let mut stack_base: usize;
            std::arch::asm!("mov {}, gs:[8]", out(reg) stack_base, options(nostack, readonly, preserves_flags));
            let mut shown = 0;
            for i in 0..384 {
                if rsp + i * 8 + 8 > stack_base {
                    break;
                }
                let v = *((rsp + i * 8) as *const usize);
                if v < 0x10000 {
                    continue;
                }
                let mut mbi = [0u64; 8];
                if VirtualQuery(v as *const c_void, mbi.as_mut_ptr() as *mut c_void, 48) == 0 {
                    continue;
                }
                // MEM_IMAGE with an executable protection (PAGE_EXECUTE_*)
                let (ty, protect) = (mbi[5] & 0xFFFF_FFFF, mbi[4] >> 32);
                if ty == 0x0100_0000 && protect & 0xF0 != 0 {
                    let mut l = RawBuf::new();
                    describe(v, &mut l);
                    rlog!("  frame+{:#x}: {}", i * 8, l.as_str());
                    shown += 1;
                    if shown == 24 {
                        break;
                    }
                }
            }
            stack_base = 0;
            let _ = stack_base;
        }
        // The unwinder often stops early, so also scan the faulting stack for return-address-like values.
        let rsp = reg(0x98) as usize;
        let stack_base: usize;
        std::arch::asm!("mov {}, gs:[8]", out(reg) stack_base, options(nostack, readonly, preserves_flags));
        let mut shown = 0;
        for i in 0..512 {
            if rsp + i * 8 + 8 > stack_base {
                break;
            }
            let v = *((rsp + i * 8) as *const usize);
            if v >= base + 8 && v < base + size && {
                let after_call = |n: usize| *((v - n) as *const u8);
                            // call rel32, call [rip+disp], call [reg+disp8], call [reg+disp32], call reg
                            let is_call = after_call(5) == 0xE8 || (after_call(6) == 0xFF && after_call(5) == 0x15) || (after_call(3) == 0xFF && (after_call(2) & 0xF8) == 0x50) || (after_call(6) == 0xFF && (after_call(5) & 0xF8) == 0x90) || (after_call(2) == 0xFF && (after_call(1) & 0xF8) == 0xD0);
                is_call
            } {
                rlog!("  stack+{:#x}: return address title+{:#x}", i * 8, v - base);
                shown += 1;
                if shown == 12 {
                    break;
                }
            }
        }
    }
    0
}

/// Index of the title's static TLS in `TEB.ThreadLocalStoragePointer`. When a DLL with static
/// TLS loads, ntdll grows every thread's vector and copies only the entries below its own
/// notion of the current size, so a high index (100 was tried) is lost; this one is kept low
/// enough to stay inside that range while being above any index real modules receive.
const TLS_TITLE_INDEX: usize = 60;

/// TLS index assigned by the loader to the helper DLL, or `usize::MAX` when the hand-built
/// vector is used instead.
static NTDLL_TLS_INDEX: AtomicUsize = AtomicUsize::new(usize::MAX);
static PATCH_FASTFAIL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static LEGACY_TLS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Loads the helper DLL that makes `ntdll` own the title's static TLS. Must run before the
/// thread that executes title code is created.
unsafe fn prepare_ntdll_tls(base: *mut u8, tls: (u32, u32)) -> Result<u32, String> {
    let d = unsafe { base.add(tls.0 as usize) };
    let (start, end, zero_fill) = unsafe { (*(d as *const u64) as usize, *(d.add(8) as *const u64) as usize, *(d.add(32) as *const u32) as usize) };
    let template = unsafe { std::slice::from_raw_parts(start as *const u8, end - start) };
    // Snapshot this thread's TLS vector so the loader's effect is visible.
    let snapshot = || -> Vec<usize> {
        let teb: usize;
        unsafe { std::arch::asm!("mov {}, gs:[0x30]", out(reg) teb, options(nostack, readonly, preserves_flags)) };
        let vector = unsafe { *((teb + 0x58) as *const *const usize) };
        (0..24).map(|i| unsafe { *vector.add(i) }).collect()
    };
    let before = snapshot();
    println!("host executable's own TLS index: {:?}", unsafe { tlsdll::host_tls_index() });
    let index = unsafe { tlsdll::load_helper(template, zero_fill)? };
    let after = snapshot();
    let changed: Vec<usize> = (0..24).filter(|&i| before[i] != after[i]).collect();
    println!("TLS vector slots changed by loading the helper DLL: {changed:?}");
    NTDLL_TLS_INDEX.store(index as usize, Ordering::Relaxed);
    Ok(index)
}
static TLS_TEMPLATE: AtomicUsize = AtomicUsize::new(0);
static TLS_LEN: AtomicUsize = AtomicUsize::new(0);
static TLS_TOTAL: AtomicUsize = AtomicUsize::new(0);

/// Gives the calling thread its own copy of the title's static TLS.
///
/// The TEB's ThreadLocalStoragePointer array has no recorded length, so this swaps in a
/// larger copy of its first entries and claims a high index for the title. Threads the
/// title creates through `CreateThread` get this via `xb_create_thread`; threads made
/// elsewhere (thread pools, host threads) do not.
unsafe fn install_tls_for_current_thread() {
    const SLOTS: usize = 128;
    let (total, len) = (TLS_TOTAL.load(Ordering::Relaxed), TLS_LEN.load(Ordering::Relaxed));
    let block = VirtualAlloc(ptr::null_mut(), total.max(1), MEM_COMMIT_RESERVE, 0x04) as *mut u8;
    ptr::copy_nonoverlapping(TLS_TEMPLATE.load(Ordering::Relaxed) as *const u8, block, len);
    let teb: usize;
    std::arch::asm!("mov {}, gs:[0x30]", out(reg) teb, options(nostack, readonly, preserves_flags));
    let slot = (teb + 0x58) as *mut *mut *mut c_void;
    // This array must come from the process heap: when a DLL with static TLS is loaded later,
    // ntdll reallocates and frees the thread's TLS pointer array with RtlFreeHeap, and freeing
    // a VirtualAlloc block that way faults (ntdll reads a heap header 16 bytes below it).
    // HEAP_ZERO_MEMORY = 0x8.
    let arr = HeapAlloc(GetProcessHeap(), 0x8, SLOTS * 8) as *mut *mut c_void;
    if !(*slot).is_null() {
        // Copy the entries the loader made, including those of DLLs loaded after the thread started
        // (graphics drivers have implicit TLS too). The array's size is not known, so as many as can
        // be read of the first 64 are taken; unused trailing entries are never looked at.
        let mut got = 0usize;
        let mut want = 64usize;
        while want >= 16 {
            if ReadProcessMemory(GetCurrentProcess(), *slot as *const c_void, arr as *mut c_void, want * 8, &mut got) != 0 {
                break;
            }
            want /= 2;
        }
        if want < 16 {
            ptr::copy_nonoverlapping(*slot, arr, 16);
        }
    }
    *arr.add(TLS_TITLE_INDEX) = block as *mut c_void;
    OLD_TLS.with(|o| o.set(*slot as usize));
    *slot = arr;
    rlog!("[tls] thread {} got the title's static TLS (array {arr:p})", GetCurrentThreadId());
}

thread_local! {
    /// The loader's TLS pointer array that the title's array replaced on this thread (0 if none).
    static OLD_TLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Whether this thread already has the title's static TLS installed.
    static TLS_READY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Installs the title's static TLS on the calling thread once.
fn ensure_tls_on_current_thread() {
    if NTDLL_TLS_INDEX.load(Ordering::Relaxed) != usize::MAX {
        return; // the loader already gave this thread its block
    }
    TLS_READY.with(|ready| {
        if !ready.get() {
            unsafe { install_tls_for_current_thread() };
            ready.set(true);
        }
    });
}

/// Puts the loader's own TLS pointer array back on this thread (undoing `install_tls_for_current_thread`), so that
/// ntdll's thread teardown frees only what it allocated. Must be called before a thread that got the title's TLS ends.
pub fn restore_loader_tls() {
    let old_array = OLD_TLS.with(|o| o.replace(0));
    if old_array != 0 {
        unsafe {
            let teb: usize;
            std::arch::asm!("mov {}, gs:[0x30]", out(reg) teb, options(nostack, readonly, preserves_flags));
            *((teb + 0x58) as *mut usize) = old_array;
        }
    }
}

/// A thread-pool I/O callback registered by the title, wrapped so pool threads get TLS first.
struct PoolIo {
    callback: usize,
    context: usize,
}

type PoolIoCallback = extern "system" fn(*mut c_void, *mut c_void, *mut c_void, u32, usize, *mut c_void);

extern "system" fn pool_io_trampoline(instance: *mut c_void, context: *mut c_void, overlapped: *mut c_void, result: u32, bytes: usize, io: *mut c_void) {
    ensure_tls_on_current_thread();
    let wrapped = unsafe { &*(context as *const PoolIo) };
    let f: PoolIoCallback = unsafe { std::mem::transmute(wrapped.callback) };
    f(instance, wrapped.context as *mut c_void, overlapped, result, bytes, io);
}

/// `CreateThreadpoolIo(file, callback, context, environment)`.
extern "system" fn xb_create_threadpool_io(file: *mut c_void, callback: usize, context: usize, env: *mut c_void) -> *mut c_void {
    // Leaked on purpose: the callback runs for every completion until the object is closed.
    static LOGGED: AtomicUsize = AtomicUsize::new(0);
    if LOGGED.fetch_add(1, Ordering::Relaxed) < 3 {
        rlog!("[io] CreateThreadpoolIo callback title+{:#x}", callback.wrapping_sub(IMG_BASE.load(Ordering::Relaxed)));
    }
    let wrapped = Box::into_raw(Box::new(PoolIo { callback, context }));
    unsafe { CreateThreadpoolIo(file, pool_io_trampoline as usize, wrapped as *mut c_void, env) }
}

/// `XB1_EVENT_TRACE=1`: log every event the title creates and sets (handle and calling title address), to find which
/// code is expected to signal an event a thread waits on.
fn event_trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("XB1_EVENT_TRACE").is_some())
}

/// Title return addresses (RVAs) found by the stack unwinder above the caller (up to 8 frames, the stack walk skips frames
/// without unwind data, so deeper frames can be missing).
pub(crate) fn title_frames() -> [usize; 8] {
    unsafe extern "system" { fn RtlCaptureStackBackTrace(skip: u32, count: u32, frames: *mut *mut c_void, hash: *mut u32) -> u16; }
    let mut f = [ptr::null_mut::<c_void>(); 32];
    let n = unsafe { RtlCaptureStackBackTrace(1, 32, f.as_mut_ptr(), ptr::null_mut()) } as usize;
    let base = IMG_BASE.load(Ordering::Relaxed);
    let size = IMG_SIZE.load(Ordering::Relaxed);
    let mut out = [0usize; 8];
    let mut k = 0;
    for p in f.iter().take(n) {
        let a = *p as usize;
        if a >= base && a < base + size && k < 8 {
            out[k] = a - base;
            k += 1;
        }
    }
    out
}

pub(crate) fn caller_rva() -> usize {
    unsafe extern "system" { fn RtlCaptureStackBackTrace(skip: u32, count: u32, frames: *mut *mut c_void, hash: *mut u32) -> u16; }
    let mut f = [ptr::null_mut::<c_void>(); 8];
    unsafe { RtlCaptureStackBackTrace(1, 8, f.as_mut_ptr(), ptr::null_mut()) };
    let base = IMG_BASE.load(Ordering::Relaxed);
    f.iter().map(|p| *p as usize).find(|a| *a >= base && *a < base + IMG_SIZE.load(Ordering::Relaxed)).map_or(0, |a| a - base)
}

extern "system" fn xb_create_event_w(attrs: *mut c_void, manual: i32, initial: i32, name: *const u16) -> *mut c_void {
    let h = unsafe { CreateEventW(attrs, manual, initial, name) };
    rlog!("[event] create {h:p} manual={manual} initial={initial} from title+{:#x}", caller_rva());
    h
}

extern "system" fn xb_create_event_a(attrs: *mut c_void, manual: i32, initial: i32, name: *const i8) -> *mut c_void {
    let h = unsafe { CreateEventA(attrs, manual, initial, name) };
    rlog!("[event] create {h:p} manual={manual} initial={initial} from title+{:#x}", caller_rva());
    h
}

extern "system" fn xb_set_event(h: *mut c_void) -> i32 {
    static N: AtomicUsize = AtomicUsize::new(0);
    if N.fetch_add(1, Ordering::Relaxed) < 3_000_000 {
        rlog!("[event] set {h:p} from title+{:#x}", caller_rva());
    }
    unsafe { SetEvent(h) }
}

extern "system" fn xb_create_semaphore(attrs: *mut c_void, initial: i32, max: i32, name: *const u16, flags: u32, access: u32) -> *mut c_void {
    let h = unsafe { CreateSemaphoreExW(attrs, initial, max, name, flags, access) };
    rlog!("[event] create semaphore {h:p} initial={initial} max={max} from title+{:#x}", caller_rva());
    h
}

extern "system" fn xb_release_semaphore(h: *mut c_void, count: i32, previous: *mut i32) -> i32 {
    static N: AtomicUsize = AtomicUsize::new(0);
    if N.fetch_add(1, Ordering::Relaxed) < 3_000_000 {
        rlog!("[event] release semaphore {h:p} by {count} from title+{:#x}", caller_rva());
    }
    unsafe { ReleaseSemaphore(h, count, previous) }
}

/// Handles of the last infinite wait-for-all the title made (the watchdog reports their state).
static WAIT_ALL_HANDLES: Mutex<Vec<usize>> = Mutex::new(Vec::new());

extern "system" fn xb_wait_multiple(count: u32, handles: *const *mut c_void, all: i32, ms: u32) -> u32 {
    if all != 0 && ms == u32::MAX && count > 0 && count <= 16 && !handles.is_null() {
        let v: Vec<usize> = (0..count as usize).map(|i| unsafe { *handles.add(i) } as usize).collect();
        rlog!("[event] wait for all of {:#x?} (infinite) from title+{:#x}", v, caller_rva());
        *WAIT_ALL_HANDLES.lock().unwrap() = v;
    }
    unsafe { WaitForMultipleObjects(count, handles, all, ms) }
}

/// Infinite single-object waits in progress: `(thread id, handle, title caller)`.
static PENDING_WAITS: Mutex<Vec<(u32, usize, String)>> = Mutex::new(Vec::new());

fn tracked_wait(h: *mut c_void, ms: u32, f: impl FnOnce() -> u32) -> u32 {
    if ms != u32::MAX {
        return f();
    }
    let tid = unsafe { GetCurrentThreadId() };
    let caller = caller_chain();
    PENDING_WAITS.lock().unwrap().push((tid, h as usize, caller));
    let r = f();
    PENDING_WAITS.lock().unwrap().retain(|w| w.0 != tid);
    r
}

extern "system" fn xb_wait_single(h: *mut c_void, ms: u32) -> u32 {
    tracked_wait(h, ms, || unsafe { WaitForSingleObject(h, ms) })
}

extern "system" fn xb_wait_single_ex(h: *mut c_void, ms: u32, alertable: i32) -> u32 {
    tracked_wait(h, ms, || unsafe { WaitForSingleObjectEx(h, ms, alertable) })
}

/// The first few title return addresses on the stack, as text.
pub(crate) fn caller_chain() -> String {
    // Raw stack scan (the title's tiny wait wrapper has no unwind data, so the unwinder stops at it).
    let marker = 0usize;
    let top = &marker as *const usize as usize;
    let (base, size) = (IMG_BASE.load(Ordering::Relaxed), IMG_SIZE.load(Ordering::Relaxed));
    let mut v: Vec<String> = Vec::new();
    for i in 0..512usize {
        let p = (top + i * 8) as *const usize;
        let mut q = 0usize;
        let mut got = 0usize;
        if unsafe { ReadProcessMemory(GetCurrentProcess(), p as *const c_void, &mut q as *mut usize as *mut c_void, 8, &mut got) } == 0 {
            break;
        }
        if q >= base && q < base + size {
            v.push(format!("{:#x}", q - base));
            if v.len() >= 7 {
                break;
            }
        }
    }
    format!("title+ {}", v.join(" "))
}

struct ThreadStart {
    routine: usize,
    param: usize,
}

extern "system" fn thread_trampoline(p: *mut c_void) -> u32 {
    let ThreadStart { routine, param } = *unsafe { Box::from_raw(p as *mut ThreadStart) };
    if routine < 0x10000 {
        rlog!("[thread] trampoline started with routine {routine:#x}, param {param:#x}, box {p:p}");
    }
    ensure_tls_on_current_thread();
    rlog!("[thread] title thread started: routine title+{:#x} param {param:#x}", routine.wrapping_sub(IMG_BASE.load(Ordering::Relaxed)));
    let hw = HWBP_ADDR.load(Ordering::Relaxed);
    if hw != 0 {
        unsafe { arm_hwbp_on(GetCurrentThread(), hw) };
    }
    let f: extern "system" fn(*mut c_void) -> u32 = unsafe { std::mem::transmute(routine) };
    let code = f(param as *mut c_void);
    // Give the loader its own array back so its thread teardown frees what it allocated.
    restore_loader_tls();
    let base = IMG_BASE.load(Ordering::Relaxed);
    rlog!("[thread] routine title+{:#x} param {param:#x} returned {code:#x}", routine.wrapping_sub(base));
    code
}

/// CreateThread for the title: wraps the start routine so the new thread gets static TLS first.
extern "system" fn xb_create_thread(
    attrs: *mut c_void,
    stack: usize,
    routine: usize,
    param: usize,
    flags: u32,
    tid: *mut u32,
) -> *mut c_void {
    let base = IMG_BASE.load(Ordering::Relaxed);
    if routine >= base && routine < base + IMG_SIZE.load(Ordering::Relaxed) {
        rlog!("[thread] CreateThread routine title+{:#x} param {param:#x} flags {flags:#x}", routine - base);
    } else {
        rlog!("[thread] CreateThread routine OUTSIDE the title: {routine:#x} param {param:#x}");
    }
    let boxed = Box::into_raw(Box::new(ThreadStart { routine, param }));
    // Console threads can ask for small stacks that the desktop D3D11 runtime and drivers overflow.
    let (stack, flags) = if stack != 0 && stack < (1 << 20) { rlog!("[thread] stack {stack:#x} raised to 1 MiB"); (1 << 20, flags | 0x10000) } else { (stack, flags) };
    let h = unsafe { CreateThread(attrs, stack, thread_trampoline as usize, boxed as *mut c_void, flags, tid) };
    if h.is_null() {
        drop(unsafe { Box::from_raw(boxed) });
    }
    h
}

/// Process-level setup a real loader would have done for the title:
/// unwind tables, PEB image base, main-thread static TLS and TLS callbacks.
unsafe fn bootstrap(base: *mut u8, size: usize, tls: (u32, u32), exc: (u32, u32)) -> Result<(), String> {
    IMG_BASE.store(base as usize, Ordering::Relaxed);
    IMG_SIZE.store(size, Ordering::Relaxed);
    AddVectoredExceptionHandler(1, crash_logger);
    // XB1_HWBP_RVA=0xa5ee08: arm the hardware write breakpoint on a fixed title address from the start (threads
    // created later get it in their trampoline), to see who writes a global the title sets early.
    if let Some(rva) = std::env::var("XB1_HWBP_RVA").ok().and_then(|v| usize::from_str_radix(v.trim_start_matches("0x"), 16).ok()) {
        let addr = base as usize + rva;
        HWBP_ADDR.store(addr, Ordering::Relaxed);
        unsafe { arm_hwbp_on(GetCurrentThread(), addr) };
        rlog!("hardware write breakpoint armed from the start on title+{rva:#x}");
    }
    // XB1_HWBP_ADDR=0x60194470960: the same with an absolute address (heap addresses of the title's allocator repeat
    // from run to run, so an object found in one run can be watched from the start of the next).
    if let Some(addr) = std::env::var("XB1_HWBP_ADDR").ok().and_then(|v| usize::from_str_radix(v.trim_start_matches("0x"), 16).ok()) {
        HWBP_ADDR.store(addr, Ordering::Relaxed);
        unsafe { arm_hwbp_on(GetCurrentThread(), addr) };
        rlog!("hardware write breakpoint armed from the start on {addr:#x}");
    }
    if let Ok(spec) = std::env::var("XB1_HWBP") {
        let _ = HWBP_SPEC.set(spec);
    }
    if let Ok(spec) = std::env::var("XB1_POKE") {
        let _ = POKE_SPEC.set(spec);
    }
    if let Ok(spec) = std::env::var("XB1_PEEK") {
        let _ = PEEK_SPEC.set(spec);
    }
    FILES_TO_STDERR.store(std::env::var_os("XB1_FILES").is_some(), Ordering::Relaxed);
    BACKTRACE_ON_CRASH.store(std::env::var_os("XB1_BT").is_some(), Ordering::Relaxed);
    hook_nt_terminate();
    // hook_nt_create_thread() hangs the run (logging while the thread is being created); kept for reference.
    let _ = hook_nt_create_thread as fn();

    if exc.0 != 0 {
        let count = exc.1 / 12;
        if RtlAddFunctionTable(base.add(exc.0 as usize) as *const c_void, count, base as u64) == 0 {
            return Err("RtlAddFunctionTable failed".into());
        }
        println!("registered {count} unwind entries");
    }

    // GetModuleHandle(NULL) reads PEB.ImageBaseAddress.
    let peb: usize;
    std::arch::asm!("mov {}, gs:[0x60]", out(reg) peb, options(nostack, readonly, preserves_flags));
    if NO_PEB_PATCH.load(Ordering::Relaxed) {
        println!("PEB.ImageBaseAddress left pointing at the host (--no-peb-patch)");
    } else {
        *((peb + 0x10) as *mut usize) = base as usize;
    }

    if tls.0 != 0 {
        let d = base.add(tls.0 as usize);
        let start = *(d as *const u64) as usize;
        let end = *(d.add(8) as *const u64) as usize;
        let idx_ptr = *(d.add(16) as *const u64) as *mut u32;
        let callbacks = *(d.add(24) as *const u64) as *const usize;
        let zero_fill = *(d.add(32) as *const u32) as usize;
        let len = end - start;

        let ntdll_index = NTDLL_TLS_INDEX.load(Ordering::Relaxed);
        if ntdll_index != usize::MAX {
            // The loader owns the block (see tlsdll.rs): every thread already has it.
            *idx_ptr = ntdll_index as u32;
            println!("static TLS: {len} bytes + {zero_fill} zero-fill, owned by ntdll at index {ntdll_index} (helper DLL)");
            // Check that this thread's block really starts with the title's template.
            let teb: usize;
            std::arch::asm!("mov {}, gs:[0x30]", out(reg) teb, options(nostack, readonly, preserves_flags));
            let vector = *((teb + 0x58) as *const *const usize);
            let block = *vector.add(ntdll_index) as *const u8;
            if block.is_null() {
                println!("TLS check: this thread has no block at index {ntdll_index}");
            } else {
                let (a, b) = (std::slice::from_raw_parts(block, len), std::slice::from_raw_parts(start as *const u8, len));
                match a.iter().zip(b).position(|(x, y)| x != y) {
                    None => println!("TLS check: block at {block:p} matches the title's template ({len} bytes)"),
                    Some(i) => println!("TLS check: block at {block:p} differs from the template at offset {i:#x} ({:#x} vs {:#x})", a[i], b[i]),
                }
            }
        } else {
            TLS_TEMPLATE.store(start, Ordering::Relaxed);
            TLS_LEN.store(len, Ordering::Relaxed);
            TLS_TOTAL.store(len + zero_fill, Ordering::Relaxed);
            *idx_ptr = TLS_TITLE_INDEX as u32;
            install_tls_for_current_thread();
            println!("static TLS: {len} bytes + {zero_fill} zero-fill at index {TLS_TITLE_INDEX} (hand-built vector)");
        }

        if !callbacks.is_null() {
            let mut i = 0;
            while *callbacks.add(i) != 0 {
                let cb: extern "system" fn(*mut c_void, u32, *mut c_void) = std::mem::transmute(*callbacks.add(i));
                cb(base as *mut c_void, 1, ptr::null_mut());
                i += 1;
            }
            println!("ran {i} TLS callbacks");
        }
    }
    Ok(())
}

static STUB_NAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());
static HIT_LOG: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

/// Every unresolved import jumps here with its stub index in rcx.
/// Original arguments are lost for now; stubs return 0 (S_OK / NULL).
extern "system" fn stub_hit(idx: u64) -> u64 {
    let idx = idx as usize;
    if HIT_LOG.lock().unwrap().get_or_insert_with(HashSet::new).insert(idx) {
        let names = STUB_NAMES.lock().unwrap();
        eprintln!("[stub] first call: {}", names[idx]);
    }
    0
}

struct Pe<'a>(&'a [u8]);

impl Pe<'_> {
    fn u16(&self, o: usize) -> Result<u16, String> {
        self.0.get(o..o + 2).map(|b| u16::from_le_bytes(b.try_into().unwrap())).ok_or_else(|| format!("truncated read at {o:#x}"))
    }
    fn u32(&self, o: usize) -> Result<u32, String> {
        self.0.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).ok_or_else(|| format!("truncated read at {o:#x}"))
    }
    fn u64(&self, o: usize) -> Result<u64, String> {
        self.0.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).ok_or_else(|| format!("truncated read at {o:#x}"))
    }
}

/// Reads a NUL-terminated string from the mapped image.
unsafe fn cstr_at(base: *const u8, rva: u32) -> String {
    let p = base.add(rva as usize);
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    String::from_utf8_lossy(std::slice::from_raw_parts(p, n)).into_owned()
}

struct StubArena {
    mem: *mut u8,
    used: usize,
}

impl StubArena {
    const SLOT: usize = 128;
    const CAP: usize = 4096 * 64;

    fn new() -> Self {
        let mem = unsafe { VirtualAlloc(ptr::null_mut(), Self::CAP, MEM_COMMIT_RESERVE, PAGE_RWX) } as *mut u8;
        assert!(!mem.is_null(), "stub arena allocation failed");
        Self { mem, used: 0 }
    }

    /// Emits: mov rcx, idx ; mov rax, stub_hit ; jmp rax
    fn make(&mut self, label: String) -> u64 {
        assert!(self.used + Self::SLOT <= Self::CAP, "stub arena full");
        let idx = {
            let mut names = STUB_NAMES.lock().unwrap();
            names.push(label);
            names.len() - 1
        };
        let slot = unsafe { self.mem.add(self.used) };
        self.used += Self::SLOT;
        let mut code = Vec::with_capacity(22);
        code.extend_from_slice(&[0x48, 0xB9]);
        code.extend_from_slice(&(idx as u64).to_le_bytes());
        code.extend_from_slice(&[0x48, 0xB8]);
        code.extend_from_slice(&(stub_hit as usize as u64).to_le_bytes());
        code.extend_from_slice(&[0xFF, 0xE0]);
        unsafe { ptr::copy_nonoverlapping(code.as_ptr(), slot, code.len()) };
        slot as u64
    }

    /// Wraps a real function in a thunk that logs the call, preserving the argument
    /// registers (rcx, rdx, r8, r9, xmm0-3) and stack alignment, then jumps to `real`.
    fn make_trace(&mut self, label: String, real: u64) -> u64 {
        assert!(self.used + Self::SLOT <= Self::CAP, "stub arena full");
        let idx = {
            let mut names = TRACE_NAMES.lock().unwrap();
            names.push(label);
            names.len() - 1
        };
        let slot = unsafe { self.mem.add(self.used) };
        self.used += Self::SLOT;
        let mut c: Vec<u8> = vec![
            0x51, // push rcx
            0x52, // push rdx
            0x41, 0x50, // push r8
            0x41, 0x51, // push r9
            0x48, 0x83, 0xEC, 0x68, // sub rsp, 0x68
            0xF3, 0x0F, 0x7F, 0x44, 0x24, 0x20, // movdqu [rsp+20h], xmm0
            0xF3, 0x0F, 0x7F, 0x4C, 0x24, 0x30, // xmm1
            0xF3, 0x0F, 0x7F, 0x54, 0x24, 0x40, // xmm2
            0xF3, 0x0F, 0x7F, 0x5C, 0x24, 0x50, // xmm3
            0x48, 0x8B, 0x94, 0x24, 0x88, 0x00, 0x00, 0x00, // mov rdx, [rsp+88h] (caller's return address)
            0x48, 0xB9, // mov rcx, idx
        ];
        c.extend_from_slice(&(idx as u64).to_le_bytes());
        c.extend_from_slice(&[0x48, 0xB8]); // mov rax, trace_hit
        c.extend_from_slice(&(trace_hit as usize as u64).to_le_bytes());
        c.extend_from_slice(&[0xFF, 0xD0]); // call rax
        c.extend_from_slice(&[
            0xF3, 0x0F, 0x6F, 0x44, 0x24, 0x20, // movdqu xmm0, [rsp+20h]
            0xF3, 0x0F, 0x6F, 0x4C, 0x24, 0x30,
            0xF3, 0x0F, 0x6F, 0x54, 0x24, 0x40,
            0xF3, 0x0F, 0x6F, 0x5C, 0x24, 0x50,
            0x48, 0x83, 0xC4, 0x68, // add rsp, 0x68
            0x41, 0x59, // pop r9
            0x41, 0x58, // pop r8
            0x5A, // pop rdx
            0x59, // pop rcx
            0x48, 0xB8, // mov rax, real
        ]);
        c.extend_from_slice(&real.to_le_bytes());
        c.extend_from_slice(&[0xFF, 0xE0]); // jmp rax
        assert!(c.len() <= Self::SLOT);
        unsafe { ptr::copy_nonoverlapping(c.as_ptr(), slot, c.len()) };
        slot as u64
    }
}

/// Crash-proof trace: a memory-mapped file used as a circular text buffer. The OS keeps
/// the pages after the process dies, so the last calls survive even a fast-fail.
/// Layout: u64 total bytes written, then RING_SIZE bytes of text.
const RING_SIZE: usize = 64 << 20;
static RING: AtomicUsize = AtomicUsize::new(0);

fn ring_init(path: &str) -> Result<(), String> {
    let cpath = CString::new(path).map_err(|e| e.to_string())?;
    unsafe {
        // GENERIC_READ|GENERIC_WRITE, share read/write, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL
        let f = CreateFileA(cpath.as_ptr(), 0xC000_0000, 3, ptr::null_mut(), 2, 0x80, ptr::null_mut());
        if f as isize == -1 {
            return Err(format!("cannot create {path}"));
        }
        let m = CreateFileMappingA(f, ptr::null_mut(), 0x04, 0, (RING_SIZE + 8) as u32, ptr::null());
        if m.is_null() {
            return Err("CreateFileMapping failed".into());
        }
        let v = MapViewOfFile(m, 0x2, 0, 0, 0);
        if v.is_null() {
            return Err("MapViewOfFile failed".into());
        }
        RING.store(v as usize, Ordering::Relaxed);
    }
    Ok(())
}

fn ring_write(args: std::fmt::Arguments) {
    let ring = RING.load(Ordering::Relaxed);
    if ring == 0 {
        // XB1_FILES=1: the file-call loggers write to stderr instead of the ring.
        if FILES_TO_STDERR.load(Ordering::Relaxed) {
            raw_log(args);
        }
        return;
    }
    let mut w = RawBuf::new();
    let _ = std::fmt::write(&mut w, args);
    unsafe {
        let total = &*(ring as *const std::sync::atomic::AtomicU64);
        let start = total.fetch_add(w.n as u64, Ordering::Relaxed) as usize;
        let data = (ring + 8) as *mut u8;
        for (i, &b) in w.b[..w.n].iter().enumerate() {
            *data.add((start + i) % RING_SIZE) = b;
        }
    }
}

static TRACE_NAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());
static TRACE_API: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static TRACE_COUNT: AtomicUsize = AtomicUsize::new(0);
static NO_PEB_PATCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static TRACE_WINDOW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Called from the trace thunks; logs each API call the title makes (capped).
extern "system" fn trace_hit(idx: u64, ret: u64) {
    if RING.load(Ordering::Relaxed) != 0 {
        if let Ok(names) = TRACE_NAMES.try_lock() {
            let (base, size) = (IMG_BASE.load(Ordering::Relaxed), IMG_SIZE.load(Ordering::Relaxed));
            let ret = ret as usize;
            if ret >= base && ret < base + size {
                ring_write(format_args!("{} <- title+{:#x}\n", names[idx as usize], ret - base));
            } else {
                ring_write(format_args!("{} <- {ret:#x}\n", names[idx as usize]));
            }
        }
        return;
    }
    if let Ok(names) = TRACE_NAMES.try_lock() {
        let name = &names[idx as usize];
        // High-volume calls that only drown out the interesting ones.
        const NOISE: [&str; 10] = [
            "CriticalSection", "GetProcessHeap", "HeapAlloc", "HeapFree", "QueryPerformance",
            "TlsGetValue", "!memcpy", "!memmove", "!memset", "!memcmp",
        ];
        if NOISE.iter().any(|n| name.contains(n)) || TRACE_COUNT.fetch_add(1, Ordering::Relaxed) >= 20000 {
            return;
        }
        rlog!("[api] {name}");
    }
}

/// The title gets its own heap so it cannot corrupt (or be corrupted by) the host's.
static GUEST_HEAP: AtomicUsize = AtomicUsize::new(0);

extern "system" fn xb_get_process_heap() -> *mut c_void {
    GUEST_HEAP.load(Ordering::Relaxed) as *mut c_void
}

/// The folder `G:` was mapped to (so the mapping can be removed again), if this process made one.
static HWBP_SPEC: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static POKE_SPEC: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static PEEK_SPEC: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static FILES_TO_STDERR: AtomicBool = AtomicBool::new(false);
/// Logs (stderr) any single Rust heap allocation of 512 MiB or more with a backtrace: the layer's
/// own memory use, as opposed to the title's.
struct BigAllocLogger;

unsafe impl std::alloc::GlobalAlloc for BigAllocLogger {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        if layout.size() >= (512 << 20) {
            eprintln!("[mem] LAYER allocation of {} MiB
{}", layout.size() >> 20, std::backtrace::Backtrace::force_capture());
        }
        std::alloc::System.alloc(layout)
    }
    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        if layout.size() >= (512 << 20) {
            eprintln!("[mem] LAYER zeroed allocation of {} MiB
{}", layout.size() >> 20, std::backtrace::Backtrace::force_capture());
        }
        std::alloc::System.alloc_zeroed(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        std::alloc::System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        if new_size >= (512 << 20) {
            eprintln!("[mem] LAYER realloc to {} MiB
{}", new_size >> 20, std::backtrace::Backtrace::force_capture());
        }
        std::alloc::System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: BigAllocLogger = BigAllocLogger;

static BACKTRACE_ON_CRASH: AtomicBool = AtomicBool::new(false);

static CONTENT_DRIVE: Mutex<Option<Vec<u16>>> = Mutex::new(None);

/// Maps `G:` to the title folder for this logon session, because the title reads its game data
/// from `g:\media\...` (the console's content drive). An existing `G:` is left alone.
fn map_content_drive(dir: &Path) {
    use std::os::windows::ffi::OsStrExt;
    // A bare file name has an empty parent: use the full path of the current folder then.
    let dir = &if dir.as_os_str().is_empty() {
        std::env::current_dir().unwrap_or_default()
    } else {
        let full = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        std::path::PathBuf::from(full.display().to_string().trim_start_matches(r"\\?\"))
    };
    let name: Vec<u16> = vec![b'G' as u16, b':' as u16, 0];
    let mut buf = [0u16; 512];
    let got = unsafe { QueryDosDeviceW(name.as_ptr(), buf.as_mut_ptr(), 512) } as usize;
    if got != 0 {
        // "\??\C:older": ours if a previous run of this layer left it behind.
        let existing = String::from_utf16_lossy(&buf[..buf.iter().position(|c| *c == 0).unwrap_or(got)]);
        if existing.trim_start_matches(r"\??\").eq_ignore_ascii_case(&dir.display().to_string()) {
            println!("G: already maps to the title folder (left by an earlier run); reusing it");
            *CONTENT_DRIVE.lock().unwrap() = Some(Vec::new());
        } else {
            println!("G: is already in use ({existing}); not mapping the title folder to it");
        }
        return;
    }
    let target: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    if unsafe { DefineDosDeviceW(0, name.as_ptr(), target.as_ptr()) } != 0 {
        println!("mapped G: to {}", dir.display());
        *CONTENT_DRIVE.lock().unwrap() = Some(target);
    } else {
        println!("could not map G: to {} (error {})", dir.display(), unsafe { GetLastError() });
    }
}

/// The console's `T:` is a scratch drive the title may write to (Forza Horizon 3 creates `T:\Turn10Temp.scratch\`
/// and builds its temporary file system there; without it that file system does not exist and the title dereferences
/// NULL). Maps `T:` to a per-user scratch folder (`%TEMP%±layer-T`) when the letter is free.
static TEMP_DRIVE: Mutex<Option<Vec<u16>>> = Mutex::new(None);

fn map_temp_drive() {
    use std::os::windows::ffi::OsStrExt;
    if std::env::var_os("XB1_NO_TEMP_DRIVE").is_some() {
        return;
    }
    let name: Vec<u16> = vec![b'T' as u16, b':' as u16, 0];
    let mut buf = [0u16; 512];
    if unsafe { QueryDosDeviceW(name.as_ptr(), buf.as_mut_ptr(), 512) } != 0 {
        return; // in use: leave it alone
    }
    let dir = std::env::temp_dir().join("xb1layer-T");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let target: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    if unsafe { DefineDosDeviceW(0, name.as_ptr(), target.as_ptr()) } != 0 {
        println!("mapped T: to {}", dir.display());
        *TEMP_DRIVE.lock().unwrap() = Some(target);
    }
}

fn unmap_temp_drive() {
    if TEMP_DRIVE.lock().unwrap().take().is_some() {
        let name: Vec<u16> = vec![b'T' as u16, b':' as u16, 0];
        unsafe { DefineDosDeviceW(0x2, name.as_ptr(), ptr::null()) };
    }
}

/// Removes the `G:` mapping made by `map_content_drive`.
fn unmap_content_drive() {
    if CONTENT_DRIVE.lock().unwrap().take().is_some() {
        let name: Vec<u16> = vec![b'G' as u16, b':' as u16, 0];
        // DDD_REMOVE_DEFINITION (0x2) with no target removes the most recent mapping of the name.
        unsafe { DefineDosDeviceW(0x2, name.as_ptr(), ptr::null()) };
    }
}

/// Address of the field the hardware write breakpoint watches (XB1_HWBP), 0 when off.
static HWBP_ADDR: AtomicUsize = AtomicUsize::new(0);

/// Sets debug register 0 (write, 8 bytes) on a thread (a 16-byte aligned CONTEXT buffer is built
/// with only the debug registers).
unsafe fn arm_hwbp_on(thread: *mut c_void, addr: usize) {
    #[repr(C, align(16))]
    struct Ctx([u8; 1232]);
    let mut c = Ctx([0u8; 1232]);
    *(c.0.as_mut_ptr().add(0x30) as *mut u32) = 0x0010_0010; // CONTEXT_AMD64 | CONTEXT_DEBUG_REGISTERS
    *(c.0.as_mut_ptr().add(0x48) as *mut u64) = addr as u64;
    *(c.0.as_mut_ptr().add(0x70) as *mut u64) = 1 | (1 << 16) | (3 << 18);
    SetThreadContext(thread, c.0.as_ptr() as *const c_void);
}

/// Arms the hardware write breakpoint on every thread of this process except the caller.
unsafe fn arm_hwbp_all_threads(addr: usize) {
    HWBP_ADDR.store(addr, Ordering::Relaxed);
    let snap = CreateToolhelp32Snapshot(0x4, 0);
    if snap as isize == -1 {
        return;
    }
    let mut entry = [0u32; 7]; // THREADENTRY32: size, usage, tid, owner pid, base priority, delta, flags
    entry[0] = 28;
    let me = GetCurrentThreadId();
    let pid = GetCurrentProcessId();
    let mut ok = Thread32First(snap, entry.as_mut_ptr());
    while ok != 0 {
        if entry[3] == pid && entry[2] != me {
            // THREAD_SUSPEND_RESUME | THREAD_SET_CONTEXT | THREAD_GET_CONTEXT
            let h = OpenThread(0x2 | 0x10 | 0x8, 0, entry[2]);
            if !h.is_null() {
                SuspendThread(h);
                arm_hwbp_on(h, addr);
                ResumeThread(h);
                CloseHandle(h);
            }
        }
        ok = Thread32Next(snap, entry.as_mut_ptr());
    }
    CloseHandle(snap);
}

// ---------------------------------------------------------------------------------------
// Per-title compatibility patches
// ---------------------------------------------------------------------------------------

extern "system" fn stub_qi(_this: *mut c_void, _iid: *const c_void, out: *mut *mut c_void) -> i32 {
    if !out.is_null() {
        unsafe { *out = ptr::null_mut() };
    }
    0x8000_4002u32 as i32 // E_NOINTERFACE
}
extern "system" fn stub_ref(_this: *mut c_void) -> u32 {
    1
}
extern "system" fn stub_ok(_this: *mut c_void) -> usize {
    0
}
/// Slot 39, `Open(out**, name, flags, flag)` (title call site 0xd93e75): the caller does not
/// initialise `out`, so it is set to NULL ("not found").
extern "system" fn stub_open(_this: *mut c_void, out: *mut *mut c_void) -> usize {
    if !out.is_null() {
        unsafe { *out = ptr::null_mut() };
    }
    0
}

/// A COM-shaped object whose methods all succeed and write nothing (out-pointers stay as the
/// caller initialised them, normally NULL), for objects the title expects but nothing provides.
pub fn null_object() -> usize {
    static OBJ: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *OBJ.get_or_init(|| {
        let mut v = vec![stub_ok as usize; 96];
        v[0] = stub_qi as usize;
        v[1] = stub_ref as usize;
        v[2] = stub_ref as usize;
        v[39] = stub_open as usize;
        let vtbl = Vec::leak(v).as_ptr() as usize;
        Box::into_raw(Box::new([vtbl, 0usize])) as usize
    })
}

/// The test title's `CFileSys` (RTTI `.?AVCFileSys@@`) keeps an object at `+0x88` that the
/// layer cannot provide (nothing in the run ever writes it, and the callers use it to open
/// `...\Manifest.xml`); its constructor stores 0 there. The two zeroing stores at
/// `title+0x158c84d` (`mov [rbx+0x88], rax` / `mov [rbx+0x98], rax`) are replaced by a jump to a
/// thunk that stores a null object at `+0x88` instead, so callers take their "nothing found"
/// paths rather than dereferencing NULL. Applied only if the original bytes match.
unsafe fn patch_filesys_storage(base: *mut u8, size: usize) {
    const SITE: usize = 0x158c84d;
    const ORIGINAL: [u8; 14] = [0x48, 0x89, 0x83, 0x88, 0, 0, 0, 0x48, 0x89, 0x83, 0x98, 0, 0, 0];
    if SITE + 14 > size || std::slice::from_raw_parts(base.add(SITE), 14) != ORIGINAL {
        return;
    }
    let thunk = VirtualAlloc(ptr::null_mut(), 64, MEM_COMMIT_RESERVE, PAGE_RWX) as *mut u8;
    let mut code: Vec<u8> = vec![0x48, 0xB8]; // mov rax, null_object
    code.extend_from_slice(&(null_object() as u64).to_le_bytes());
    code.extend_from_slice(&[0x48, 0x89, 0x83, 0x88, 0, 0, 0]); // mov [rbx+0x88], rax
    code.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax
    code.extend_from_slice(&[0x48, 0x89, 0x83, 0x98, 0, 0, 0]); // mov [rbx+0x98], rax
    code.extend_from_slice(&[0xFF, 0x25, 0, 0, 0, 0]); // jmp [rip+0]
    code.extend_from_slice(&((base as u64) + (SITE as u64) + 14).to_le_bytes());
    ptr::copy_nonoverlapping(code.as_ptr(), thunk, code.len());
    let mut jump: Vec<u8> = vec![0xFF, 0x25, 0, 0, 0, 0];
    jump.extend_from_slice(&(thunk as u64).to_le_bytes());
    ptr::copy_nonoverlapping(jump.as_ptr(), base.add(SITE), 14);
    println!("compat: CFileSys storage object stubbed (title+{SITE:#x})");
}

/// Minecraft Xbox One Edition 1.2.0.0: the storage manager copies the loaded player profile (`memcpy(block, buffer, n)`
/// at `title+0x5a8b4a`) over the default options the game has just set. The layer's storage stand-ins complete the
/// load without any data, so the buffer is blank and the copy zeroed the look sensitivity and the other options. A record
/// without any option set means "no saved profile": the copy is skipped and the defaults stay. Applied only if the original
/// bytes match (`call memcpy; mov rcx,[rdi]; call [rip+disp]`).
unsafe fn patch_profile_copy(base: *mut u8, size: usize) {
    const SITE: usize = 0x5a8b4a;
    const MEMCPY_STUB: usize = 0x6ad62a;
    if SITE + 14 > size {
        return;
    }
    let site = std::slice::from_raw_parts(base.add(SITE), 14);
    let rel = i32::from_le_bytes([site[1], site[2], site[3], site[4]]);
    let target = (SITE as i64 + 5 + rel as i64) as usize;
    if site[0] != 0xE8 || target != MEMCPY_STUB || site[5..8] != [0x48, 0x8B, 0x0F] || site[8..10] != [0xFF, 0x15] {
        return;
    }
    let disp = i32::from_le_bytes([site[10], site[11], site[12], site[13]]);
    let slot = base as usize + (SITE as i64 + 14 + disp as i64) as usize; // the IAT slot `call [rip+disp]` uses
    let thunk = VirtualAlloc(ptr::null_mut(), 96, MEM_COMMIT_RESERVE, PAGE_RWX) as *mut u8;
    let mut code: Vec<u8> = vec![0x48, 0xB8]; // mov rax, compat_profile_copy
    code.extend_from_slice(&(compat_profile_copy as usize as u64).to_le_bytes());
    code.extend_from_slice(&[0xFF, 0xD0]); // call rax
    code.extend_from_slice(&[0x48, 0x8B, 0x0F]); // mov rcx, [rdi]
    code.extend_from_slice(&[0x48, 0xB8]); // mov rax, slot
    code.extend_from_slice(&(slot as u64).to_le_bytes());
    code.extend_from_slice(&[0xFF, 0x10]); // call [rax]
    code.extend_from_slice(&[0xFF, 0x25, 0, 0, 0, 0]); // jmp [rip+0]
    code.extend_from_slice(&((base as u64) + (SITE as u64) + 14).to_le_bytes());
    ptr::copy_nonoverlapping(code.as_ptr(), thunk, code.len());
    let mut jump: Vec<u8> = vec![0xFF, 0x25, 0, 0, 0, 0];
    jump.extend_from_slice(&(thunk as u64).to_le_bytes());
    ptr::copy_nonoverlapping(jump.as_ptr(), base.add(SITE), 14);
    println!("compat: profile copy skips a profile record without options (title+{SITE:#x})");
}

/// Experiment (`XB1_PREMOUNT=1`, Forza Horizon 3 only): the title registers the directories of the zips listed in
/// `media\zipmanifest.xml` but only mounts the ones it enumerates, so its render scenarios are never found. Right before
/// it loads them (title+0x1899469, a call to title+0x1b04d40) the layer opens each manifest directory through the
/// content manager's "open directory" method (vtable slot 0x1d8), which is what the title does for its UI zips and which
/// opens and mounts the zip. Byte-guarded: nothing happens unless the code at the site is the expected one.
unsafe fn patch_premount_zips(base: *mut u8, size: usize) {
    const SITE: usize = 0x1899462;
    const LOADER: usize = 0x1b04d40;
    const NEXT: usize = 0x1885730;
    if SITE + 17 > size {
        return;
    }
    let b = std::slice::from_raw_parts(base.add(SITE), 17);
    let rel = |at: usize, off: usize| -> usize { (SITE as i64 + at as i64 + 5 + i32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]]) as i64) as usize };
    if b[0..3] != [0x48, 0x8B, 0x0D] || b[7] != 0xE8 || b[12] != 0xE8 || rel(7, 8) != LOADER || rel(12, 13) != NEXT {
        return;
    }
    let global = (SITE as i64 + 7 + i32::from_le_bytes([b[3], b[4], b[5], b[6]]) as i64) as usize;
    let thunk = VirtualAlloc(ptr::null_mut(), 128, MEM_COMMIT_RESERVE, PAGE_RWX) as *mut u8;
    let mut code: Vec<u8> = Vec::new();
    let mut call_abs = |code: &mut Vec<u8>, addr: u64| {
        code.extend_from_slice(&[0x48, 0xB8]);
        code.extend_from_slice(&addr.to_le_bytes());
        code.extend_from_slice(&[0xFF, 0xD0]);
    };
    call_abs(&mut code, premount_zips as usize as u64);
    code.extend_from_slice(&[0x48, 0xB8]); // mov rax, global
    code.extend_from_slice(&((base as u64) + global as u64).to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8B, 0x08]); // mov rcx, [rax]
    call_abs(&mut code, (base as u64) + LOADER as u64);
    call_abs(&mut code, (base as u64) + NEXT as u64);
    code.extend_from_slice(&[0xFF, 0x25, 0, 0, 0, 0]);
    code.extend_from_slice(&((base as u64) + SITE as u64 + 17).to_le_bytes());
    ptr::copy_nonoverlapping(code.as_ptr(), thunk, code.len());
    let mut jump: Vec<u8> = vec![0xFF, 0x25, 0, 0, 0, 0];
    jump.extend_from_slice(&(thunk as u64).to_le_bytes());
    ptr::copy_nonoverlapping(jump.as_ptr(), base.add(SITE), 14);
    println!("compat: premount hook installed before the render scenario load (title+{SITE:#x})");
}

/// Opens every manifest zip's directory through the title's content manager (see `patch_premount_zips`).
extern "system" fn premount_zips() {
    unsafe {
        let base = IMG_BASE.load(Ordering::Relaxed);
        let Ok(text) = std::fs::read_to_string("./media/zipmanifest.xml") else {
            rlog!("[premount] media/zipmanifest.xml unreadable");
            return;
        };
        let mgr = *((base + 0x4e7c920) as *const usize);
        if mgr == 0 {
            rlog!("[premount] the content manager does not exist yet");
            return;
        }
        let open_dir: extern "system" fn(usize, *mut u8, *mut u8, usize, u64, u64, u64, u64) -> *mut u8 =
            std::mem::transmute(*((*(mgr as *const usize) + 0x1d8) as *const usize));
        let make_string: extern "system" fn(*mut u8, *const u8, usize) -> *mut u8 = std::mem::transmute(base + 0x2451a0);
        let skip: Vec<String> = std::env::var("XB1_PREMOUNT_SKIP").unwrap_or_else(|_| "game:\\media\\ui\\;game:\\media\\drivers\\;game:\\media\\stripped\\".into()).split(';').map(str::to_owned).collect();
        let mut seen = std::collections::HashSet::new();
        let mut opened = 0usize;
        // XB1_PREMOUNT_EXTRA="dir;dir": more directories to open (experiment), after the manifest ones.
        let extra: Vec<String> = std::env::var("XB1_PREMOUNT_EXTRA").unwrap_or_default().split(';').filter(|d| !d.is_empty()).map(str::to_owned).collect();
        for part in text.split("path=\"").skip(1) {
            let Some(end) = part.find('"') else { continue };
            let path = part[..end].to_ascii_lowercase();
            let Some(stem) = path.strip_suffix(".zip") else { continue };
            let stem = match stem.rfind("_pri_") { Some(i) if stem[i + 5..].bytes().all(|c| c.is_ascii_digit()) => &stem[..i], _ => stem };
            let dir = format!("{stem}\\");
            if !dir.starts_with("game:\\media\\") || skip.iter().any(|k| !k.is_empty() && dir.starts_with(k.as_str())) || !seen.insert(dir.clone()) {
                continue;
            }
            let mut name = [0u8; 32];
            *(name.as_mut_ptr().add(0x18) as *mut u64) = 0xf;
            make_string(name.as_mut_ptr(), dir.as_ptr(), dir.len());
            let mut out = [0u8; 0x100];
            open_dir(mgr, out.as_mut_ptr(), name.as_mut_ptr(), 0, 0, 0, 0, 1);
            opened += 1;
            if opened <= 5 || opened % 100 == 0 || dir.contains("renderscenarios") {
                let obj = *(out.as_ptr().add(0x28) as *const usize);
                rlog!("[premount] {dir} -> object {obj:#x}");
                rlog!("[premount] opened {opened}: {dir}");
            }
        }
        for dir in &extra {
            let mut name = [0u8; 32];
            *(name.as_mut_ptr().add(0x18) as *mut u64) = 0xf;
            make_string(name.as_mut_ptr(), dir.as_ptr(), dir.len());
            let mut out = [0u8; 0x100];
            open_dir(mgr, out.as_mut_ptr(), name.as_mut_ptr(), 0, 0, 0, 0, 1);
            rlog!("[premount] extra {dir} -> object {:#x}", *(out.as_ptr().add(0x28) as *const usize));
        }
        rlog!("[premount] opened {opened} manifest directories");
        // The zips' directories are read with overlapped I/O that the title's worker threads complete.
        let wait = std::env::var("XB1_PREMOUNT_WAIT_MS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(3000);
        std::thread::sleep(std::time::Duration::from_millis(wait));
        // Debug: the title's registry of known files (title+0x4fa4208 .. 0x4fa4210), looked up by exact name.
        let (begin, end) = (*((base + 0x4fa4208) as *const usize), *((base + 0x4fa4210) as *const usize));
        let count = end.wrapping_sub(begin) / 8;
        rlog!("[premount] file registry holds {count} entries");
        if count < 1_000_000 {
            let mut shown = 0;
            for i in 0..count {
                let e = *((begin + i * 8) as *const usize);
                if e == 0 { continue; }
                let size = *((e + 0xc0 + 0x10) as *const usize);
                let cap = *((e + 0xc0 + 0x18) as *const usize);
                let p = if cap >= 16 { *((e + 0xc0) as *const usize) } else { e + 0xc0 };
                if size > 0 && size < 260 {
                    let name = String::from_utf8_lossy(std::slice::from_raw_parts(p as *const u8, size)).to_string();
                    if (name.to_ascii_lowercase().contains("renderscenarios") && shown < 5) || i < 3 {
                        rlog!("[premount]   entry {i}: {name}");
                        shown += 1;
                    }
                }
            }
        }
    }
}

/// `memcpy(dst, src, n)` that leaves `dst` alone when `src` is all zeros (see `patch_profile_copy`).
extern "system" fn compat_profile_copy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    unsafe {
        if dst.is_null() || src.is_null() || n == 0 {
            return dst;
        }
        rlog!("[compat] profile copy: {n} bytes, source starts {:02x?}", std::slice::from_raw_parts(src, n.min(8)));
        // The option bytes (1 to 6) of a saved profile are never all zero (sensitivities, volumes, ...); byte 7 holds a
        // header value (0x96) even in the blank record the stand-in storage hands back.
        if n >= 8 && n <= 4096 && std::slice::from_raw_parts(src, n)[1..7].iter().all(|b| *b == 0) {
            rlog!("[compat] profile record has no options set ({n} bytes): the defaults are kept");
            return dst;
        }
        ptr::copy(src, dst, n);
    }
    dst
}

/// Logs who ends the process, then does it.
fn log_exit_request(what: &str, code: u32) {
    let (base, size) = (IMG_BASE.load(Ordering::Relaxed), IMG_SIZE.load(Ordering::Relaxed));
    rlog!("[exit] {what}({code:#x})");
    rlog!("{}", std::backtrace::Backtrace::force_capture());
    let mut frames = [ptr::null_mut::<c_void>(); 32];
    let n = unsafe { RtlCaptureStackBackTrace(0, 32, frames.as_mut_ptr(), ptr::null_mut()) } as usize;
    for f in frames[..n].iter().map(|f| *f as usize).take(16) {
        if f >= base && f < base + size {
            rlog!("  caller title+{:#x}", f - base);
            continue;
        }
        let mut module: *mut c_void = ptr::null_mut();
        // GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | UNCHANGED_REFCOUNT
        if unsafe { GetModuleHandleExA(0x6, f as *const i8, &mut module) } != 0 {
            let mut name = [0u8; 260];
            let len = unsafe { GetModuleFileNameA(module, name.as_mut_ptr(), 260) } as usize;
            let full = String::from_utf8_lossy(&name[..len]).into_owned();
            rlog!("  frame {}+{:#x}", full.rsplit('\\').next().unwrap_or(""), f - module as usize);
        } else {
            rlog!("  frame {f:#x}");
        }
    }
}

extern "system" fn hooked_rtl_exit_user_process(code: u32) {
    log_exit_request("RtlExitUserProcess", code);
    unsafe {
        let nt = GetProcAddress(GetModuleHandleA(c"ntdll.dll".as_ptr()), c"NtTerminateProcess".as_ptr());
        let f: extern "system" fn(isize, u32) -> i32 = std::mem::transmute(nt);
        f(-1, code);
    }
}

/// Patches the start of `ntdll!RtlExitUserProcess` with `mov rax, hook; jmp rax` so every normal
/// exit path (ExitProcess, the CRT's exit) is logged with the title's call chain.
fn hook_exit_path() {
    unsafe {
        let target = GetProcAddress(GetModuleHandleA(c"ntdll.dll".as_ptr()), c"RtlExitUserProcess".as_ptr()) as *mut u8;
        if target.is_null() {
            return;
        }
        let mut old = 0u32;
        if VirtualProtect(target as *mut c_void, 16, 0x40, &mut old) == 0 {
            return;
        }
        let mut code = [0x48u8, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xE0];
        code[2..10].copy_from_slice(&(hooked_rtl_exit_user_process as usize as u64).to_le_bytes());
        ptr::copy_nonoverlapping(code.as_ptr(), target, code.len());
    }
}

static NT_TERMINATE_TRAMPOLINE: AtomicUsize = AtomicUsize::new(0);

extern "system" fn hooked_nt_terminate_process(process: isize, status: u32) -> i32 {
    if process == -1 || process == 0 {
        log_exit_request("NtTerminateProcess", status);
        unmap_content_drive();
        unmap_temp_drive();
    }
    let f: extern "system" fn(isize, u32) -> i32 = unsafe { std::mem::transmute(NT_TERMINATE_TRAMPOLINE.load(Ordering::Relaxed)) };
    f(process, status)
}

/// Hooks `ntdll!NtTerminateProcess` (`mov r10, rcx; mov eax, n` = 8 bytes are copied to a trampoline
/// that continues into the original at +8).
fn hook_nt_terminate() {
    unsafe {
        let target = GetProcAddress(GetModuleHandleA(c"ntdll.dll".as_ptr()), c"NtTerminateProcess".as_ptr()) as *mut u8;
        if target.is_null() || *target != 0x4C || *target.add(3) != 0xB8 || *target.add(8) != 0xF6 {
            rlog!("[exit] NtTerminateProcess has an unexpected prologue; not hooked");
            return;
        }
        let tramp = VirtualAlloc(ptr::null_mut(), 64, 0x3000, 0x40) as *mut u8;
        ptr::copy_nonoverlapping(target, tramp, 16);
        let mut jump = [0x48u8, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xE0];
        jump[2..10].copy_from_slice(&(target as u64 + 16).to_le_bytes());
        ptr::copy_nonoverlapping(jump.as_ptr(), tramp.add(16), jump.len());
        NT_TERMINATE_TRAMPOLINE.store(tramp as usize, Ordering::Relaxed);
        let mut old = 0u32;
        if VirtualProtect(target as *mut c_void, 16, 0x40, &mut old) == 0 {
            return;
        }
        let mut hook = [0x48u8, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xE0];
        hook[2..10].copy_from_slice(&(hooked_nt_terminate_process as usize as u64).to_le_bytes());
        ptr::copy_nonoverlapping(hook.as_ptr(), target, hook.len());
    }
}

static NT_CREATE_THREAD_TRAMPOLINE: AtomicUsize = AtomicUsize::new(0);

/// `NtCreateThreadEx(thread*, access, attrs, process, start, arg, flags, zero_bits, stack_commit, stack_reserve, attr_list)`
extern "system" fn hooked_nt_create_thread_ex(
    a1: usize, a2: usize, a3: usize, a4: usize, start: usize, a6: usize, a7: usize, a8: usize, a9: usize, a10: usize, a11: usize,
) -> i32 {
    let (base, size) = (IMG_BASE.load(Ordering::Relaxed), IMG_SIZE.load(Ordering::Relaxed));
    if start < base || start >= base + size {
        let mut module: *mut c_void = ptr::null_mut();
        let mut name = String::from("?");
        unsafe {
            if GetModuleHandleExA(0x6, start as *const i8, &mut module) != 0 {
                let mut buf = [0u8; 260];
                let len = GetModuleFileNameA(module, buf.as_mut_ptr(), 260) as usize;
                name = format!("{}+{:#x}", String::from_utf8_lossy(&buf[..len]).rsplit('\\').next().unwrap_or(""), start - module as usize);
            }
        }
        rlog!("[thread] NtCreateThreadEx start {name} arg {a6:#x} process {a4:#x}");
        let mut frames = [ptr::null_mut::<c_void>(); 16];
        let n = unsafe { RtlCaptureStackBackTrace(1, 16, frames.as_mut_ptr(), ptr::null_mut()) } as usize;
        for f in frames[..n].iter().map(|f| *f as usize) {
            let mut m: *mut c_void = ptr::null_mut();
            if unsafe { GetModuleHandleExA(0x6, f as *const i8, &mut m) } != 0 {
                let mut buf = [0u8; 260];
                let len = unsafe { GetModuleFileNameA(m, buf.as_mut_ptr(), 260) } as usize;
                rlog!("  frame {}+{:#x}", String::from_utf8_lossy(&buf[..len]).rsplit('\\').next().unwrap_or(""), f - m as usize);
            }
        }
    }
    let f: extern "system" fn(usize, usize, usize, usize, usize, usize, usize, usize, usize, usize, usize) -> i32 =
        unsafe { std::mem::transmute(NT_CREATE_THREAD_TRAMPOLINE.load(Ordering::Relaxed)) };
    f(a1, a2, a3, a4, start, a6, a7, a8, a9, a10, a11)
}

fn hook_nt_create_thread() {
    unsafe {
        let target = GetProcAddress(GetModuleHandleA(c"ntdll.dll".as_ptr()), c"NtCreateThreadEx".as_ptr()) as *mut u8;
        if target.is_null() || *target != 0x4C || *target.add(3) != 0xB8 || *target.add(8) != 0xF6 {
            return;
        }
        let tramp = VirtualAlloc(ptr::null_mut(), 64, 0x3000, 0x40) as *mut u8;
        ptr::copy_nonoverlapping(target, tramp, 16);
        let mut jump = [0x48u8, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xE0];
        jump[2..10].copy_from_slice(&(target as u64 + 16).to_le_bytes());
        ptr::copy_nonoverlapping(jump.as_ptr(), tramp.add(16), jump.len());
        NT_CREATE_THREAD_TRAMPOLINE.store(tramp as usize, Ordering::Relaxed);
        let mut old = 0u32;
        if VirtualProtect(target as *mut c_void, 16, 0x40, &mut old) == 0 {
            return;
        }
        let mut hook = [0x48u8, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xE0];
        hook[2..10].copy_from_slice(&(hooked_nt_create_thread_ex as usize as u64).to_le_bytes());
        ptr::copy_nonoverlapping(hook.as_ptr(), target, hook.len());
    }
}

extern "system" fn xb_exit_process(code: u32) {
    log_exit_request("ExitProcess", code);
    unsafe { ExitProcess(code) }
}

extern "system" fn xb_terminate_process(process: *mut c_void, code: u32) -> i32 {
    if process as isize == -1 {
        log_exit_request("TerminateProcess", code);
    }
    unsafe { TerminateProcess(process, code) }
}

/// `CELL::details::CellGlobalAllocator::Get()` of the title's (missing) telemetry library: the title
/// calls `allocator->vtbl[0](allocator, size)` on the result. Slot 0 allocates from the guest heap,
/// slot 1 frees; the other slots are assumed and do nothing.
#[repr(C)]
struct CellAllocator {
    vtbl: *const usize,
}

extern "system" fn cell_alloc(_this: *mut c_void, size: usize) -> *mut c_void {
    unsafe { HeapAlloc(GUEST_HEAP.load(Ordering::Relaxed) as *mut c_void, 0, size.max(1)) }
}

extern "system" fn cell_free(_this: *mut c_void, p: *mut c_void) {
    if !p.is_null() {
        unsafe { HeapFree(GUEST_HEAP.load(Ordering::Relaxed) as *mut c_void, 0, p) };
    }
}

extern "system" fn cell_nop(_this: *mut c_void) -> usize {
    0
}

extern "system" fn cell_global_allocator_get() -> *mut c_void {
    static VTBL: [usize; 8] = [0; 8];
    static OBJ: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let _ = &VTBL;
    *OBJ.get_or_init(|| {
        let mut v = vec![cell_nop as usize; 8];
        v[0] = cell_alloc as usize;
        v[1] = cell_free as usize;
        let vtbl = Vec::leak(v).as_ptr();
        Box::into_raw(Box::new(CellAllocator { vtbl })) as usize
    }) as *mut c_void
}

/// `CELL::CellGlobal::CreateCellInternal()`: an `ICell` the title logs through (the real interface
/// is unknown).
extern "system" fn cell_return_self(this: *mut c_void) -> *mut c_void {
    this
}

extern "system" fn cell_create_cell_internal() -> *mut c_void {
    static OBJ: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    // A null object: every method returns the object itself, because the title uses the result of
    // `ICell` slot 3 (offset 0x18) as another object and calls through it.
    *OBJ.get_or_init(|| {
        let vtbl = Vec::leak(vec![cell_return_self as usize; 64]).as_ptr();
        Box::into_raw(Box::new(CellAllocator { vtbl })) as usize
    }) as *mut c_void
}

/// Zeroed, leaked memory for the telemetry library's `get_Global*` tables (the title reads and
/// writes them; the real element types are unknown).
extern "system" fn cell_zeroed_table() -> *mut c_void {
    static BUF: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *BUF.get_or_init(|| Vec::leak(vec![0u8; 1 << 16]).as_ptr() as usize) as *mut c_void
}

// ---------------------------------------------------------------------------------------
// AcpHal.dll: the console audio processor's HAL (the audio engine's memory and contexts)
// ---------------------------------------------------------------------------------------
//
// Signatures are from memory of the XDK headers and from the call sites, not verified, so every
// call logs its raw arguments. `ApuAlloc(void** va, UINT32* physical, size, alignment, flags)` is
// treated as an allocator of zeroed memory; the rest succeed.

static APU_CALLS: AtomicU32 = AtomicU32::new(0);

fn apu_log(name: &str, a: [usize; 5]) {
    if APU_CALLS.fetch_add(1, Ordering::Relaxed) < 40 {
        rlog!("[acp] {name}({:#x}, {:#x}, {:#x}, {:#x}, {:#x})", a[0], a[1], a[2], a[3], a[4]);
    }
}

/// A HAL object whose methods all succeed (slot 0 is called by the title as an ordinary method,
/// `(this, 0x400, 0x400)`, so it cannot be a `QueryInterface`).
fn acp_object() -> usize {
    static OBJ: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *OBJ.get_or_init(|| {
        let vtbl = Vec::leak(vec![stub_ok as usize; 96]).as_ptr() as usize;
        Box::into_raw(Box::new([vtbl, 0usize])) as usize
    })
}

/// `AcpHalCreate(IAcpHal** out, ...)`: the object comes back through the first argument (title
/// call site 0x23f6b58, which then calls slot 0 on it).
extern "system" fn acp_create(out: *mut usize, a2: usize, a3: usize, a4: usize, a5: usize) -> i32 {
    apu_log("AcpHalCreate", [out as usize, a2, a3, a4, a5]);
    if !out.is_null() && (out as usize) % 8 == 0 {
        unsafe { *out = acp_object() };
    }
    0
}
extern "system" fn apu_create_heap(a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> i32 {
    apu_log("ApuCreateHeap", [a1, a2, a3, a4, a5]);
    0
}
extern "system" fn apu_alloc(out: *mut usize, physical: *mut u32, size: usize, align: usize, flags: usize) -> i32 {
    apu_log("ApuAlloc", [out as usize, physical as usize, size, align, flags]);
    let size = (size.max(1) + 0xFFFF) & !0xFFFF;
    let mem = unsafe { VirtualAlloc(ptr::null_mut(), size, MEM_COMMIT_RESERVE, 0x04) } as usize;
    if mem == 0 {
        return 0x8007000E_u32 as i32; // E_OUTOFMEMORY
    }
    if !out.is_null() {
        unsafe { *out = mem };
    }
    if !physical.is_null() {
        unsafe { *physical = mem as u32 };
    }
    0
}
extern "system" fn apu_free(a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> i32 {
    apu_log("ApuFree", [a1, a2, a3, a4, a5]);
    0
}
extern "system" fn apu_is_va_valid(a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> i32 {
    apu_log("ApuIsVirtualAddressValid", [a1, a2, a3, a4, a5]);
    1
}
extern "system" fn apu_map_va(va: usize, a2: usize, physical: *mut u32, a4: usize, a5: usize) -> i32 {
    apu_log("ApuMapVirtualAddress", [va, a2, physical as usize, a4, a5]);
    if !physical.is_null() && (physical as usize) % 4 == 0 {
        unsafe { *physical = va as u32 };
    }
    0
}
/// `AcpHalAllocateShapeContexts(config, 12, 0x50, 0x60, ...)`: `config` is the title's stack
/// structure (`{0x200, .., 0x400 at +0xc, 0x200 at +0x10, ..}`); the call fills pointer fields at
/// `+0x18` (the title then zeroes count*32 bytes there), `+0x30` (count*16) and `+0x38` (count*64). Each gets a
/// zeroed block. Inferred from the call site at title+0x23f6ad1; other fields are unknown.
extern "system" fn acp_allocate_shape_contexts(config: *mut u8, a2: usize, a3: usize, a4: usize, a5: usize) -> i32 {
    apu_log("AcpHalAllocateShapeContexts", [config as usize, a2, a3, a4, a5]);
    if !config.is_null() {
        for off in [0x18usize, 0x30, 0x38] {
            let block = unsafe { VirtualAlloc(ptr::null_mut(), 1 << 20, MEM_COMMIT_RESERVE, 0x04) } as usize;
            unsafe { ptr::write_unaligned(config.add(off) as *mut usize, block) };
        }
    }
    0
}
extern "system" fn acp_release_shape_contexts(a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> i32 {
    apu_log("AcpHalReleaseShapeContexts", [a1, a2, a3, a4, a5]);
    0
}

/// `D3D11CreateDevice(adapter, driver_type, software, flags, feature_levels*, n_levels, sdk_version, device**, level*, context**)`.
extern "system" fn xb_d3d11_create_device(
    adapter: *mut c_void, driver_type: u32, software: *mut c_void, flags: u32, levels: *const u32, n_levels: u32, sdk: u32,
    device: *mut *mut c_void, level: *mut u32, context: *mut *mut c_void,
) -> i32 {
    let m = unsafe { LoadLibraryA(c"d3d11.dll".as_ptr()) };
    let f = unsafe { GetProcAddress(m, c"D3D11CreateDevice".as_ptr()) };
    let real: extern "system" fn(*mut c_void, u32, *mut c_void, u32, *const u32, u32, u32, *mut *mut c_void, *mut u32, *mut *mut c_void) -> i32 =
        unsafe { std::mem::transmute(f) };
    let first = if !levels.is_null() && n_levels > 0 { unsafe { *levels } } else { 0 };
    // The console SDK version (131090) is not a desktop one; D3D11_SDK_VERSION is 7.
    let debug = std::env::var_os("XB1_D3D_DEBUG").is_some();
    let flags = if debug { flags | 0x2 } else { flags }; // D3D11_CREATE_DEVICE_DEBUG
    let hr = real(adapter, driver_type, software, flags, levels, n_levels, 7, device, level, context);
    if debug && hr >= 0 && !device.is_null() {
        unsafe { gpu::start_debug_pump_raw(*device) };
    }
    rlog!(
        "[gpu] D3D11CreateDevice(adapter {adapter:p}, driver {driver_type}, flags {flags:#x}, {n_levels} level(s), first {first:#x}, sdk {sdk}) -> {:#x}",
        hr as u32
    );
    if hr >= 0 && std::env::var_os("XB1_TEST_DEFERRED").is_some() {
        // Experiment: does CreateDeferredContext work on the fresh device at all?
        let dev = unsafe { *device };
        let create: extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32 = unsafe { std::mem::transmute(*((*(dev as *const *const usize)).add(27))) };
        let mut ctx: *mut c_void = ptr::null_mut();
        let r = create(dev, 0, &mut ctx);
        rlog!("[gpu] test CreateDeferredContext on the fresh device -> {:#x} {ctx:p}", r as u32);
    }
    if hr >= 0 {
        unsafe { gpu::wrap_created_device(device, context) };
        display::install();
    }
    hr
}

/// `CreateXAudio2Object(IXAudio2** out, UINT32 flags, XAUDIO2_PROCESSOR processor)`: the console
/// runtime's entry point, taken to have `XAudio2Create`'s shape (the desktop DLL does not export it).
extern "system" fn xb_create_xaudio2_object(out: *mut *mut c_void, flags: u32, processor: u32) -> i32 {
    let m = unsafe { LoadLibraryA(c"xaudio2_9.dll".as_ptr()) };
    let f = unsafe { GetProcAddress(m, c"XAudio2Create".as_ptr()) };
    if f.is_null() {
        return 0x8007007Fu32 as i32;
    }
    let create: extern "system" fn(*mut *mut c_void, u32, u32) -> i32 = unsafe { std::mem::transmute(f) };
    // The console processor selector is not a desktop one; XAUDIO2_DEFAULT_PROCESSOR is 1.
    let hr = create(out, flags & 0, 1);
    let _ = processor;
    rlog!("[audio] CreateXAudio2Object -> XAudio2Create -> {:#x}", hr as u32);
    hr
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryW(name: *const u16) -> *mut c_void;
}

extern "system" fn xb_load_library_w(name: *const u16) -> *mut c_void {
    let shown = if name.is_null() { String::new() } else { wide_to_string(name) };
    let r = unsafe { LoadLibraryW(name) };
    rlog!("[dll] LoadLibraryW({shown}) -> {r:p}");
    r
}

/// `GetProcAddress` for the title: names that only exist in console DLLs are redirected.
extern "system" fn xb_get_proc_address(module: *mut c_void, name: *const i8) -> *mut c_void {
    let p = unsafe { GetProcAddress(module, name) };
    if p.is_null() && (name as usize) > 0xFFFF {
        let n = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy();
        if n == "CreateXAudio2Object" {
            rlog!("[audio] GetProcAddress(CreateXAudio2Object) -> shim over XAudio2Create");
            return xb_create_xaudio2_object as usize as *mut c_void;
        }
        rlog!("[proc] GetProcAddress({n}) not found");
    }
    p
}

/// Set when the title links the Visual C++ 2012 runtime (`vccorlib110`) instead of the 2015+ one.
static VCCORLIB110: AtomicBool = AtomicBool::new(false);

/// Imports we implement ourselves instead of forwarding to Windows.
fn override_for(dll: &str, func: &str) -> Option<usize> {
    if (dll.eq_ignore_ascii_case("vccorlib140.dll") || dll.eq_ignore_ascii_case("vccorlib110.dll")) && func.starts_with("?GetActivationFactoryByPCWSTR") {
        if dll.eq_ignore_ascii_case("vccorlib110.dll") {
            VCCORLIB110.store(true, Ordering::Relaxed);
        }
        return Some(xb_get_activation_factory as usize);
    }
    if dll.eq_ignore_ascii_case("d3d11_x.dll") && func == "D3D11XCreateDeviceX" {
        return Some(gpu::create_device_x as usize);
    }
    // Titles that use the plain `D3D11CreateDevice` export of the console runtime (same signature as
    // the desktop one): forwarded to the real d3d11.dll, with the call and its result logged.
    if dll.eq_ignore_ascii_case("d3d11_x.dll") && func == "D3D11CreateDevice" {
        return Some(xb_d3d11_create_device as usize);
    }
    if dll.eq_ignore_ascii_case("d3d11_x.dll") && func == "DXGIXPresentArray" {
        return Some(gpu::present_array as usize);
    }
    if dll.eq_ignore_ascii_case("d3d11_x.dll") && func == "DXGIXGetFrameStatistics" {
        return Some(gpu::get_frame_statistics as usize);
    }
    if dll.eq_ignore_ascii_case("d3d11_x.dll") && func == "DXGIXSetVLineNotification" {
        return Some(gpu::set_vline_notification as usize);
    }
    if dll.eq_ignore_ascii_case("AcpHal.dll") {
        match func {
            "AcpHalCreate" => return Some(acp_create as usize),
            "ApuCreateHeap" => return Some(apu_create_heap as usize),
            "ApuAlloc" => return Some(apu_alloc as usize),
            "ApuFree" => return Some(apu_free as usize),
            "ApuIsVirtualAddressValid" => return Some(apu_is_va_valid as usize),
            "ApuMapVirtualAddress" => return Some(apu_map_va as usize),
            "AcpHalAllocateShapeContexts" => return Some(acp_allocate_shape_contexts as usize),
            "AcpHalReleaseShapeContexts" => return Some(acp_release_shape_contexts as usize),
            _ => {}
        }
    }
    // The console hands out zeroed memory from its heap and the title relies on it (Minecraft's player settings block is
    // read before it is filled: reused desktop heap memory gave a random controller mapping and a crash on the first
    // button press), so the C++ `operator new` of the older runtime returns zeroed blocks.
    if dll.to_ascii_lowercase().starts_with("msvcr110") {
        match func {
            "??2@YAPEAX_K@Z" => return Some(xb_operator_new as usize),
            "??_U@YAPEAX_K@Z" => return Some(xb_operator_new_array as usize),
            "malloc" => return Some(xb_malloc as usize),
            _ => {}
        }
    }
    if func == "CoCreateInstance" {
        return Some(xb_co_create_instance as usize);
    }
    if !dll.eq_ignore_ascii_case("kernelx.dll") && !dll.eq_ignore_ascii_case("kernel32.dll") {
        return None;
    }
    match func {
        "GetProcessHeap" => Some(xb_get_process_heap as usize),
        "GetProcAddress" => Some(xb_get_proc_address as usize),
        "LoadLibraryExW" => Some(xb_load_library_ex_w as usize),
        "LoadLibraryW" => Some(xb_load_library_w as usize),
        "ExitProcess" => Some(xb_exit_process as usize),
        "TerminateProcess" => Some(xb_terminate_process as usize),
        "HeapAlloc" => Some(xb_heap_alloc as usize),
        "HeapFree" => Some(xb_heap_free as usize),
        "VirtualAlloc" => Some(xb_virtual_alloc as usize),
        "XMemAlloc" | "XMemAllocDefault" => Some(xb_xmem_alloc as usize),
        "XMemFree" | "XMemFreeDefault" => Some(xb_xmem_free as usize),
        "XMemSetAllocationHooks" => Some(xb_xmem_set_hooks as usize),
        "TitleMemoryStatus" => Some(xb_title_memory_status as usize),
        "CreateThread" => Some(xb_create_thread as usize),
        "CreateThreadpoolIo" => Some(xb_create_threadpool_io as usize),
        "CreateEventW" if event_trace() => Some(xb_create_event_w as usize),
        "CreateEventA" if event_trace() => Some(xb_create_event_a as usize),
        "SetEvent" if event_trace() => Some(xb_set_event as usize),
        "CreateSemaphoreExW" if event_trace() => Some(xb_create_semaphore as usize),
        "ReleaseSemaphore" if event_trace() => Some(xb_release_semaphore as usize),
        "WaitForMultipleObjects" if event_trace() => Some(xb_wait_multiple as usize),
        "WaitForSingleObject" if event_trace() => Some(xb_wait_single as usize),
        "WaitForSingleObjectEx" if event_trace() => Some(xb_wait_single_ex as usize),
        "LoadLibraryExW" if RING.load(Ordering::Relaxed) != 0 || std::env::var_os("XB1_FILES").is_some() => Some(xb_load_library_ex_w as usize),
        "CreateFile2" if RING.load(Ordering::Relaxed) != 0 || std::env::var_os("XB1_FILES").is_some() => Some(xb_create_file2 as usize),
        "FindFirstFileExA" if RING.load(Ordering::Relaxed) != 0 || std::env::var_os("XB1_FILES").is_some() => Some(xb_find_first_file_ex_a as usize),
        "GetFileAttributesExA" if RING.load(Ordering::Relaxed) != 0 || std::env::var_os("XB1_FILES").is_some() => Some(xb_get_file_attributes_ex_a as usize),
        "CreateFileW" if RING.load(Ordering::Relaxed) != 0 || std::env::var_os("XB1_FILES").is_some() => Some(xb_create_file_w as usize),
        "GetFileAttributesW" if RING.load(Ordering::Relaxed) != 0 || std::env::var_os("XB1_FILES").is_some() => Some(xb_get_file_attributes_w as usize),
        "GetFileAttributesExW" if RING.load(Ordering::Relaxed) != 0 || std::env::var_os("XB1_FILES").is_some() => Some(xb_get_file_attributes_ex_w as usize),
        "FindFirstFileW" if RING.load(Ordering::Relaxed) != 0 || std::env::var_os("XB1_FILES").is_some() => Some(xb_find_first_file_w as usize),
        "ReadFile" if RING.load(Ordering::Relaxed) != 0 || std::env::var_os("XB1_FILES").is_some() => Some(xb_read_file as usize),
        "GetFileInformationByHandleEx" if RING.load(Ordering::Relaxed) != 0 || std::env::var_os("XB1_FILES").is_some() => Some(xb_get_file_info as usize),
        _ => None,
    }
}

static TRACE_HEAP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn real_kernel32(name: &std::ffi::CStr) -> usize {
    unsafe { GetProcAddress(LoadLibraryA(c"kernel32.dll".as_ptr()), name.as_ptr()) as usize }
}

extern "system" fn xb_heap_alloc(heap: *mut c_void, flags: u32, size: usize) -> *mut c_void {
    let real: extern "system" fn(*mut c_void, u32, usize) -> *mut c_void =
        unsafe { std::mem::transmute(real_kernel32(c"HeapAlloc")) };
    // A single request above the cap (XB1_MAX_ALLOC_MB, off by default) is refused: the test title asks
    // once for 8.8 GB (a garbage size, from an asset-loading thread), which exhausts the system commit
    // limit and makes later GPU allocations fail with E_OUTOFMEMORY.
    static CAP_MB: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let cap = *CAP_MB.get_or_init(|| std::env::var("XB1_MAX_ALLOC_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(0));
    if cap != 0 && size > (cap << 20) {
        rlog!("[mem] refused a HeapAlloc of {} MiB (cap {cap} MiB)", size >> 20);
        return ptr::null_mut();
    }
    let p = real(heap, flags, size);
    if TRACE_HEAP.load(Ordering::Relaxed) || (size >= (64 << 20) && std::env::var_os("XB1_BIG_ALLOCS").is_some()) {
        rlog!("HeapAlloc(heap={heap:p}, flags={flags:#x}, size={size}) -> {p:p}");
        if size >= (64 << 20) {
            let (base, isize) = (IMG_BASE.load(Ordering::Relaxed), IMG_SIZE.load(Ordering::Relaxed));
            let mut frames = [ptr::null_mut::<c_void>(); 24];
            let n = unsafe { RtlCaptureStackBackTrace(0, 24, frames.as_mut_ptr(), ptr::null_mut()) } as usize;
            for f in frames[..n].iter().map(|f| *f as usize).filter(|f| *f >= base && *f < base + isize).take(8) {
                rlog!("  big HeapAlloc caller title+{:#x}", f - base);
            }
            // The unwinder loses the chain here, so also scan the stack for return-address-like values.
            let sp = &frames as *const _ as usize;
            let mut shown = 0;
            for k in 0..1024usize {
                let v = unsafe { *((sp + k * 8) as *const usize) };
                if v >= base && v < base + isize && shown < 14 {
                    rlog!("  big HeapAlloc stack[{k}] -> title+{:#x}", v - base);
                    shown += 1;
                }
            }
        }
    }
    p
}

/// `XMemAlloc(size, attributes)`: the console's title-memory allocator. The attribute bits
/// (alignment, memory type) are not decoded yet, so every block is page-backed and aligned
/// to 64 KB, which satisfies any alignment a title is likely to ask for.
extern "system" fn xb_xmem_alloc(size: usize, attrs: u64) -> *mut c_void {
    const GRANULE: usize = 64 << 10;
    let rounded = (size.max(1) + GRANULE - 1) & !(GRANULE - 1);
    let p = unsafe { VirtualAlloc(ptr::null_mut(), rounded, MEM_COMMIT_RESERVE, 0x04) };
    if rounded >= (64 << 20) && std::env::var_os("XB1_BIG_ALLOCS").is_some() {
        rlog!("[mem] XMemAlloc {} MiB attrs={attrs:#x} -> {p:p}", rounded >> 20);
    }
    if !p.is_null() {
        COMMITTED_BYTES.fetch_add(rounded, Ordering::Relaxed);
        XMEM_BLOCKS.lock().unwrap().insert(p as usize);
    }
    ring_write(format_args!("  XMemAlloc(size={size:#x}, attrs={attrs:#x}) -> {p:p}\n"));
    p
}

/// `XMemFree(address, attributes)`; any further arguments a title passes are ignored.
extern "system" fn xb_xmem_free(p: *mut c_void, attrs: u64) {
    ring_write(format_args!("  XMemFree({p:p}, attrs={attrs:#x})\n"));
    if !p.is_null() && XMEM_BLOCKS.lock().unwrap().remove(&(p as usize)) {
        unsafe { VirtualFree(p, 0, 0x8000) }; // MEM_RELEASE
    }
}

/// `XMemSetAllocationHooks`: accepted and ignored for now (allocations are not redirected).
extern "system" fn xb_xmem_set_hooks(_alloc: usize, _free: usize) {
    ring_write(format_args!("  XMemSetAllocationHooks (ignored)\n"));
}

/// `TitleMemoryStatus(TITLEMEMORYSTATUS*)`. The struct layout is not verified: this zero-fills
/// up to the caller-supplied `dwLength` and reports a total/available pair at offsets 8/16,
/// which is a guess based on the console SDK's documented field order.
extern "system" fn xb_title_memory_status(p: *mut u8) -> i32 {
    if p.is_null() {
        return 0;
    }
    unsafe {
        let len = (*(p as *const u32) as usize).min(256);
        ptr::write_bytes(p.add(4), 0, len.saturating_sub(4));
        if len >= 24 {
            // XB1_TITLE_MEM_MB overrides the reported total (experiment: what sizes the title's pools).
            let total = std::env::var("XB1_TITLE_MEM_MB").ok().and_then(|v| v.parse::<u64>().ok()).map(|m| m << 20).unwrap_or(5 << 30);
            *(p.add(8) as *mut u64) = total; // total title memory (5 GiB by default)
            *(p.add(16) as *mut u64) = total / 10 * 9; // available: 90% of it
        }
    }
    ring_write(format_args!("  TitleMemoryStatus (layout guessed)\n"));
    1
}

/// Allocation-type bits desktop Windows accepts. Titles also pass console-only bits
/// (observed: 0x40000000 and 0x10000000) and large pages, which make VirtualAlloc fail
/// with ERROR_INVALID_PARAMETER, so those are stripped.
/// Blocks handed out by `XMemAlloc`, so a stray `HeapFree` on one can be routed correctly.
static XMEM_BLOCKS: Mutex<std::collections::BTreeSet<usize>> = Mutex::new(std::collections::BTreeSet::new());

/// Total bytes the title has committed through `VirtualAlloc`/`XMemAlloc`, for crash reports.
static COMMITTED_BYTES: AtomicUsize = AtomicUsize::new(0);

const VALID_ALLOC_TYPE: u32 = 0x1000 | 0x2000 | 0x80000 | 0x100000 | 0x200000 | 0x400000 | 0x100_0000;

/// Console VirtualAlloc: strips unsupported flags and keeps the title's fixed-address hint,
/// falling back to any address for a reservation if that range is not available here.
extern "system" fn xb_virtual_alloc(addr: *mut c_void, size: usize, ty: u32, prot: u32) -> *mut c_void {
    let clean = ty & VALID_ALLOC_TYPE;
    // Same for the protection: the title passes console-only bits (observed: 0x10000 on top of
    // PAGE_READWRITE), which fail with ERROR_NOT_SUPPORTED. Keep the PAGE_* value, PAGE_GUARD,
    // PAGE_NOCACHE and PAGE_WRITECOMBINE.
    let prot = prot & 0x7FF;
    let mut p = unsafe { VirtualAlloc(addr, size, clean, prot) };
    if !p.is_null() && clean & 0x1000 != 0 {
        let before = COMMITTED_BYTES.fetch_add(size, Ordering::Relaxed);
        // XB1_BIG_ALLOCS: report every 512 MiB crossed.
        if (before >> 29) != ((before + size) >> 29) && std::env::var_os("XB1_BIG_ALLOCS").is_some() {
            rlog!("[mem] the title has committed {} MiB (last commit {} KiB at {p:p})", (before + size) >> 20, size >> 10);
        }
    }
    if size >= (64 << 20) && std::env::var_os("XB1_BIG_ALLOCS").is_some() {
        rlog!("[mem] VirtualAlloc {} MiB type={clean:#x} protect={prot:#x} -> {p:p}", size >> 20);
    }
    let mut moved = false;
    if p.is_null() && !addr.is_null() && clean & 0x2000 != 0 {
        p = unsafe { VirtualAlloc(ptr::null_mut(), size, clean, prot) };
        moved = !p.is_null();
    }
    if TRACE_HEAP.load(Ordering::Relaxed) {
        let err = if p.is_null() { unsafe { GetLastError() } } else { 0 };
        let note = if moved { " (address hint unavailable, moved)" } else { "" };
        rlog!("VirtualAlloc(addr={addr:p}, size={size:#x}, type={ty:#x}->{clean:#x}, protect={prot:#x}) -> {p:p} err={err}{note}");
    }
    p
}

// File call tracing (ring mode only): passes straight through to Windows and records
// arguments and results, so a stuck file loop can be diagnosed from the ring.
extern "system" fn xb_create_file_w(name: *const u16, access: u32, share: u32, sec: *mut c_void, disp: u32, flags: u32, tmpl: *mut c_void) -> *mut c_void {
    let h = unsafe { CreateFileW(name, access, share, sec, disp, flags, tmpl) };
    let err = if h as isize == -1 { unsafe { GetLastError() } } else { 0 };
    let mut units = Vec::new();
    unsafe {
        let mut i = 0;
        while *name.add(i) != 0 && i < 260 {
            units.push(*name.add(i));
            i += 1;
        }
    }
    ring_write(format_args!("  CreateFileW({:?}, access={access:#x}, disp={disp}) -> {h:p} err={err}\n", String::from_utf16_lossy(&units)));
    h
}

/// C++/CX `GetActivationFactoryByPCWSTR(class_name, guid, out)`: looks up a WinRT class factory.
/// Passed through to the real vccorlib140; logs the class name and the HRESULT.
/// `operator new(size_t)` of MSVCR110 with the block zeroed (see `override_for`).
extern "system" fn xb_operator_new(size: usize) -> *mut c_void {
    zeroed_new(size, c"??2@YAPEAX_K@Z")
}

/// `operator new[](size_t)` of MSVCR110 with the block zeroed.
extern "system" fn xb_operator_new_array(size: usize) -> *mut c_void {
    zeroed_new(size, c"??_U@YAPEAX_K@Z")
}

/// `malloc` of MSVCR110 with the block zeroed.
extern "system" fn xb_malloc(size: usize) -> *mut c_void {
    zeroed_new(size, c"malloc")
}

fn zeroed_new(size: usize, name: &std::ffi::CStr) -> *mut c_void {
    static NEW: AtomicUsize = AtomicUsize::new(0);
    static NEW_ARRAY: AtomicUsize = AtomicUsize::new(0);
    static MALLOC: AtomicUsize = AtomicUsize::new(0);
    let slot = match name.to_bytes() {
        b"??2@YAPEAX_K@Z" => &NEW,
        b"malloc" => &MALLOC,
        _ => &NEW_ARRAY,
    };
    let mut f = slot.load(Ordering::Relaxed);
    if f == 0 {
        unsafe {
            let m = GetModuleHandleA(c"MSVCR110.dll".as_ptr());
            f = if m.is_null() { 0 } else { GetProcAddress(m, name.as_ptr()) as usize };
        }
        slot.store(f, Ordering::Relaxed);
    }
    if f == 0 {
        return ptr::null_mut();
    }
    let real: extern "system" fn(usize) -> *mut c_void = unsafe { std::mem::transmute(f) };
    let p = real(size);
    if !p.is_null() {
        unsafe { ptr::write_bytes(p as *mut u8, 0, size) };
    }
    p
}

extern "system" fn xb_get_activation_factory(name: *const u16, guid: *const u8, out: *mut *mut c_void) -> i32 {
    if !guid.is_null() && !out.is_null() {
        let class = wide_to_string(name);
        if let Some(hr) = winrt::try_activation_factory(&class, unsafe { &*(guid as *const windows::core::GUID) }, out) {
            return hr.0;
        }
    }
    let real: extern "system" fn(*const u16, *const u8, *mut *mut c_void) -> i32 = unsafe {
        let m = LoadLibraryA(if VCCORLIB110.load(Ordering::Relaxed) { c"vccorlib110.dll".as_ptr() } else { c"vccorlib140.dll".as_ptr() });
        std::mem::transmute(GetProcAddress(m, c"?GetActivationFactoryByPCWSTR@@YAJPEAXAEAVGuid@Platform@@PEAPEAX@Z".as_ptr()))
    };
    let hr = real(name, guid, out);
    let g = if guid.is_null() { [0u8; 16] } else { unsafe { *(guid as *const [u8; 16]) } };
    ring_write(format_args!(
        "  GetActivationFactoryByPCWSTR({:?}, iid={:02x?}) -> hr={:#x}\n",
        wide_to_string(name), g, hr as u32
    ));
    rlog!("[winrt] real activation factory for {} -> hr={:#x}", wide_to_string(name), hr as u32);
    hr
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryInformationThread(thread: *mut c_void, class: u32, info: *mut c_void, len: u32, ret: *mut u32) -> i32;
    fn GetCurrentThread() -> *mut c_void;
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoCreateInstance(clsid: *const windows::core::GUID, outer: *mut c_void, ctx: u32, iid: *const windows::core::GUID, out: *mut *mut c_void) -> i32;
}

/// `CoCreateInstance`: passed through to Windows, logging the CLSID, IID and result.
extern "system" fn xb_co_create_instance(clsid: *const windows::core::GUID, outer: *mut c_void, ctx: u32, iid: *const windows::core::GUID, out: *mut *mut c_void) -> i32 {
    let hr = unsafe { CoCreateInstance(clsid, outer, ctx, iid, out) };
    let (c, i) = unsafe { (*clsid, *iid) };
    if hr == 0 {
        winrt::wrap_co_created(&c, &i, out);
    }
    ring_write(format_args!("  CoCreateInstance(clsid={c:?}, ctx={ctx:#x}, iid={i:?}) -> hr={:#x}\n", hr as u32));
    rlog!("[com] CoCreateInstance(clsid {c:?}, iid {i:?}) -> {:#x}", hr as u32);
    hr
}

fn wide_to_string(p: *const u16) -> String {
    let mut units = Vec::new();
    unsafe {
        while !p.is_null() && *p.add(units.len()) != 0 && units.len() < 260 {
            units.push(*p.add(units.len()));
        }
    }
    String::from_utf16_lossy(&units)
}

fn ansi_to_string(p: *const i8) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

extern "system" fn xb_load_library_ex_w(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void {
    let s = wide_to_string(name);
    ring_write(format_args!("  LoadLibraryExW({s:?}, flags={flags:#x}) ...\n"));
    // Mirror stderr too, because a crash inside the load would lose the ring's result line.
    rlog!("[load] LoadLibraryExW({s:?}, flags={flags:#x})");
    let m = unsafe { LoadLibraryExW(name, file, flags) };
    ring_write(format_args!("  LoadLibraryExW({s:?}) -> {m:p}\n"));
    m
}

extern "system" fn xb_create_file2(name: *const u16, access: u32, share: u32, disp: u32, params: *mut c_void) -> *mut c_void {
    let h = unsafe { CreateFile2(name, access, share, disp, params) };
    let err = if h as isize == -1 { unsafe { GetLastError() } } else { 0 };
    let shown = wide_to_string(name);
    ring_write(format_args!("  CreateFile2({:?}, access={access:#x}, disp={disp}) -> {h:p} err={err}\n", shown));
    if shown.to_ascii_lowercase().contains(".slt") {
        // Who opens the game database: the call chain.
        let mut frames = [ptr::null_mut::<c_void>(); 40];
        let n = unsafe { RtlCaptureStackBackTrace(0, 40, frames.as_mut_ptr(), ptr::null_mut()) } as usize;
        for f in frames[..n].iter().map(|f| *f as usize).take(14) {
            let mut text = RawBuf::new();
            describe(f, &mut text);
            rlog!("  .slt opened from {}", text.as_str());
        }
    }
    h
}

extern "system" fn xb_find_first_file_ex_a(name: *const i8, level: u32, data: *mut c_void, op: u32, filter: *mut c_void, flags: u32) -> *mut c_void {
    let h = unsafe { FindFirstFileExA(name, level, data, op, filter, flags) };
    let err = if h as isize == -1 { unsafe { GetLastError() } } else { 0 };
    ring_write(format_args!("  FindFirstFileExA({:?}) -> {h:p} err={err}\n", ansi_to_string(name)));
    h
}

extern "system" fn xb_get_file_attributes_ex_a(name: *const i8, level: u32, data: *mut c_void) -> i32 {
    let ok = unsafe { GetFileAttributesExA(name, level, data) };
    let err = if ok == 0 { unsafe { GetLastError() } } else { 0 };
    ring_write(format_args!("  GetFileAttributesExA({:?}) -> ok={ok} err={err}\n", ansi_to_string(name)));
    ok
}

extern "system" fn xb_read_file(h: *mut c_void, buf: *mut c_void, n: u32, read: *mut u32, ov: *mut c_void) -> i32 {
    let ok = unsafe { ReadFile(h, buf, n, read, ov) };
    let got = if read.is_null() { 0 } else { unsafe { *read } };
    let err = if ok == 0 { unsafe { GetLastError() } } else { 0 };
    ring_write(format_args!("  ReadFile({h:p}, want={n}) -> ok={ok} got={got} err={err}\n"));
    ok
}

extern "system" fn xb_get_file_info(h: *mut c_void, class: u32, buf: *mut c_void, size: u32) -> i32 {
    let ok = unsafe { GetFileInformationByHandleEx(h, class, buf, size) };
    let err = if ok == 0 { unsafe { GetLastError() } } else { 0 };
    ring_write(format_args!("  GetFileInformationByHandleEx({h:p}, class={class}, size={size}) -> ok={ok} err={err}\n"));
    ok
}

extern "system" fn xb_heap_free(heap: *mut c_void, flags: u32, p: *mut c_void) -> i32 {
    static REAL: AtomicUsize = AtomicUsize::new(0);
    if REAL.load(Ordering::Relaxed) == 0 {
        REAL.store(real_kernel32(c"HeapFree"), Ordering::Relaxed);
    }
    let real: extern "system" fn(*mut c_void, u32, *mut c_void) -> i32 =
        unsafe { std::mem::transmute(REAL.load(Ordering::Relaxed)) };
    // A block from XMemAlloc is page-backed, not a heap block: handing it to the real
    // HeapFree makes ntdll look for a header 16 bytes before it.
    if XMEM_BLOCKS.lock().unwrap().remove(&(p as usize)) {
        if TRACE_HEAP.load(Ordering::Relaxed) {
            rlog!("HeapFree({p:p}) was an XMemAlloc block -> released with VirtualFree");
        }
        unsafe { VirtualFree(p, 0, 0x8000) };
        return 1;
    }
    let r = real(heap, flags, p);
    if TRACE_HEAP.load(Ordering::Relaxed) {
        rlog!("HeapFree(heap={heap:p}, p={p:p}) -> {r}");
    }
    r
}

/// Resolves the import table of a mapped image (the title or a guest DLL). Returns (resolved, stubbed).
unsafe fn resolve_imports(base: *mut u8, import_dir: (u32, u32), dir: &Path, arena: &mut StubArena, dump_iat: bool) -> (usize, usize) {
    let (mut real, mut stubbed) = (0usize, 0usize);
    if import_dir.0 != 0 {
        let mut desc = import_dir.0 as usize;
        loop {
            let (oft, name_rva, ft) = unsafe {
                (
                    *(base.add(desc) as *const u32),
                    *(base.add(desc + 12) as *const u32),
                    *(base.add(desc + 16) as *const u32),
                )
            };
            if name_rva == 0 {
                break;
            }
            let dll = unsafe { cstr_at(base, name_rva) };
            let module = load_dll(dir, &dll);
            let lookup = if oft != 0 { oft } else { ft } as usize;
            let mut i = 0;
            loop {
                let thunk = unsafe { *(base.add(lookup + i * 8) as *const u64) };
                if thunk == 0 {
                    break;
                }
                let (label, cname) = if thunk >> 63 == 1 {
                    (format!("{dll}!#{}", thunk & 0xFFFF), None)
                } else {
                    let n = unsafe { cstr_at(base, (thunk as u32) + 2) };
                    (format!("{dll}!{n}"), Some(CString::new(n).unwrap()))
                };
                let mut addr = cname
                    .as_ref()
                    .and_then(|c| override_for(&dll, c.to_str().unwrap_or("")))
                    .unwrap_or(0) as u64;
                if addr == 0 && !module.is_null() {
                    let p = match &cname {
                        Some(c) => unsafe { proc_address(module, c.as_ptr(), 0) },
                        None => unsafe { proc_address(module, ptr::null(), (thunk & 0xFFFF) as usize) },
                    };
                    addr = p as u64;
                }
                if addr == 0 {
                    addr = arena.make(label.clone());
                    stubbed += 1;
                } else {
                    real += 1;
                    if TRACE_API.load(Ordering::Relaxed) {
                        addr = arena.make_trace(label.clone(), addr);
                    }
                }
                unsafe { *(base.add(ft as usize + i * 8) as *mut u64) = addr };
                if dump_iat {
                    println!("iat {:#x} {label}", ft as usize + i * 8);
                }
                i += 1;
            }
            if module.is_null() {
                println!("  missing DLL (stubbed): {dll}");
            }
            desc += 20;
        }
    }
    (real, stubbed)
}


/// A console DLL shipped with the title, mapped by this loader (Windows cannot load it: it
/// imports console-only DLLs).
struct GuestDll {
    base: usize,
    size: usize,
    entry: u32,
    exc: (u32, u32),
    name: String,
}

static GUEST_DLLS: Mutex<Vec<GuestDll>> = Mutex::new(Vec::new());

/// `GetProcAddress` that also understands guest DLLs (a guest module handle is its base address).
fn proc_address(module: *mut c_void, name: *const i8, ordinal: usize) -> *mut c_void {
    let is_guest = GUEST_DLLS.lock().unwrap().iter().any(|g| g.base == module as usize);
    if !is_guest {
        let arg = if name.is_null() { ordinal as *const i8 } else { name };
        return unsafe { GetProcAddress(module, arg) };
    }
    unsafe { guest_export(module as *const u8, name, ordinal) as *mut c_void }
}

/// Looks up an export of a mapped image by name or ordinal. Forwarded exports are not followed.
unsafe fn guest_export(base: *const u8, name: *const i8, ordinal: usize) -> usize {
    let nt = *(base.add(0x3C) as *const u32) as usize;
    let dir_rva = *(base.add(nt + 24 + 112) as *const u32) as usize;
    let dir_size = *(base.add(nt + 24 + 112 + 4) as *const u32) as usize;
    if dir_rva == 0 {
        return 0;
    }
    let dir = base.add(dir_rva);
    let ord_base = *(dir.add(16) as *const u32) as usize;
    let n_funcs = *(dir.add(20) as *const u32) as usize;
    let n_names = *(dir.add(24) as *const u32) as usize;
    let funcs = base.add(*(dir.add(28) as *const u32) as usize) as *const u32;
    let names = base.add(*(dir.add(32) as *const u32) as usize) as *const u32;
    let ords = base.add(*(dir.add(36) as *const u32) as usize) as *const u16;
    let index = if name.is_null() {
        match ordinal.checked_sub(ord_base) {
            Some(i) if i < n_funcs => i,
            _ => return 0,
        }
    } else {
        let want = std::ffi::CStr::from_ptr(name).to_bytes();
        let mut found = None;
        for i in 0..n_names {
            let n = std::ffi::CStr::from_ptr(base.add(*names.add(i) as usize) as *const i8).to_bytes();
            if n == want {
                found = Some(*ords.add(i) as usize);
                break;
            }
        }
        match found {
            Some(i) if i < n_funcs => i,
            _ => return 0,
        }
    };
    let rva = *funcs.add(index) as usize;
    if rva >= dir_rva && rva < dir_rva + dir_size {
        return 0; // a forwarder
    }
    base as usize + rva
}

/// Maps a console DLL from the title folder with this loader: sections, relocations, imports.
/// Its unwind tables are registered and `DllMain` is called later, on the title thread (see
/// `init_guest_dlls`). Returns the base address, which serves as the module handle.
fn load_guest_dll(dir: &Path, name: &str) -> *mut c_void {
    if let Some(g) = GUEST_DLLS.lock().unwrap().iter().find(|g| g.name.eq_ignore_ascii_case(name)) {
        return g.base as *mut c_void;
    }
    let path = dir.join(name);
    let Ok(data) = std::fs::read(&path) else { return ptr::null_mut() };
    let pe = Pe(&data);
    let parse = || -> Result<(u32, u64, usize, usize, (u32, u32), (u32, u32), usize, usize, usize), String> {
        if pe.u16(0)? != 0x5A4D {
            return Err("not MZ".into());
        }
        let nt = pe.u32(0x3C)? as usize;
        if pe.u32(nt)? != 0x4550 || pe.u16(nt + 4)? != 0x8664 {
            return Err("not an x64 PE".into());
        }
        let nsec = pe.u16(nt + 6)? as usize;
        let opt_size = pe.u16(nt + 20)? as usize;
        let opt = nt + 24;
        Ok((
            pe.u32(opt + 16)?,
            pe.u64(opt + 24)?,
            pe.u32(opt + 56)? as usize,
            pe.u32(opt + 60)? as usize,
            (pe.u32(opt + 112 + 8)?, pe.u32(opt + 112 + 12)?),
            (pe.u32(opt + 112 + 5 * 8)?, pe.u32(opt + 112 + 5 * 8 + 4)?),
            nsec,
            opt_size,
            opt,
        ))
    };
    let Ok((entry, image_base, size_of_image, size_of_headers, import_dir, reloc_dir, nsec, opt_size, opt)) = parse() else {
        return ptr::null_mut();
    };
    let exc = (pe.u32(opt + 112 + 3 * 8).unwrap_or(0), pe.u32(opt + 112 + 3 * 8 + 4).unwrap_or(0));
    let mut base = unsafe { VirtualAlloc(image_base as *mut c_void, size_of_image, MEM_COMMIT_RESERVE, PAGE_RWX) };
    if base.is_null() {
        base = unsafe { VirtualAlloc(ptr::null_mut(), size_of_image, MEM_COMMIT_RESERVE, PAGE_RWX) };
    }
    if base.is_null() {
        return ptr::null_mut();
    }
    let base = base as *mut u8;
    unsafe {
        ptr::copy_nonoverlapping(data.as_ptr(), base, size_of_headers.min(data.len()));
        for i in 0..nsec {
            let s = opt + opt_size + i * 40;
            let (Ok(vsize), Ok(va), Ok(raw_size), Ok(raw_ptr)) = (pe.u32(s + 8), pe.u32(s + 12), pe.u32(s + 16), pe.u32(s + 20)) else { continue };
            let n = (raw_size as usize).min((vsize as usize).max(raw_size as usize));
            if let Some(src) = data.get(raw_ptr as usize..raw_ptr as usize + n) {
                ptr::copy_nonoverlapping(src.as_ptr(), base.add(va as usize), n);
            }
        }
        let delta = (base as u64).wrapping_sub(image_base);
        if delta != 0 && reloc_dir.0 != 0 {
            let mut off = reloc_dir.0 as usize;
            let end = off + reloc_dir.1 as usize;
            while off + 8 <= end {
                let page = *(base.add(off) as *const u32) as usize;
                let size = *(base.add(off + 4) as *const u32) as usize;
                if size < 8 {
                    break;
                }
                for e in 0..(size - 8) / 2 {
                    let entry = *(base.add(off + 8 + e * 2) as *const u16);
                    if entry >> 12 == 10 {
                        let p = base.add(page + (entry & 0xFFF) as usize) as *mut u64;
                        *p = (*p).wrapping_add(delta);
                    }
                }
                off += size;
            }
        }
    }
    // A DLL with a static TLS directory (the CRT's thread-safe statics use it) gets its own helper
    // DLL, so the Windows loader owns its block on every thread, and its `_tls_index` is pointed at
    // the index the loader assigned.
    unsafe {
        let tls_rva = pe.u32(opt + 112 + 9 * 8).unwrap_or(0);
        if tls_rva != 0 {
            static NEXT_OWNER: AtomicU32 = AtomicU32::new(1);
            let d = base.add(tls_rva as usize);
            let (start, end, index_ptr, zero) = (*(d as *const usize), *(d.add(8) as *const usize), *(d.add(16) as *const usize) as *mut u32, *(d.add(32) as *const u32) as usize);
            let template = std::slice::from_raw_parts(start as *const u8, end.saturating_sub(start));
            match tlsdll::load_helper_n(template, zero, NEXT_OWNER.fetch_add(1, Ordering::Relaxed)) {
                Ok(index) => {
                    *index_ptr = index;
                    eprintln!("[guest-dll] {name}: static TLS ({} bytes) owned by the loader at index {index}", template.len());
                }
                Err(e) => eprintln!("[guest-dll] {name}: static TLS not set up ({e})"),
            }
        }
    }
    // Registered before the imports are resolved so a cycle finds it.
    GUEST_DLLS.lock().unwrap().push(GuestDll { base: base as usize, size: size_of_image, entry, exc, name: name.to_string() });
    let mut arena = StubArena::new();
    let (real, stubbed) = unsafe { resolve_imports(base, import_dir, dir, &mut arena, false) };
    std::mem::forget(arena);
    eprintln!("[guest-dll] {name}: mapped at {:#x}, {real} imports resolved, {stubbed} stubbed", base as usize);
    base as *mut c_void
}

/// On the title thread, after bootstrap: unwind tables and `DllMain(DLL_PROCESS_ATTACH)` for each
/// guest DLL, dependencies first (they were pushed in load order, which is dependencies last, so
/// the list is walked in reverse).
unsafe fn init_guest_dlls() {
    let dlls: Vec<(usize, u32, (u32, u32), String)> =
        GUEST_DLLS.lock().unwrap().iter().rev().map(|g| (g.base, g.entry, g.exc, g.name.clone())).collect();
    for (base, entry, exc, name) in dlls {
        if exc.0 != 0 {
            RtlAddFunctionTable((base + exc.0 as usize) as *const c_void, exc.1 / 12, base as u64);
        }
        if entry != 0 {
            let dll_main: extern "system" fn(*mut c_void, u32, *mut c_void) -> i32 = std::mem::transmute(base + entry as usize);
            let ok = dll_main(base as *mut c_void, 1, ptr::null_mut());
            eprintln!("[guest-dll] {name}: DllMain(DLL_PROCESS_ATTACH) -> {ok}");
            if std::env::var_os("XB1_PROBE_GUEST").is_some() {
                let p = (base + 0x7cf70) as *const usize;
                eprintln!("[guest-dll] probe: [base+0x7cf70] = {:#x}, [+8] = {:#x}", unsafe { *p }, unsafe { *p.add(1) });
            }
        }
    }
}

/// Prefers a DLL shipped next to the title, then falls back to the system search path.
fn load_dll(dir: &Path, name: &str) -> *mut c_void {
    // kernelx.dll is the console's kernel API surface; nearly all of its
    // exports are standard Win32 names, so forward them to kernel32.
    // Console-only exports (XMem*, TitleMemoryStatus) are still not found
    // there and fall through to logging stubs.
    if name.eq_ignore_ascii_case("kernelx.dll") {
        return unsafe { LoadLibraryA(c"kernel32.dll".as_ptr()) };
    }
    let local = dir.join(name);
    if local.is_file() {
        if let Ok(c) = CString::new(local.to_string_lossy().as_bytes()) {
            let m = unsafe { LoadLibraryA(c.as_ptr()) };
            if !m.is_null() {
                return m;
            }
        }
    }
    if local.is_file() {
        let g = load_guest_dll(dir, name);
        if !g.is_null() {
            return g;
        }
    }
    let c = CString::new(name).unwrap();
    unsafe { LoadLibraryA(c.as_ptr()) }
}

/// Prints every stubbed import grouped by DLL.
fn print_import_report() {
    let names = STUB_NAMES.lock().unwrap();
    let mut by_dll: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for n in names.iter() {
        let (dll, func) = n.split_once('!').unwrap_or((n, ""));
        by_dll.entry(dll).or_default().push(func);
    }
    println!("\nstubbed imports by DLL:");
    for (dll, mut funcs) in by_dll {
        funcs.sort();
        println!("\n[{dll}] {} functions", funcs.len());
        for f in funcs {
            println!("  {f}");
        }
    }
}

fn load(data: &[u8], dir: &Path, run: bool, report: bool, dump_iat: bool) -> Result<(), String> {
    let pe = Pe(data);
    if pe.u16(0)? != 0x5A4D {
        return Err("not an MZ executable".into());
    }
    let nt = pe.u32(0x3C)? as usize;
    if pe.u32(nt)? != 0x0000_4550 {
        return Err("missing PE signature".into());
    }
    if pe.u16(nt + 4)? != 0x8664 {
        return Err("not an x64 image".into());
    }
    let nsec = pe.u16(nt + 6)? as usize;
    let opt_size = pe.u16(nt + 20)? as usize;
    let opt = nt + 24;
    if pe.u16(opt)? != 0x20B {
        return Err("not a PE32+ image".into());
    }
    let entry = pe.u32(opt + 16)?;
    let image_base = pe.u64(opt + 24)?;
    let size_of_image = pe.u32(opt + 56)? as usize;
    let size_of_headers = pe.u32(opt + 60)? as usize;
    let import_dir = (pe.u32(opt + 112 + 8)?, pe.u32(opt + 112 + 12)?);
    let reloc_dir = (pe.u32(opt + 112 + 5 * 8)?, pe.u32(opt + 112 + 5 * 8 + 4)?);
    let exc_dir = (pe.u32(opt + 112 + 3 * 8)?, pe.u32(opt + 112 + 3 * 8 + 4)?);
    let tls_dir = (pe.u32(opt + 112 + 9 * 8)?, pe.u32(opt + 112 + 9 * 8 + 4)?);

    let mut base = unsafe { VirtualAlloc(image_base as *mut c_void, size_of_image, MEM_COMMIT_RESERVE, PAGE_RWX) };
    if base.is_null() {
        base = unsafe { VirtualAlloc(ptr::null_mut(), size_of_image, MEM_COMMIT_RESERVE, PAGE_RWX) };
    }
    if base.is_null() {
        return Err("could not allocate image memory".into());
    }
    let base = base as *mut u8;
    println!("image base {image_base:#x} -> mapped at {:#x}, {} sections", base as usize, nsec);

    unsafe {
        ptr::copy_nonoverlapping(data.as_ptr(), base, size_of_headers.min(data.len()));
        for i in 0..nsec {
            let s = opt + opt_size + i * 40;
            let vsize = pe.u32(s + 8)? as usize;
            let va = pe.u32(s + 12)? as usize;
            let raw_size = pe.u32(s + 16)? as usize;
            let raw_ptr = pe.u32(s + 20)? as usize;
            let n = raw_size.min(vsize.max(raw_size));
            let src = data.get(raw_ptr..raw_ptr + n).ok_or("section out of file bounds")?;
            ptr::copy_nonoverlapping(src.as_ptr(), base.add(va), n);
        }
    }

    // Relocations
    let delta = (base as u64).wrapping_sub(image_base);
    if delta != 0 && reloc_dir.0 != 0 {
        let mut off = reloc_dir.0 as usize;
        let end = off + reloc_dir.1 as usize;
        while off + 8 <= end {
            let page = unsafe { *(base.add(off) as *const u32) } as usize;
            let size = unsafe { *(base.add(off + 4) as *const u32) } as usize;
            if size < 8 {
                break;
            }
            for e in 0..(size - 8) / 2 {
                let entry = unsafe { *(base.add(off + 8 + e * 2) as *const u16) };
                if entry >> 12 == 10 {
                    let p = unsafe { base.add(page + (entry & 0xFFF) as usize) } as *mut u64;
                    unsafe { *p = (*p).wrapping_add(delta) };
                }
            }
            off += size;
        }
        println!("applied relocations (delta {delta:#x})");
    }

    // Imports
    GUEST_HEAP.store(unsafe { HeapCreate(0, 0, 0) } as usize, Ordering::Relaxed);
    let mut arena = StubArena::new();
    let (real, stubbed) = unsafe { resolve_imports(base, import_dir, dir, &mut arena, dump_iat) };
    println!("imports: {real} resolved natively, {stubbed} stubbed");
    if report {
        print_import_report();
    }

    if run {
        // DLLs with static TLS that the title loads later (on its own threads) make ntdll rebuild every
        // thread's TLS vector, which crashed inside the loader; loaded now, before any title thread
        // exists, the later LoadLibrary only bumps a reference count.
        for name in [c"xaudio2_9.dll", c"x3daudio1_7.dll"] {
            let _ = unsafe { LoadLibraryA(name.as_ptr()) };
        }
        map_content_drive(dir);
        map_temp_drive();
        // XB1_AFFINITY=0x3: restrict the process to those CPUs (experiment: does a title's start-up race depend on how many
        // threads really run at once?).
        if let Some(mask) = std::env::var("XB1_AFFINITY").ok().and_then(|v| usize::from_str_radix(v.trim_start_matches("0x"), 16).ok()) {
            let ok = unsafe { SetProcessAffinityMask(GetCurrentProcess(), mask) };
            println!("process affinity mask {mask:#x}: {}", if ok != 0 { "set" } else { "failed" });
        }
        // XB1_TIMEOUT_SECS=n: end the process after n seconds (a watchdog for tests that might hang the GPU driver).
        if let Some(secs) = std::env::var("XB1_TIMEOUT_SECS").ok().and_then(|v| v.parse::<u64>().ok()) {
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(secs));
                rlog!("[watchdog] XB1_TIMEOUT_SECS={secs} reached: ending the process");
                if std::env::var_os("XB1_WATCHDOG_DUMP").is_some() {
                    dump_all_threads();
                }
                for h in WAIT_ALL_HANDLES.lock().unwrap().iter() {
                    let r = unsafe { WaitForSingleObject(*h as *mut c_void, 0) };
                    rlog!("[watchdog] wait-all handle {h:#x}: {}", match r { 0 => "signaled", 0x102 => "not signaled", _ => "invalid or abandoned" });
                }
                for (tid, h, caller) in PENDING_WAITS.lock().unwrap().iter() {
                    rlog!("[watchdog] thread {tid} waits forever on {h:#x} from {caller}");
                }
                // XB1_WATCHDOG_PEEK="0xADDR:len;0xADDR:len": memory at absolute addresses when the watchdog fires.
                if let Ok(list) = std::env::var("XB1_WATCHDOG_PEEK") {
                    for item in list.split(';') {
                        let Some((a, n)) = item.split_once(':') else { continue };
                        let a = a.trim();
                        // "*0xRVA+0xOFF": the pointer stored at title+RVA, plus OFF.
                        let deref = a.strip_prefix('*').and_then(|r| {
                            let (rva, off) = r.split_once('+')?;
                            let rva = usize::from_str_radix(rva.trim_start_matches("0x"), 16).ok()?;
                            let off = usize::from_str_radix(off.trim_start_matches("0x"), 16).ok()?;
                            let mut q = 0usize;
                            let mut g = 0usize;
                            if unsafe { ReadProcessMemory(GetCurrentProcess(), (IMG_BASE.load(Ordering::Relaxed) + rva) as *const c_void, &mut q as *mut usize as *mut c_void, 8, &mut g) } == 0 { return None; }
                            Some(q + off)
                        });
                        let addr = if deref.is_some() { deref } else if let Some(off) = a.strip_prefix("@+") { usize::from_str_radix(off.trim_start_matches("0x"), 16).ok().map(|o| LAST_HIT_RCX.load(Ordering::Relaxed) + o) } else { usize::from_str_radix(a.trim_start_matches("0x"), 16).ok() };
                        let (Some(a), Ok(n)) = (addr, n.trim().parse::<usize>()) else { continue };
                        let mut buf = [0u8; 256];
                        let mut got = 0usize;
                        let n = n.min(256);
                        if unsafe { ReadProcessMemory(GetCurrentProcess(), a as *const c_void, buf.as_mut_ptr() as *mut c_void, n, &mut got) } != 0 {
                            let hex: String = buf[..got].iter().map(|b| format!("{b:02x}")).collect();
                            rlog!("[watchdog] peek {a:#x}: {hex}");
                        } else {
                            rlog!("[watchdog] peek {a:#x}: unreadable");
                        }
                    }
                }
                // XB1_WATCH_HANDLES="0x2f4,0x2f8": whether each handle is signaled at this moment.
                if let Ok(list) = std::env::var("XB1_WATCH_HANDLES") {
                    for item in list.split(',') {
                        if let Ok(h) = usize::from_str_radix(item.trim().trim_start_matches("0x"), 16) {
                            let r = unsafe { WaitForSingleObject(h as *mut c_void, 0) };
                            rlog!("[watchdog] handle {h:#x}: {}", match r { 0 => "signaled", 0x102 => "not signaled", _ => "invalid or abandoned" });
                        }
                    }
                }
                unmap_content_drive();
                unmap_temp_drive();
                process::exit(0);
            });
        }
        // Titles expect a much larger main-thread stack than Rust's default, and the
        // TLS setup in `bootstrap` patches the calling thread's TEB, so do both on a
        // dedicated thread.
        let (base_addr, entry_addr) = (base as usize, base as usize + entry as usize);
        if tls_dir.0 != 0 && !LEGACY_TLS.load(Ordering::Relaxed) {
            match unsafe { prepare_ntdll_tls(base, tls_dir) } {
                Ok(index) => println!("helper TLS DLL loaded; the loader assigned TLS index {index}"),
                Err(e) => println!("helper TLS DLL failed ({e}); falling back to the hand-built TLS vector"),
            }
        }
        if let Some(list) = args_watch_sites() {
            unsafe { plant_watch_sites(base, &list) };
            println!("planted {} one-shot watchpoints", list.len());
        }
        if std::env::var_os("XB1_NO_COMPAT").is_none() {
            unsafe { patch_filesys_storage(base, size_of_image) };
            unsafe { patch_profile_copy(base, size_of_image) };
        }
        if std::env::var_os("XB1_PREMOUNT").is_some() {
            unsafe { patch_premount_zips(base, size_of_image) };
        }
        // XB1_PATCH="0x142cf6d=b201;0x...=..." (or --patch=...): write bytes into the mapped title
        // before it runs. For experiments and per-title compatibility fixes.
        if let Some(spec) = std::env::var_os("XB1_PATCH") {
            for item in spec.to_string_lossy().split(';') {
                let Some((rva, hex)) = item.split_once('=') else { continue };
                let Ok(rva) = usize::from_str_radix(rva.trim().trim_start_matches("0x"), 16) else { continue };
                let bytes: Vec<u8> = (0..hex.len() / 2).filter_map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()).collect();
                if rva + bytes.len() <= size_of_image {
                    unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), base.add(rva), bytes.len()) };
                    println!("patched {} byte(s) at title+{rva:#x}", bytes.len());
                }
            }
        }
        if PATCH_FASTFAIL.load(Ordering::Relaxed) {
            let n = unsafe { patch_fastfail_sites(base, size_of_image) };
            println!("patched {n} fast-fail sites with breakpoints (--patch-fastfail)");
        }
        let worker = std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(move || -> Result<u32, String> {
                TITLE_TID.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
                unsafe { bootstrap(base_addr as *mut u8, size_of_image, tls_dir, exc_dir)? };
                unsafe { init_guest_dlls() };
        if let Some(dir) = std::env::var_os("XB1_DUMP_CMD") {
            // Periodic snapshots of every context's live command buffer.
            std::thread::spawn(move || {
                for round in 1..=5 {
                    std::thread::sleep(std::time::Duration::from_secs(15));
                    gpu::dump_all_contexts(std::path::Path::new(&dir), &format!("t{}", round * 15));
                }
            });
        }
        if std::env::var_os("XB1_VTABLES").is_some() {
            gpu::log_vtable_identities();
        }
                println!("jumping to entry point {entry_addr:#x}");
                let f: extern "system" fn() -> u32 = unsafe { std::mem::transmute(entry_addr) };
                Ok(f())
            })
            .map_err(|e| e.to_string())?;
        let watch = WATCH_SECS.load(Ordering::Relaxed);
        if watch > 0 {
            eprintln!("watch: sleeping {watch}s in main");
            if HEAPCHECK.load(Ordering::Relaxed) {
                // Poll the process heap so the corrupting write is caught close to when it happens.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(watch as u64);
                while std::time::Instant::now() < deadline {
                    if unsafe { HeapValidate(GetProcessHeap(), 0, ptr::null()) } == 0 {
                        rlog!("heapcheck: process heap is corrupt");
                        dump_title_thread();
                        process::exit(1);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                rlog!("heapcheck: heap stayed valid for {watch}s");
            }
            std::thread::sleep(std::time::Duration::from_secs(watch as u64));
            eprintln!("watch: dumping title thread {}", TITLE_TID.load(Ordering::Relaxed));
            if std::env::var_os("XB1_ALL_THREADS").is_some() {
                dump_all_threads();
            } else {
                dump_title_thread();
            }
            process::exit(0);
        }
        let code = worker.join().map_err(|_| "title thread panicked".to_string())??;
        unmap_content_drive();
        unmap_temp_drive();
        println!("entry returned {code}");
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if let Some(dll) = args.iter().find_map(|a| a.strip_prefix("--test-tls-load=")) {
        // Diagnostic: does loading a DLL that has static TLS change this thread's TLS vector?
        let snapshot = || -> Vec<usize> {
            let teb: usize;
            unsafe { std::arch::asm!("mov {}, gs:[0x30]", out(reg) teb, options(nostack, readonly, preserves_flags)) };
            let vector = unsafe { *((teb + 0x58) as *const *const usize) };
            (0..48).map(|i| unsafe { *vector.add(i) }).collect()
        };
        let before = snapshot();
        let wide: Vec<u16> = dll.encode_utf16().chain(std::iter::once(0)).collect();
        let m = unsafe { LoadLibraryExW(wide.as_ptr(), ptr::null_mut(), 0) };
        let after = snapshot();
        let changed: Vec<usize> = (0..48).filter(|&i| before[i] != after[i]).collect();
        println!("LoadLibraryExW({dll}) -> {m:p}; TLS vector slots changed: {changed:?}");
        unsafe { tlsdll::report_loader_entry(m as usize) };
        // If the DLL exports `xb1_block`, report where that buffer sits inside the loader's block.
        let proc = unsafe { GetProcAddress(m, c"xb1_block".as_ptr()) };
        if !proc.is_null() {
            let f: extern "system" fn() -> *mut u8 = unsafe { std::mem::transmute(proc) };
            let buf = f() as usize;
            let teb: usize;
            unsafe { std::arch::asm!("mov {}, gs:[0x30]", out(reg) teb, options(nostack, readonly, preserves_flags)) };
            let vector = unsafe { *((teb + 0x58) as *const *const usize) };
            for &i in &changed {
                let block = unsafe { *vector.add(i) };
                if buf >= block && buf - block < 0x2000 {
                    println!("xb1_block() = {buf:#x}; TLS slot {i} block = {block:#x}; buffer offset inside the block = {:#x}", buf - block);
                }
            }
        }
        process::exit(0);
    }
    if args.iter().any(|a| a == "--test-d3d") {
        // Exercises the D3D11.X -> desktop D3D11 device creation without loading a title.
        let (mut dev, mut ctx) = (ptr::null_mut(), ptr::null_mut());
        let hr = gpu::create_device_x(ptr::null(), &mut dev, &mut ctx);
        println!("D3D11XCreateDeviceX -> hr={:#x} device={dev:p} context={ctx:p}", hr as u32);
        if hr != 0 || dev.is_null() || ctx.is_null() {
            process::exit(1);
        }
        // Exercise the proxies through ordinary D3D11 calls: each goes through a forwarding thunk.
        use windows::Win32::Graphics::Direct3D11::*;
        use windows::core::Interface;
        let device: ID3D11Device = unsafe { Interface::from_raw(dev) };
        let context: ID3D11DeviceContext = unsafe { Interface::from_raw(ctx) };
        println!("GetFeatureLevel via proxy -> {:#x}", unsafe { device.GetFeatureLevel().0 });
        let imm = unsafe { device.GetImmediateContext() }.ok();
        println!("GetImmediateContext returns the context proxy: {}", imm.as_ref().map(|c| c.as_raw() == ctx).unwrap_or(false));
        let desc = D3D11_BUFFER_DESC { ByteWidth: 256, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
        let mut buffer = None;
        let created = unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) };
        println!("CreateBuffer via proxy -> {:?}, buffer {}", created.is_ok(), buffer.is_some());
        unsafe { context.ClearState() };
        unsafe { context.Flush() };
        println!("ClearState and Flush via proxy: ok");
        let ctx_device = unsafe { context.GetDevice() };
        println!("context.GetDevice returns the device proxy: {}", ctx_device.map(|d| d.as_raw() == dev).unwrap_or(false));
        // A console-only slot on the context: must log and return 0, not read past the vtable.
        let vtbl = unsafe { *(ctx as *const *const usize) };
        let ext: extern "system" fn(*mut c_void, u32) -> usize = unsafe { std::mem::transmute(*vtbl.add(321)) };
        println!("extension slot 321 call returned {}", ext(ctx, 1));

        // Baseline for the readback technique: an ordinary buffer, no placement involved.
        {
            let data: Vec<u8> = (0..256u32).map(|i| i as u8).collect();
            let plain_desc = D3D11_BUFFER_DESC { ByteWidth: 256, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
            let init = D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr() as *const c_void, ..Default::default() };
            let mut plain = None;
            unsafe { device.CreateBuffer(&plain_desc, Some(&init), Some(&mut plain)) }.expect("plain buffer");
            let plain = plain.unwrap();
            let st_desc = D3D11_BUFFER_DESC { ByteWidth: 256, Usage: D3D11_USAGE_STAGING, CPUAccessFlags: 0x20000, ..Default::default() };
            let mut st = None;
            unsafe { device.CreateBuffer(&st_desc, None, Some(&mut st)) }.expect("staging");
            let st = st.unwrap();
            if std::env::var("XB_TEST_BARE_DRAW").is_ok() {
                unsafe { context.Draw(3, 0) };
                println!("baseline: issued a bare Draw with no pipeline bound");
            }
            unsafe { context.CopyResource(&st, &plain) };
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            match unsafe { context.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)) } {
                Ok(()) => {
                    let ok = unsafe { std::slice::from_raw_parts(m.pData as *const u8, 256) } == data.as_slice();
                    unsafe { context.Unmap(&st, 0) };
                    println!("baseline readback of an ordinary buffer matches: {ok}");
                }
                Err(e) => println!("baseline readback FAILED: {e}"),
            }
        }

        // Placement mirroring: a buffer placed in memory the "title" writes directly.
        let mut placement = vec![0u8; 256];
        let desc_words: [u32; 6] = [256, 0, 4, 0, 0, 0]; // ByteWidth, Usage=DEFAULT, BIND_CONSTANT_BUFFER
        let dev_vtbl = unsafe { *(dev as *const *const usize) };
        let create_placed: extern "system" fn(*mut c_void, *const u32, *mut u8, *mut *mut c_void) -> i32 = unsafe { std::mem::transmute(*dev_vtbl.add(62)) };
        let mut placed: *mut c_void = ptr::null_mut();
        let hr = create_placed(dev, desc_words.as_ptr(), placement.as_mut_ptr(), &mut placed);
        println!("placement buffer created: hr={hr:#x} buffer={placed:p}");
        // The title writes constants straight into the placement memory.
        for (i, b) in placement.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7).wrapping_add(3);
        }
        // Read the buffer back through a staging copy. The copy goes through the context's
        // embedded function table (console index 46 = desktop `CopyResource`, at object offset
        // 0x3E0 + 8 * 46), which must first upload the changed placement memory. (A bare Draw
        // with no pipeline bound would remove the device on this GPU, so no draw is issued.)
        let buffer: ID3D11Buffer = unsafe { Interface::from_raw(placed) };
        let staging_desc = D3D11_BUFFER_DESC { ByteWidth: 256, Usage: D3D11_USAGE_STAGING, CPUAccessFlags: 0x20000, ..Default::default() };
        let mut staging = None;
        unsafe { device.CreateBuffer(&staging_desc, None, Some(&mut staging)) }.expect("staging buffer");
        let staging = staging.unwrap();
        let embedded_copy: usize = unsafe { *((ctx as usize + 0x3E0 + 8 * 46) as *const usize) };
        let copy: extern "system" fn(*mut c_void, *mut c_void, *mut c_void) = unsafe { std::mem::transmute(embedded_copy) };
        copy(ctx, staging.as_raw(), buffer.as_raw());
        println!("device status after the embedded CopyResource: {:?}", unsafe { device.GetDeviceRemovedReason() });
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.expect("map staging");
        let back = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, 256) };
        let same = back == placement.as_slice();
        unsafe { context.Unmap(&staging, 0) };
        println!("placement mirror: buffer contents match the placement memory after a draw: {same}");
        process::exit(if same { 0 } else { 1 });
    }
    let Some(path) = args.iter().find(|a| !a.starts_with("--")) else {
        eprintln!("usage: xb1layer <file.exe> [--run] [--imports]");
        process::exit(2);
    };
    let data = fs::read(path).unwrap_or_else(|e| {
        eprintln!("cannot read {path}: {e}");
        process::exit(1);
    });
    let dir = Path::new(path).parent().unwrap_or(Path::new("."));
    let run = args.iter().any(|a| a == "--run");
    let report = args.iter().any(|a| a == "--imports");
    let dump_iat = args.iter().any(|a| a == "--iat");
    HEAPCHECK.store(args.iter().any(|a| a == "--heapcheck"), Ordering::Relaxed);
    let ring_path = args.iter().find_map(|a| a.strip_prefix("--ring="));
    if let Some(p) = ring_path {
        if let Err(e) = ring_init(p) {
            eprintln!("ring trace: {e}");
            process::exit(1);
        }
    }
    WATCH_REPEAT.store(args.iter().any(|a| a == "--watch-repeat"), Ordering::Relaxed);
    PATCH_FASTFAIL.store(args.iter().any(|a| a == "--patch-fastfail"), Ordering::Relaxed);
    LEGACY_TLS.store(args.iter().any(|a| a == "--legacy-tls"), Ordering::Relaxed);
    NO_PEB_PATCH.store(args.iter().any(|a| a == "--no-peb-patch"), Ordering::Relaxed);
    TRACE_WINDOW.store(args.iter().any(|a| a == "--trace-window"), Ordering::Relaxed);
    TRACE_API.store(ring_path.is_some() || args.iter().any(|a| a == "--trace-api"), Ordering::Relaxed);
    TRACE_HEAP.store(args.iter().any(|a| a == "--trace-heap"), Ordering::Relaxed);
    if let Some(n) = args.iter().find_map(|a| a.strip_prefix("--watch=")) {
        WATCH_SECS.store(n.parse().unwrap_or(5), Ordering::Relaxed);
    }
    if let Err(e) = load(&data, dir, run, report, dump_iat) {
        eprintln!("load failed: {e}");
        process::exit(1);
    }
}



extern "system" fn xb_get_file_attributes_w(name: *const u16) -> u32 {
    let r = unsafe { GetFileAttributesW(name) };
    let err = if r == u32::MAX { unsafe { GetLastError() } } else { 0 };
    ring_write(format_args!("  GetFileAttributesW({:?}) -> {r:#x} err={err}\n", wide_to_string(name)));
    r
}

extern "system" fn xb_get_file_attributes_ex_w(name: *const u16, level: u32, data: *mut c_void) -> i32 {
    let ok = unsafe { GetFileAttributesExW(name, level, data) };
    let err = if ok == 0 { unsafe { GetLastError() } } else { 0 };
    ring_write(format_args!("  GetFileAttributesExW({:?}) -> ok={ok} err={err}\n", wide_to_string(name)));
    ok
}

extern "system" fn xb_find_first_file_w(name: *const u16, data: *mut c_void) -> *mut c_void {
    let h = unsafe { FindFirstFileW(name, data) };
    let err = if h as isize == -1 { unsafe { GetLastError() } } else { 0 };
    ring_write(format_args!("  FindFirstFileW({:?}) -> {h:p} err={err}\n", wide_to_string(name)));
    h
}
