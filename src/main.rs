//! xb1layer: PE64 loader core for an Xbox One (GDK) compatibility layer.
//!
//! Maps an x64 PE image manually, applies relocations, resolves imports
//! against real Windows DLLs where possible, and routes everything else
//! (xgameruntime, GDK/XDK system libs) to logging stubs so we can see
//! which console APIs a title needs.
//!
//! Usage: xb1layer <file.exe> [--run]

mod winrt;

use std::collections::{BTreeMap, HashSet};
use std::ffi::{c_void, CString};
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::{env, fs, process, ptr};

unsafe extern "system" {
    fn VirtualAlloc(addr: *mut c_void, size: usize, ty: u32, prot: u32) -> *mut c_void;
    fn CreateFileA(name: *const i8, access: u32, share: u32, sec: *mut c_void, disp: u32, flags: u32, template: *mut c_void) -> *mut c_void;
    fn CreateFileMappingA(file: *mut c_void, sec: *mut c_void, prot: u32, hi: u32, lo: u32, name: *const i8) -> *mut c_void;
    fn MapViewOfFile(map: *mut c_void, access: u32, hi: u32, lo: u32, bytes: usize) -> *mut c_void;
    fn CreateFileW(name: *const u16, access: u32, share: u32, sec: *mut c_void, disp: u32, flags: u32, tmpl: *mut c_void) -> *mut c_void;
    fn CreateFile2(name: *const u16, access: u32, share: u32, disp: u32, params: *mut c_void) -> *mut c_void;
    fn FindFirstFileExA(name: *const i8, level: u32, data: *mut c_void, op: u32, filter: *mut c_void, flags: u32) -> *mut c_void;
    fn GetFileAttributesExA(name: *const i8, level: u32, data: *mut c_void) -> i32;
    fn ReadFile(h: *mut c_void, buf: *mut c_void, n: u32, read: *mut u32, ov: *mut c_void) -> i32;
    fn GetFileInformationByHandleEx(h: *mut c_void, class: u32, buf: *mut c_void, size: u32) -> i32;
    fn GetLastError() -> u32;
    fn CreateThread(attrs: *mut c_void, stack: usize, routine: usize, param: *mut c_void, flags: u32, tid: *mut u32) -> *mut c_void;
    fn VirtualFree(addr: *mut c_void, size: usize, ty: u32) -> i32;
    fn HeapCreate(options: u32, initial: usize, max: usize) -> *mut c_void;
    fn HeapValidate(heap: *mut c_void, flags: u32, mem: *const c_void) -> i32;
    fn GetProcessHeap() -> *mut c_void;
    fn VirtualQuery(addr: *const c_void, info: *mut c_void, len: usize) -> usize;
    fn GetStdHandle(which: u32) -> *mut c_void;
    fn WriteFile(h: *mut c_void, buf: *const u8, n: u32, written: *mut u32, overlapped: *mut c_void) -> i32;
    fn GetCurrentThreadId() -> u32;
    fn OpenThread(access: u32, inherit: i32, tid: u32) -> *mut c_void;
    fn SuspendThread(thread: *mut c_void) -> u32;
    fn ResumeThread(thread: *mut c_void) -> u32;
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
static SEEN_FAULTS: [AtomicUsize; 64] = [const { AtomicUsize::new(0) }; 64];
static SEEN_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Fixed-size formatting buffer so crash reporting never touches the heap
/// (the title may have crashed inside it, or corrupted it).
struct RawBuf {
    b: [u8; 1024],
    n: usize,
}

impl RawBuf {
    fn new() -> Self {
        Self { b: [0; 1024], n: 0 }
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
    unsafe {
        let h = OpenThread(0x1F_FFFF, 0, TITLE_TID.load(Ordering::Relaxed));
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
                if shown == 16 {
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
        let ctx = *info.add(1);
        let reg = |off: usize| *(ctx.add(off) as *const u64);
        rlog!(
            "  rax={:#x} rcx={:#x} rdx={:#x} rbx={:#x} rsi={:#x} rdi={:#x} r8={:#x} r9={:#x} rsp={:#x}",
            reg(0x78), reg(0x80), reg(0x88), reg(0x90), reg(0xA8), reg(0xB0), reg(0xB8), reg(0xC0), reg(0x98)
        );
        if code == 0xC000_0005 {
            let kind = *(rec.add(32) as *const usize);
            let target = *(rec.add(40) as *const usize);
            let kind = match kind { 0 => "read", 1 => "write", _ => "execute" };
            rlog!("  access violation: {kind} of {target:#x}");
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
            if v >= base + 8 && v < base + size && (*((v - 5) as *const u8) == 0xE8 || (*((v - 6) as *const u8) == 0xFF && *((v - 5) as *const u8) == 0x15)) {
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

const TLS_TITLE_INDEX: usize = 100;
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
    let arr = VirtualAlloc(ptr::null_mut(), SLOTS * 8, MEM_COMMIT_RESERVE, 0x04) as *mut *mut c_void;
    if !(*slot).is_null() {
        ptr::copy_nonoverlapping(*slot, arr, 16);
    }
    *arr.add(TLS_TITLE_INDEX) = block as *mut c_void;
    *slot = arr;
}

struct ThreadStart {
    routine: usize,
    param: usize,
}

extern "system" fn thread_trampoline(p: *mut c_void) -> u32 {
    let ThreadStart { routine, param } = *unsafe { Box::from_raw(p as *mut ThreadStart) };
    unsafe { install_tls_for_current_thread() };
    let f: extern "system" fn(*mut c_void) -> u32 = unsafe { std::mem::transmute(routine) };
    f(param as *mut c_void)
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
    let boxed = Box::into_raw(Box::new(ThreadStart { routine, param }));
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
    *((peb + 0x10) as *mut usize) = base as usize;

    if tls.0 != 0 {
        let d = base.add(tls.0 as usize);
        let start = *(d as *const u64) as usize;
        let end = *(d.add(8) as *const u64) as usize;
        let idx_ptr = *(d.add(16) as *const u64) as *mut u32;
        let callbacks = *(d.add(24) as *const u64) as *const usize;
        let zero_fill = *(d.add(32) as *const u32) as usize;
        let len = end - start;

        TLS_TEMPLATE.store(start, Ordering::Relaxed);
        TLS_LEN.store(len, Ordering::Relaxed);
        TLS_TOTAL.store(len + zero_fill, Ordering::Relaxed);
        *idx_ptr = TLS_TITLE_INDEX as u32;
        install_tls_for_current_thread();
        println!("static TLS: {len} bytes + {zero_fill} zero-fill at index {TLS_TITLE_INDEX}");

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

/// Imports we implement ourselves instead of forwarding to Windows.
fn override_for(dll: &str, func: &str) -> Option<usize> {
    if dll.eq_ignore_ascii_case("vccorlib140.dll") && func.starts_with("?GetActivationFactoryByPCWSTR") {
        return Some(xb_get_activation_factory as usize);
    }
    if func == "CoCreateInstance" {
        return Some(xb_co_create_instance as usize);
    }
    if !dll.eq_ignore_ascii_case("kernelx.dll") && !dll.eq_ignore_ascii_case("kernel32.dll") {
        return None;
    }
    match func {
        "GetProcessHeap" => Some(xb_get_process_heap as usize),
        "HeapAlloc" if TRACE_HEAP.load(Ordering::Relaxed) => Some(xb_heap_alloc as usize),
        "HeapFree" if TRACE_HEAP.load(Ordering::Relaxed) => Some(xb_heap_free as usize),
        "VirtualAlloc" => Some(xb_virtual_alloc as usize),
        "XMemAlloc" | "XMemAllocDefault" => Some(xb_xmem_alloc as usize),
        "XMemFree" | "XMemFreeDefault" => Some(xb_xmem_free as usize),
        "XMemSetAllocationHooks" => Some(xb_xmem_set_hooks as usize),
        "TitleMemoryStatus" => Some(xb_title_memory_status as usize),
        "CreateThread" => Some(xb_create_thread as usize),
        "CreateFile2" if RING.load(Ordering::Relaxed) != 0 => Some(xb_create_file2 as usize),
        "FindFirstFileExA" if RING.load(Ordering::Relaxed) != 0 => Some(xb_find_first_file_ex_a as usize),
        "GetFileAttributesExA" if RING.load(Ordering::Relaxed) != 0 => Some(xb_get_file_attributes_ex_a as usize),
        "CreateFileW" if RING.load(Ordering::Relaxed) != 0 => Some(xb_create_file_w as usize),
        "ReadFile" if RING.load(Ordering::Relaxed) != 0 => Some(xb_read_file as usize),
        "GetFileInformationByHandleEx" if RING.load(Ordering::Relaxed) != 0 => Some(xb_get_file_info as usize),
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
    let p = real(heap, flags, size);
    rlog!("HeapAlloc(heap={heap:p}, flags={flags:#x}, size={size}) -> {p:p}");
    p
}

/// `XMemAlloc(size, attributes)`: the console's title-memory allocator. The attribute bits
/// (alignment, memory type) are not decoded yet, so every block is page-backed and aligned
/// to 64 KB, which satisfies any alignment a title is likely to ask for.
extern "system" fn xb_xmem_alloc(size: usize, attrs: u64) -> *mut c_void {
    const GRANULE: usize = 64 << 10;
    let rounded = (size.max(1) + GRANULE - 1) & !(GRANULE - 1);
    let p = unsafe { VirtualAlloc(ptr::null_mut(), rounded, MEM_COMMIT_RESERVE, 0x04) };
    ring_write(format_args!("  XMemAlloc(size={size:#x}, attrs={attrs:#x}) -> {p:p}\n"));
    p
}

/// `XMemFree(address, attributes)`; any further arguments a title passes are ignored.
extern "system" fn xb_xmem_free(p: *mut c_void, attrs: u64) {
    ring_write(format_args!("  XMemFree({p:p}, attrs={attrs:#x})\n"));
    if !p.is_null() {
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
            *(p.add(8) as *mut u64) = 5 << 30; // total title memory: 5 GiB
            *(p.add(16) as *mut u64) = (9u64 << 30) / 2; // available: 4.5 GiB
        }
    }
    ring_write(format_args!("  TitleMemoryStatus (layout guessed)\n"));
    1
}

/// Allocation-type bits desktop Windows accepts. Titles also pass console-only bits
/// (observed: 0x40000000 and 0x10000000) and large pages, which make VirtualAlloc fail
/// with ERROR_INVALID_PARAMETER, so those are stripped.
const VALID_ALLOC_TYPE: u32 = 0x1000 | 0x2000 | 0x80000 | 0x100000 | 0x200000 | 0x400000 | 0x100_0000;

/// Console VirtualAlloc: strips unsupported flags and keeps the title's fixed-address hint,
/// falling back to any address for a reservation if that range is not available here.
extern "system" fn xb_virtual_alloc(addr: *mut c_void, size: usize, ty: u32, prot: u32) -> *mut c_void {
    let clean = ty & VALID_ALLOC_TYPE;
    let mut p = unsafe { VirtualAlloc(addr, size, clean, prot) };
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
extern "system" fn xb_get_activation_factory(name: *const u16, guid: *const u8, out: *mut *mut c_void) -> i32 {
    if !guid.is_null() && !out.is_null() {
        let class = wide_to_string(name);
        if let Some(hr) = winrt::try_activation_factory(&class, unsafe { &*(guid as *const windows::core::GUID) }, out) {
            return hr.0;
        }
    }
    let real: extern "system" fn(*const u16, *const u8, *mut *mut c_void) -> i32 = unsafe {
        let m = LoadLibraryA(c"vccorlib140.dll".as_ptr());
        std::mem::transmute(GetProcAddress(m, c"?GetActivationFactoryByPCWSTR@@YAJPEAXAEAVGuid@Platform@@PEAPEAX@Z".as_ptr()))
    };
    let hr = real(name, guid, out);
    let g = if guid.is_null() { [0u8; 16] } else { unsafe { *(guid as *const [u8; 16]) } };
    ring_write(format_args!(
        "  GetActivationFactoryByPCWSTR({:?}, iid={:02x?}) -> hr={:#x}\n",
        wide_to_string(name), g, hr as u32
    ));
    hr
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

extern "system" fn xb_create_file2(name: *const u16, access: u32, share: u32, disp: u32, params: *mut c_void) -> *mut c_void {
    let h = unsafe { CreateFile2(name, access, share, disp, params) };
    let err = if h as isize == -1 { unsafe { GetLastError() } } else { 0 };
    ring_write(format_args!("  CreateFile2({:?}, access={access:#x}, disp={disp}) -> {h:p} err={err}\n", wide_to_string(name)));
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
    let real: extern "system" fn(*mut c_void, u32, *mut c_void) -> i32 =
        unsafe { std::mem::transmute(real_kernel32(c"HeapFree")) };
    let r = real(heap, flags, p);
    rlog!("HeapFree(heap={heap:p}, p={p:p}) -> {r}");
    r
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
                        Some(c) => unsafe { GetProcAddress(module, c.as_ptr()) },
                        None => unsafe { GetProcAddress(module, (thunk & 0xFFFF) as usize as *const i8) },
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
    println!("imports: {real} resolved natively, {stubbed} stubbed");
    if report {
        print_import_report();
    }

    if run {
        // Titles expect a much larger main-thread stack than Rust's default, and the
        // TLS setup in `bootstrap` patches the calling thread's TEB, so do both on a
        // dedicated thread.
        let (base_addr, entry_addr) = (base as usize, base as usize + entry as usize);
        let worker = std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(move || -> Result<u32, String> {
                TITLE_TID.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
                unsafe { bootstrap(base_addr as *mut u8, size_of_image, tls_dir, exc_dir)? };
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
            dump_title_thread();
            process::exit(0);
        }
        let code = worker.join().map_err(|_| "title thread panicked".to_string())??;
        println!("entry returned {code}");
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
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
