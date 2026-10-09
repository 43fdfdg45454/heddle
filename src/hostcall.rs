//! Llamada a funciones del host (x86-64 SysV) con registros enteros, de coma flotante y pila.

#[repr(C)]
pub struct Call {
    pub f: u64,         // 0
    pub ints: [u64; 6], // 8
    pub fps: [u64; 8],  // 56
    pub nstack: u64,    // 120
    pub stack: u64,     // 128 (puntero a `nstack` palabras)
    pub out_rax: u64,   // 136
    pub out_xmm0: u64,  // 144
    pub out_rdx: u64,   // 152
}

impl Call {
    /// Solo se puede llamar a un `HostFn` (ver boundary.rs): no hay llamada al host desde un entero cualquiera.
    pub fn new(f: crate::boundary::HostFn) -> Call {
        Call { f: f.addr() as u64, ints: [0; 6], fps: [0; 8], nstack: 0, stack: 0, out_rax: 0, out_xmm0: 0, out_rdx: 0 }
    }
}

std::arch::global_asm!(
    ".globl heddle_hostcall",
    ".type heddle_hostcall,@function",
    "heddle_hostcall:",
    "push rbp",
    "mov rbp, rsp",
    "push rbx",
    "push r12",
    "mov rbx, rdi",
    "mov rcx, [rbx + 120]",
    "mov rsi, [rbx + 128]",
    "lea rax, [rcx*8]",
    "sub rsp, rax",
    "and rsp, -16",
    "xor rdx, rdx",
    "2:",
    "cmp rdx, rcx",
    "jae 3f",
    "mov rax, [rsi + rdx*8]",
    "mov [rsp + rdx*8], rax",
    "inc rdx",
    "jmp 2b",
    "3:",
    "movq xmm0, [rbx + 56]",
    "movq xmm1, [rbx + 64]",
    "movq xmm2, [rbx + 72]",
    "movq xmm3, [rbx + 80]",
    "movq xmm4, [rbx + 88]",
    "movq xmm5, [rbx + 96]",
    "movq xmm6, [rbx + 104]",
    "movq xmm7, [rbx + 112]",
    "mov rdi, [rbx + 8]",
    "mov rsi, [rbx + 16]",
    "mov rdx, [rbx + 24]",
    "mov rcx, [rbx + 32]",
    "mov r8,  [rbx + 40]",
    "mov r9,  [rbx + 48]",
    "mov eax, 8",
    "mov r11, [rbx]",
    "call r11",
    "mov [rbx + 136], rax",
    "movq [rbx + 144], xmm0",
    "mov [rbx + 152], rdx",
    "lea rsp, [rbp - 16]",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
    ".size heddle_hostcall, .-heddle_hostcall",
);

extern "C" {
    fn heddle_hostcall(c: *mut Call);
}

pub unsafe fn call(c: &mut Call) {
    heddle_hostcall(c as *mut Call);
}
