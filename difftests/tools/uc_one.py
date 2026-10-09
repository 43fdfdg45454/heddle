#!/usr/bin/env python3
"""Ejecuta una palabra A64 en Unicorn: uc_one.py <hexword> <fpcr hex> vN=hi_lo ..."""
import sys, struct
from unicorn import *
from unicorn.arm64_const import *
w = int(sys.argv[1], 16)
fpcr = int(sys.argv[2], 16)
uc = Uc(UC_ARCH_ARM64, UC_MODE_ARM)
uc.ctl_set_cpu_model(UC_CPU_ARM64_MAX)
uc.reg_write(UC_ARM64_REG_CPACR_EL1, 0x300000)
B = 0x10000
uc.mem_map(B, 0x1000)
uc.mem_write(B, struct.pack('<I', w))
uc.reg_write(UC_ARM64_REG_FPCR, fpcr)
for a in sys.argv[3:]:
    k, v = a.split('=')
    hi, lo = v.split('_')
    n = int(k[1:])
    uc.reg_write(UC_ARM64_REG_Q0 + n, (int(hi, 16) << 64) | int(lo, 16))
uc.emu_start(B, B + 4)
print("fpsr=%#x" % uc.reg_read(UC_ARM64_REG_FPSR))
for a in sys.argv[3:]:
    n = int(a.split('=')[0][1:])
    print("v%d=%032x" % (n, uc.reg_read(UC_ARM64_REG_Q0 + n)))
for n in range(32):
    pass
rd = w & 31
print("Rd v%d=%032x" % (rd, uc.reg_read(UC_ARM64_REG_Q0 + rd)))
