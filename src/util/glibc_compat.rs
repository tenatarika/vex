//! glibc 2.38 renamed the C23 `strtol` family to `__isoc23_*`, and ort's
//! prebuilt onnxruntime references those names. Linux release binaries link
//! against glibc 2.35 (ubuntu-22.04), which lacks them, so forward them to the
//! classic entry points. The only C23 difference is `0b` prefix parsing, which
//! onnxruntime doesn't rely on.

use std::ffi::{c_char, c_int, c_long, c_longlong, c_ulonglong};

extern "C" {
    fn strtol(nptr: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_long;
    fn strtoll(nptr: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_longlong;
    fn strtoull(nptr: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_ulonglong;
}

/// # Safety
/// Same contract as C `strtol`.
#[no_mangle]
pub unsafe extern "C" fn __isoc23_strtol(
    nptr: *const c_char,
    endptr: *mut *mut c_char,
    base: c_int,
) -> c_long {
    strtol(nptr, endptr, base)
}

/// # Safety
/// Same contract as C `strtoll`.
#[no_mangle]
pub unsafe extern "C" fn __isoc23_strtoll(
    nptr: *const c_char,
    endptr: *mut *mut c_char,
    base: c_int,
) -> c_longlong {
    strtoll(nptr, endptr, base)
}

/// # Safety
/// Same contract as C `strtoull`.
#[no_mangle]
pub unsafe extern "C" fn __isoc23_strtoull(
    nptr: *const c_char,
    endptr: *mut *mut c_char,
    base: c_int,
) -> c_ulonglong {
    strtoull(nptr, endptr, base)
}
