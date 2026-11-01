//! A helper DLL that gives the title's static TLS to the Windows loader.
//!
//! The title is mapped by hand, so `ntdll` knows nothing about its `.tls` section. Installing
//! the title's block into `TEB.ThreadLocalStoragePointer` by hand does not survive: when any DLL
//! with static TLS loads, `ntdll` rebuilds every thread's vector and drops entries it does not
//! know about. A real DLL with a TLS directory fixes this: the loader allocates its block for
//! every thread (existing and future), grows and copies it correctly, and tells us the index it
//! assigned. The title's `_tls_index` is then pointed at that index.
//!
//! The helper is built by `build.rs` from `helper/xb1tls.rs` (a hand-assembled PE image was not
//! accepted by the loader's TLS processing; genuine linker output is). Its only thread-local is
//! a 4 KB buffer, which the linker places 8 bytes after the start of the TLS template (behind
//! the CRT's `_tls_start` marker). The title addresses its TLS data from the start of its
//! block, so before writing the DLL out, its TLS directory's `StartAddressOfRawData` is moved
//! forward by that 8 bytes: the loader's per-thread block then begins exactly at the buffer.
//! After loading, the layout is verified, and the title's template is copied into the DLL's
//! template so every new thread starts with it.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

/// The helper DLL, built by build.rs.
static HELPER_DLL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/xb1tls.dll"));

/// Bytes between the start of the linker's TLS template and the helper's buffer (the CRT's
/// `_tls_start` marker, padded). Checked again at runtime.
const PRELUDE: u64 = 8;

/// Size of the helper's buffer, which bounds the title's TLS (template plus zero-fill).
const BUFFER_SIZE: usize = 4096;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryW(name: *const u16) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const i8) -> *mut c_void;
    fn GetLastError() -> u32;
    fn VirtualProtect(addr: *mut c_void, size: usize, new: u32, old: *mut u32) -> i32;
    fn GetModuleHandleW(name: *const u16) -> *mut c_void;
}

fn read_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
fn read_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// Maps an RVA in a PE file to its file offset using the section table.
fn rva_to_offset(b: &[u8], rva: u32) -> Option<usize> {
    let nt = read_u32(b, 0x3C) as usize;
    let sections = read_u16(b, nt + 6) as usize;
    let opt_size = read_u16(b, nt + 20) as usize;
    let table = nt + 24 + opt_size;
    for i in 0..sections {
        let s = table + i * 40;
        let (vsize, va, raw_size, raw_ptr) = (read_u32(b, s + 8), read_u32(b, s + 12), read_u32(b, s + 16), read_u32(b, s + 20));
        if rva >= va && rva < va + vsize.max(raw_size) {
            return Some(raw_ptr as usize + (rva - va) as usize);
        }
    }
    None
}

/// Returns the helper DLL with its TLS template start moved forward by `PRELUDE`.
fn patched_helper() -> Result<Vec<u8>, String> {
    let mut b = HELPER_DLL.to_vec();
    let nt = read_u32(&b, 0x3C) as usize;
    let tls_rva = read_u32(&b, nt + 24 + 112 + 9 * 8);
    if tls_rva == 0 {
        return Err("helper DLL has no TLS directory".into());
    }
    let off = rva_to_offset(&b, tls_rva).ok_or("TLS directory is outside every section")?;
    let start = u64::from_le_bytes(b[off..off + 8].try_into().unwrap());
    let end = u64::from_le_bytes(b[off + 8..off + 16].try_into().unwrap());
    if end - start < PRELUDE + BUFFER_SIZE as u64 {
        return Err(format!("helper template is {} bytes, expected at least {}", end - start, PRELUDE + BUFFER_SIZE as u64));
    }
    b[off..off + 8].copy_from_slice(&(start + PRELUDE).to_le_bytes());
    Ok(b)
}

/// Writes and loads the helper DLL, fills its TLS template with the title's, and returns the
/// TLS index the loader assigned to it.
///
/// Must run before any thread that executes title code is created: threads created later get
/// their block initialised from the template, and threads that already exist only got a copy of
/// the zeroed template at load time.
///
/// # Safety
/// `title_template` must be the title's TLS template bytes.
pub unsafe fn load_helper(title_template: &[u8], zero_fill: usize) -> Result<u32, String> {
    if title_template.len() + zero_fill > BUFFER_SIZE {
        return Err(format!("title TLS is {} bytes, larger than the helper's {BUFFER_SIZE}", title_template.len() + zero_fill));
    }
    let dir = std::env::temp_dir();
    remove_stale(&dir);
    let path = dir.join(format!("xb1layer_tls_{}.dll", std::process::id()));
    std::fs::write(&path, patched_helper()?).map_err(|e| format!("cannot write {}: {e}", path.display()))?;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let module = unsafe { LoadLibraryW(wide.as_ptr()) };
    if module.is_null() {
        return Err(format!("LoadLibraryW({}) failed with error {}", path.display(), unsafe { GetLastError() }));
    }
    let base = module as usize;

    // The loaded module's own TLS directory (absolute addresses are already relocated).
    let nt = base + unsafe { *((base + 0x3C) as *const u32) } as usize;
    let tls_rva = unsafe { *((nt + 24 + 112 + 9 * 8) as *const u32) } as usize;
    let tls = base + tls_rva;
    let (start, end, index_ptr) = unsafe {
        (
            *(tls as *const usize),
            *((tls + 8) as *const usize),
            *((tls + 16) as *const usize) as *const u32,
        )
    };
    let index = unsafe { *index_ptr };

    // Verify the loader really allocated a block for this module, and that the block starts at
    // the helper's buffer (offset 0), which is what the title assumes.
    let host_index = unsafe { host_tls_index() };
    if Some(index) == host_index {
        return Err(format!("the loader did not assign the helper its own TLS index (got {index}, the host's)"));
    }
    let get_block = unsafe { GetProcAddress(module, c"xb1_block".as_ptr()) };
    if get_block.is_null() {
        return Err("helper DLL does not export xb1_block".into());
    }
    let get_block: extern "system" fn() -> *mut u8 = unsafe { std::mem::transmute(get_block) };
    let teb: usize;
    unsafe { std::arch::asm!("mov {}, gs:[0x30]", out(reg) teb, options(nostack, readonly, preserves_flags)) };
    let vector = unsafe { *((teb + 0x58) as *const *const usize) };
    let block = unsafe { *vector.add(index as usize) };
    let buffer = get_block() as usize;
    // The helper's own code still reaches its buffer at the original offset (PRELUDE) because
    // that offset is baked into its compiled code; the patched template start only changes what
    // the loader copies into the block, so the block's first bytes are the template we fill.
    if block == 0 || buffer != block + PRELUDE as usize {
        return Err(format!("helper block layout is not as expected: block {block:#x}, buffer {buffer:#x}"));
    }
    if end - start < BUFFER_SIZE {
        return Err(format!("helper template is only {} bytes", end - start));
    }

    // Fill the template so every thread created from now on starts with the title's data.
    let template = start as *mut u8;
    let mut old = 0u32;
    unsafe {
        VirtualProtect(template as *mut c_void, end - start, 0x04, &mut old); // PAGE_READWRITE
        ptr::copy_nonoverlapping(title_template.as_ptr(), template, title_template.len());
    }
    Ok(index)
}

/// Prints what the loader recorded for the module: its `TlsIndex` (0xFFFF means it processed a
/// TLS directory, 0 means it did not) and flags. Walks `PEB.Ldr.InLoadOrderModuleList`.
pub unsafe fn report_loader_entry(base: usize) {
    let peb: usize;
    unsafe { std::arch::asm!("mov {}, gs:[0x60]", out(reg) peb, options(nostack, readonly, preserves_flags)) };
    let ldr = unsafe { *((peb + 0x18) as *const usize) };
    let head = ldr + 0x10;
    let mut entry = unsafe { *(head as *const usize) };
    while entry != head {
        let dll_base = unsafe { *((entry + 0x30) as *const usize) };
        if dll_base == base {
            let flags = unsafe { *((entry + 0x68) as *const u32) };
            let tls_index = unsafe { *((entry + 0x6E) as *const u16) };
            println!("loader entry for module {base:#x}: TlsIndex={tls_index:#x} flags={flags:#x}");
            return;
        }
        entry = unsafe { *(entry as *const usize) };
    }
    println!("loader entry for the module not found at {base:#x}");
}

/// The TLS index of the host executable, read from its own TLS directory. Must be called
/// before `PEB.ImageBaseAddress` is pointed at the title.
pub unsafe fn host_tls_index() -> Option<u32> {
    let base = unsafe { GetModuleHandleW(ptr::null()) } as usize;
    let nt = base + unsafe { *((base + 0x3C) as *const u32) } as usize;
    let tls_rva = unsafe { *((nt + 24 + 112 + 9 * 8) as *const u32) } as usize;
    if tls_rva == 0 {
        return None;
    }
    // IMAGE_TLS_DIRECTORY64.AddressOfIndex is an absolute address, already relocated.
    let index_ptr = unsafe { *((base + tls_rva + 16) as *const usize) } as *const u32;
    Some(unsafe { *index_ptr })
}

/// Deletes helper DLLs left behind by earlier runs (they cannot be deleted while loaded).
fn remove_stale(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("xb1layer_tls_") && name.ends_with(".dll") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}
