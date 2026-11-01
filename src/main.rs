//! xb1layer: PE64 loader core for an Xbox One (GDK) compatibility layer.
//!
//! Maps an x64 PE image manually, applies relocations, resolves imports
//! against real Windows DLLs where possible, and routes everything else
//! (xgameruntime, GDK/XDK system libs) to logging stubs so we can see
//! which console APIs a title needs.
//!
//! Usage: xb1layer <file.exe> [--run]

use std::collections::HashSet;
use std::ffi::{c_void, CString};
use std::sync::Mutex;
use std::{env, fs, process, ptr};

unsafe extern "system" {
    fn VirtualAlloc(addr: *mut c_void, size: usize, ty: u32, prot: u32) -> *mut c_void;
    fn LoadLibraryA(name: *const i8) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const i8) -> *mut c_void;
}

const MEM_COMMIT_RESERVE: u32 = 0x3000;
const PAGE_RWX: u32 = 0x40;

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
    const SLOT: usize = 32;
    const CAP: usize = 4096 * 16;

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
}

fn load(data: &[u8], run: bool) -> Result<(), String> {
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
            let cdll = CString::new(dll.clone()).unwrap();
            let module = unsafe { LoadLibraryA(cdll.as_ptr()) };
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
                let mut addr = 0u64;
                if !module.is_null() {
                    let p = match &cname {
                        Some(c) => unsafe { GetProcAddress(module, c.as_ptr()) },
                        None => unsafe { GetProcAddress(module, (thunk & 0xFFFF) as usize as *const i8) },
                    };
                    addr = p as u64;
                }
                if addr == 0 {
                    addr = arena.make(label);
                    stubbed += 1;
                } else {
                    real += 1;
                }
                unsafe { *(base.add(ft as usize + i * 8) as *mut u64) = addr };
                i += 1;
            }
            if module.is_null() {
                println!("  missing DLL (stubbed): {dll}");
            }
            desc += 20;
        }
    }
    println!("imports: {real} resolved natively, {stubbed} stubbed");

    if run {
        println!("jumping to entry point {:#x}", base as usize + entry as usize);
        let f: extern "system" fn() -> u32 = unsafe { std::mem::transmute(base.add(entry as usize)) };
        let code = f();
        println!("entry returned {code}");
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let Some(path) = args.get(1) else {
        eprintln!("usage: xb1layer <file.exe> [--run]");
        process::exit(2);
    };
    let data = fs::read(path).unwrap_or_else(|e| {
        eprintln!("cannot read {path}: {e}");
        process::exit(1);
    });
    if let Err(e) = load(&data, args.iter().any(|a| a == "--run")) {
        eprintln!("load failed: {e}");
        process::exit(1);
    }
}
