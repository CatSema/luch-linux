//! luch-kms: see src/kms.c. Kept as a separate, minimal binary so only it needs CAP_SYS_ADMIN.
use std::ffi::{c_char, c_int, CString};

extern "C" {
    fn luch_kms_main(argc: c_int, argv: *const *const c_char) -> c_int;
}

fn main() {
    let args: Vec<CString> = std::env::args().map(|a| CString::new(a).unwrap()).collect();
    let ptrs: Vec<*const c_char> = args.iter().map(|a| a.as_ptr()).collect();
    // SAFETY: argv-style array of valid C strings that outlives the call.
    std::process::exit(unsafe { luch_kms_main(ptrs.len() as c_int, ptrs.as_ptr()) });
}
