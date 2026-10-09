//! heddle: traductor ARM64 -> x86-64 con equivalencia funcional estricta del monitor
//! exclusivo, pensado como native bridge de Android.
pub mod cpu;
pub mod decode;
pub mod fp;
pub mod neon;
pub mod neonfp;
pub mod neonx;
pub mod neonh;
pub mod neonls;
pub mod crypto;
pub mod softfp;
pub mod interp;
pub mod monitor;
pub mod jit;
pub mod profg;
pub mod sys;
pub mod hostcall;
pub mod mem;
pub mod boundary;
pub mod guestlib;
pub mod sigs;
#[rustfmt::skip]
mod sigs_gen;
pub mod hle;
pub mod elf;
pub mod pagecompat;
pub mod namespace;
pub mod tls;
pub mod rt;
pub mod sig;
pub mod syscall;
pub mod fmt;
pub mod ldio;
pub mod cbthunk;
pub mod proxy;
pub mod ndkcb;
pub mod vk;
pub mod libc_hle;
pub mod dl;
pub mod jni;
pub mod bridge;
pub mod diag;
pub mod feat;
pub mod procemu;
#[cfg(test)]
mod xbench;
