/* Partes derivadas de bionic/strtold.cpp, bionic/stdlib_l.cpp y bionic/wcstod.cpp: */
/*
 * Copyright (C) 2014 The Android Open Source Project
 * All rights reserved.
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions
 * are met:
 *  * Redistributions of source code must retain the above copyright
 *    notice, this list of conditions and the following disclaimer.
 *  * Redistributions in binary form must reproduce the above copyright
 *    notice, this list of conditions and the following disclaimer in
 *    the documentation and/or other materials provided with the
 *    distribution.
 *
 * THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
 * "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
 * LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS
 * FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE
 * COPYRIGHT OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT,
 * INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING,
 * BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS
 * OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED
 * AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
 * OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT
 * OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
 * SUCH DAMAGE.
 */

/* Parte derivada de stdio/stdio.cpp de bionic (vsnprintf): */
/*-
 * Copyright (c) 1990, 1993
 *	The Regents of the University of California.  All rights reserved.
 *
 * This code is derived from software contributed to Berkeley by
 * Chris Torek.
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions
 * are met:
 * 1. Redistributions of source code must retain the above copyright
 *    notice, this list of conditions and the following disclaimer.
 * 2. Redistributions in binary form must reproduce the above copyright
 *    notice, this list of conditions and the following disclaimer in the
 *    documentation and/or other materials provided with the distribution.
 * 3. Neither the name of the University nor the names of its contributors
 *    may be used to endorse or promote products derived from this software
 *    without specific prior written permission.
 *
 * THIS SOFTWARE IS PROVIDED BY THE REGENTS AND CONTRIBUTORS ``AS IS'' AND
 * ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
 * ARE DISCLAIMED.  IN NO EVENT SHALL THE REGENTS OR CONTRIBUTORS BE LIABLE
 * FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 * DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS
 * OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION)
 * HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
 * LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
 * OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
 * SUCH DAMAGE.
 */


/*
 * long double (binary128) de la libc de bionic que no se puede reenviar al host: texto <-> long double.
 *
 * - strtold, strtold_l, wcstold, wcstold_l: exportadas con su nombre (la tabla de firmas las marca Unsafe).
 * - __heddle_snprintf_ld: formatea UNA conversion %L[aAeEfFgG] con el vfprintf de bionic (el puente formatea el
 *   resto del formato con el printf del host y solo delega aqui esas conversiones). No es API: el puente la busca
 *   por nombre y resolve() no la entrega al guest.
 *
 * Todo es codigo de bionic (gdtoa, stdio/vfprintf.cpp, stdio/parsefloat.c, fvwrite.c) en el commit de
 * guest/BIONIC_COMMIT; este archivo solo replica los envoltorios de bionic que no se pueden compilar tal cual
 * (bionic/strtold.cpp, bionic/stdlib_l.cpp, bionic/wcstod.cpp, vsnprintf de stdio/stdio.cpp) y define, como
 * internos: el mutex de gdtoa (el mutex normal de bionic, sin cruzar la frontera), nl_langinfo(RADIXCHAR) para
 * vfprintf (en bionic siempre ".") y las rutinas de stdio que esos caminos referencian pero no ejecutan.
 */

#include <errno.h>
#include <langinfo.h>
#include <float.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>
#include <wchar.h>
#include <wctype.h>

#include "local.h"

#define EXPORT extern "C" __attribute__((visibility("default")))

extern "C" int __strtorQ(const char*, char**, int, void*);
extern "C" int __vfprintf(FILE*, const char*, va_list);
extern "C" size_t parsefloat(FILE*, char*, char*);

// --- Ruta rapida de strtold (no es de bionic; da los mismos bits que __strtorQ) -------------------------------------
// Decimal simple ([+-]digitos[.digitos][e[+-]digitos], sin espacios delante, ni hexadecimal, inf o nan) con a lo
// sumo 34 cifras significativas w (< 10^34 < 2^113: exacto en binary128) y valor w * 10^k con |k| <= 48 (10^48 =
// 5^48 * 2^48, 5^48 < 2^113: exacto). El resultado es UNA multiplicacion o division de binary128 de compiler-rt,
// correctamente redondeada: el mismo valor que gdtoa (correctamente redondeado) en redondeo al mas cercano, el
// unico modo en que se usa (con otro, FLT_ROUNDS cambia el de gdtoa). Sin desbordamiento ni subnormales en ese
// rango: gdtoa no toca errno. Lo demas va a __strtorQ.
static const long double kP10[49] = {
    1e0L,  1e1L,  1e2L,  1e3L,  1e4L,  1e5L,  1e6L,  1e7L,  1e8L,  1e9L,  1e10L, 1e11L, 1e12L,
    1e13L, 1e14L, 1e15L, 1e16L, 1e17L, 1e18L, 1e19L, 1e20L, 1e21L, 1e22L, 1e23L, 1e24L, 1e25L,
    1e26L, 1e27L, 1e28L, 1e29L, 1e30L, 1e31L, 1e32L, 1e33L, 1e34L, 1e35L, 1e36L, 1e37L, 1e38L,
    1e39L, 1e40L, 1e41L, 1e42L, 1e43L, 1e44L, 1e45L, 1e46L, 1e47L, 1e48L};

static inline bool is_digit(char c) {
  return c >= '0' && c <= '9';
}

static bool strtold_fast(const char* s, char** end_ptr, long double* out) {
  unsigned long fpcr;
  __asm__("mrs %0, fpcr" : "=r"(fpcr));
  if (fpcr & (3UL << 22)) return false;  // no es redondeo al mas cercano
  const char* p = s;
  bool neg = false;
  if (*p == '-' || *p == '+') neg = *p++ == '-';
  if (!is_digit(*p) && !(*p == '.' && is_digit(p[1]))) return false;  // espacios, inf, nan, "." solo...
  if (p[0] == '0' && (p[1] == 'x' || p[1] == 'X')) return false;       // hexadecimal
  unsigned __int128 w = 0;
  int nd = 0, pend = 0, nfrac = 0;
  bool frac = false;
  for (;; p++) {
    if (*p == '.' && !frac) {
      frac = true;
      continue;
    }
    if (!is_digit(*p)) break;
    if (frac) nfrac++;
    if (*p == '0') {
      if (w) pend++;
      continue;
    }
    nd += pend + 1;
    if (nd > 34) return false;
    for (; pend; pend--) w *= 10;
    w = w * 10 + (*p - '0');
  }
  long e = 0;
  if (*p == 'e' || *p == 'E') {
    const char* q = p + 1;
    bool eneg = false;
    if (*q == '-' || *q == '+') eneg = *q++ == '-';
    if (is_digit(*q)) {
      int n = 0;
      for (; is_digit(*q); q++) {
        if (++n > 6) return false;
        e = e * 10 + (*q - '0');
      }
      if (eneg) e = -e;
      p = q;
    }
  }
  long double r;
  if (w == 0) {
    r = 0.0L;
  } else {
    long k = e + pend - nfrac;
    if (k > 48 && nd + (k - 48) <= 34) {
      for (; k > 48; k--) w *= 10;
    }
    if (k < -48 || k > 48) return false;
    r = static_cast<long double>(w);
    if (k >= 0) {
      r *= kP10[k];
    } else {
      r /= kP10[-k];
    }
  }
  if (end_ptr) *end_ptr = const_cast<char*>(p);
  *out = neg ? -r : r;
  return true;
}

// bionic/strtold.cpp (LP64), con la ruta rapida delante
EXPORT long double strtold(const char* s, char** end_ptr) {
  long double result;
  if (strtold_fast(s, end_ptr, &result)) return result;
  __strtorQ(s, end_ptr, FLT_ROUNDS, &result);
  return result;
}

// Solo pruebas del puente (resolve() no las entrega al guest): cada ruta por separado, para compararlas.
EXPORT int __heddle_strtold_fast(const char* s, char** end_ptr, long double* out) {
  return strtold_fast(s, end_ptr, out);
}
EXPORT long double __heddle_strtold_slow(const char* s, char** end_ptr) {
  long double result;
  __strtorQ(s, end_ptr, FLT_ROUNDS, &result);
  return result;
}

// bionic/stdlib_l.cpp. Con nombre propio y etiqueta de ensamblador: las cabeceras de NDK recientes declaran
// strtold_l/wcstold_l como alias (__RENAME) de strtold/wcstold y no se pueden definir con su nombre.
EXPORT long double heddle_strtold_l(const char* s, char** end_ptr, locale_t) __asm__("strtold_l");
EXPORT long double heddle_strtold_l(const char* s, char** end_ptr, locale_t) {
  return strtold(s, end_ptr);
}

// bionic/wcstod.cpp, plantilla wcstod<long double> (new[]/delete[] -> malloc/free)
EXPORT long double wcstold(const wchar_t* str, wchar_t** end) {
  const wchar_t* original_str = str;
  while (iswspace(*str)) {
    str++;
  }
  size_t max_len = wcsspn(str, L"-+0123456789.xXeEpP()nNaAiIfFtTyY");
  // (heddle: una cadena corta va en la pila en lugar de new[]; el resultado no cambia)
  char small[128];
  char* ascii_str = max_len < sizeof(small) ? small : static_cast<char*>(malloc(max_len + 1));
  if (!ascii_str) return 0.0L;
  for (size_t i = 0; i < max_len; ++i) {
    ascii_str[i] = str[i] & 0xff;
  }
  ascii_str[max_len] = 0;

  FILE f;
  __sfileext fext;
  _FILEEXT_SETUP(&f, &fext);
  f._flags = __SRD;
  f._bf._base = f._p = reinterpret_cast<unsigned char*>(ascii_str);
  f._bf._size = f._r = max_len;
  f._read = [](void*, char*, int) { return 0; };
  f._lb._base = nullptr;

  size_t actual_len = parsefloat(&f, ascii_str, ascii_str + max_len);

  char* ascii_end;
  long double result = strtold(ascii_str, &ascii_end);
  if (ascii_end != ascii_str + actual_len) abort();

  if (end) {
    if (actual_len == 0) {
      *end = const_cast<wchar_t*>(original_str);
    } else {
      *end = const_cast<wchar_t*>(str) + actual_len;
    }
  }

  if (ascii_str != small) free(ascii_str);
  return result;
}

EXPORT long double heddle_wcstold_l(const wchar_t* s, wchar_t** end, locale_t) __asm__("wcstold_l");
EXPORT long double heddle_wcstold_l(const wchar_t* s, wchar_t** end, locale_t) {
  return wcstold(s, end);
}

// vsnprintf de stdio/stdio.cpp, con la firma variadica de snprintf
EXPORT int __heddle_snprintf_ld(char* s, size_t n, const char* fmt, ...) {
  __check_count("vsnprintf", "size", n);
  char one_byte_buffer[1];
  if (n == 0) {
    s = one_byte_buffer;
    n = 1;
  }
  FILE f;
  __sfileext fext;
  _FILEEXT_SETUP(&f, &fext);
  f._file = -1;
  f._flags = __SWR | __SSTR;
  f._bf._base = f._p = reinterpret_cast<unsigned char*>(s);
  f._bf._size = f._w = n - 1;
  va_list ap;
  va_start(ap, fmt);
  int result = __vfprintf(&f, fmt, ap);
  va_end(ap);
  *f._p = '\0';
  return result;
}

// --- nl_langinfo de vfprintf.cpp (compilado con -Dnl_langinfo=__heddle_nl_langinfo) ---------------------------------
// bionic/langinfo.cpp: RADIXCHAR es siempre "." (bionic solo tiene los locales C y C.UTF-8). vfprintf no pide otro.
extern "C" char* __heddle_nl_langinfo(nl_item item) {
  if (item == RADIXCHAR) return const_cast<char*>(".");
  abort();
}

// --- Bloqueos de gdtoa (guest/include/thread_private.h) ----------------------------------------------------------
// Mutex normal sobre la primera palabra de la ranura: 0 libre, 1 tomado, 2 tomado con esperas (futex privado).
static long futex(int* w, int op, int v) {
  register long x0 __asm__("x0") = reinterpret_cast<long>(w);
  register long x1 __asm__("x1") = op;
  register long x2 __asm__("x2") = v;
  register long x3 __asm__("x3") = 0;
  register long x8 __asm__("x8") = 98;  // __NR_futex
  __asm__ volatile("svc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3), "r"(x8) : "memory");
  return x0;
}
extern "C" void __heddle_slot_lock(void* slot) {
  int* w = static_cast<int*>(slot);
  int c = 0;
  if (__atomic_compare_exchange_n(w, &c, 1, false, __ATOMIC_ACQUIRE, __ATOMIC_RELAXED)) return;
  if (c != 2) c = __atomic_exchange_n(w, 2, __ATOMIC_ACQUIRE);
  while (c != 0) {
    futex(w, 128 /* FUTEX_WAIT_PRIVATE */, 2);
    c = __atomic_exchange_n(w, 2, __ATOMIC_ACQUIRE);
  }
}
extern "C" void __heddle_slot_unlock(void* slot) {
  int* w = static_cast<int*>(slot);
  if (__atomic_exchange_n(w, 0, __ATOMIC_RELEASE) == 2) futex(w, 129 /* FUTEX_WAKE_PRIVATE */, 1);
}

// --- Internos de stdio referenciados por esos caminos -----------------------------------------------------------
// parsefloat sobre el FILE de mentira de wcstold (compilado con -D__srefill=... -Dungetc=...): __srefill de bionic
// con _read = "sin mas datos" deja el FILE en EOF; lo que parsefloat devuelve con ungetc no se vuelve a leer (el
// FILE se descarta), asi que solo importa que no falle.
extern "C" int __heddle_srefill(FILE* fp) {
  fp->_r = 0;
  fp->_flags |= __SEOF;
  return EOF;
}
extern "C" int __heddle_ungetc(int c, FILE*) {
  return c;
}
// Caminos de vfprintf/fvwrite que una cadena (__SWR | __SSTR, buffer no nulo) no recorre.
extern "C" int __swsetup(FILE*) {
  abort();
}
extern "C" int __sflush(FILE*) {
  abort();
}
// (openbsd-compat.h la declara sin extern "C" en C++; fvwrite.c, en C, la llama con su nombre de C)
extern "C" void* heddle_recallocarray(void*, size_t, size_t, size_t) __asm__("recallocarray");
extern "C" void* heddle_recallocarray(void*, size_t, size_t, size_t) {
  abort();
}
extern "C" void async_safe_fatal_va_list(const char*, const char*, va_list) {
  abort();
}
// %m y %#m: el puente nunca los pasa a __heddle_snprintf_ld
extern "C" char* __gnu_strerror_r(int, char*, size_t) {
  abort();
}
extern "C" const char* strerrorname_np(int) {
  abort();
}
