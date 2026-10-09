//! Declaraciones minimas de libc (sin dependencias externas).
#![allow(non_camel_case_types, dead_code)]
use std::os::raw::{c_char, c_int, c_long, c_void};

pub type size_t = usize;

#[repr(C)]
pub struct DlInfo {
    pub fname: *const c_char,
    pub fbase: *mut c_void,
    pub sname: *const c_char,
    pub saddr: *mut c_void,
}
pub type off_t = i64;

extern "C" {
    pub fn mmap(addr: *mut c_void, len: size_t, prot: c_int, flags: c_int, fd: c_int, off: off_t) -> *mut c_void;
    pub fn munmap(addr: *mut c_void, len: size_t) -> c_int;
    pub fn mprotect(addr: *mut c_void, len: size_t, prot: c_int) -> c_int;
    pub fn madvise(addr: *mut c_void, len: size_t, advice: c_int) -> c_int;
    pub fn dl_iterate_phdr(cb: extern "C" fn(*mut c_void, size_t, *mut c_void) -> c_int, data: *mut c_void) -> c_int;
    pub fn kill(pid: c_int, sig: c_int) -> c_int;
    pub fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    pub fn dlopen(name: *const c_char, flags: c_int) -> *mut c_void;
    pub fn dlerror() -> *const c_char;
    pub fn dladdr(addr: *const c_void, info: *mut DlInfo) -> c_int;
    pub fn open(path: *const c_char, flags: c_int, ...) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn read(fd: c_int, buf: *mut c_void, n: size_t) -> isize;
    pub fn pread(fd: c_int, buf: *mut c_void, n: size_t, off: off_t) -> isize;
    pub fn syscall(n: c_long, ...) -> c_long;
    // bionic (Android) llama a esta funcion `__errno`; glibc, `__errno_location`.
    #[cfg_attr(target_os = "android", link_name = "__errno")]
    pub fn __errno_location() -> *mut c_int;
    pub fn getpid() -> c_int;
    pub fn sysconf(n: c_int) -> c_long;
    pub fn snprintf(buf: *mut c_char, n: size_t, fmt: *const c_char, ...) -> c_int;
    pub fn strlen(s: *const c_char) -> size_t;
    pub fn abort() -> !;
    pub fn getauxval(t: u64) -> u64;
    pub fn getrandom(buf: *mut c_void, n: size_t, flags: u32) -> isize;
    pub fn mincore(addr: *mut c_void, len: size_t, vec: *mut u8) -> c_int;
    pub fn fork() -> c_int;
    pub fn _exit(c: c_int) -> !;
    pub fn exit(c: c_int) -> !;
    pub fn raise(sig: c_int) -> c_int;
    pub fn write(fd: c_int, buf: *const c_void, n: size_t) -> isize;
    pub fn pthread_create(t: *mut u64, attr: *const c_void, f: extern "C" fn(*mut c_void) -> *mut c_void, arg: *mut c_void) -> c_int;
    pub fn pthread_attr_init(a: *mut c_void) -> c_int;
    pub fn pthread_attr_setstacksize(a: *mut c_void, n: size_t) -> c_int;
    pub fn pthread_attr_setdetachstate(a: *mut c_void, s: c_int) -> c_int;
    pub fn pthread_attr_getdetachstate(a: *const c_void, s: *mut c_int) -> c_int;
    pub fn pthread_attr_getstacksize(a: *const c_void, n: *mut size_t) -> c_int;
    pub fn pthread_attr_destroy(a: *mut c_void) -> c_int;
    pub fn pthread_exit(r: *mut c_void) -> !;
    pub fn pthread_self() -> u64;
    pub fn pthread_getattr_np(t: u64, a: *mut c_void) -> c_int;
    pub fn pthread_attr_setstack(a: *mut c_void, base: *mut c_void, size: size_t) -> c_int;
    pub fn pthread_getspecific(k: u32) -> *mut c_void;
    pub fn pthread_setspecific(k: u32, v: *const c_void) -> c_int;
    pub fn pthread_key_create(k: *mut u32, d: *const c_void) -> c_int;
    pub fn pthread_key_delete(k: u32) -> c_int;
}

pub const PROT_NONE: c_int = 0;
pub const PROT_READ: c_int = 1;
pub const PROT_WRITE: c_int = 2;
pub const PROT_EXEC: c_int = 4;
pub const MAP_PRIVATE: c_int = 2;
pub const MAP_FIXED: c_int = 0x10;
pub const MAP_ANONYMOUS: c_int = 0x20;
pub const MAP_NORESERVE: c_int = 0x4000;
pub const MAP_FAILED: *mut c_void = !0usize as *mut c_void;
pub const RTLD_DEFAULT: *mut c_void = std::ptr::null_mut();
pub const RTLD_NOW: c_int = 2;

pub fn errno() -> i32 {
    unsafe { *__errno_location() }
}
