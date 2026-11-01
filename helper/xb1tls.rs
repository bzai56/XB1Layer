//! Helper DLL that lets the Windows loader own the title's static TLS.
//!
//! This is compiled by `build.rs` with the ordinary toolchain (a hand-written PE image was not
//! accepted by the loader's TLS processing, a genuine linker output is) and embedded in
//! xb1layer. Its one thread-local is a 4 KB buffer; xb1layer patches the TLS directory so the
//! loader's per-thread block starts exactly at the buffer, then fills the template with the
//! title's own TLS template.

use std::cell::UnsafeCell;

struct Block(UnsafeCell<[u8; 4096]>);

thread_local! {
    static BLOCK: Block = const { Block(UnsafeCell::new([0u8; 4096])) };
}

/// Address of the calling thread's buffer. Used by xb1layer to verify the block layout.
#[no_mangle]
pub extern "C" fn xb1_block() -> *mut u8 {
    BLOCK.with(|b| b.0.get() as *mut u8)
}
