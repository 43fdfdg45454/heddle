/* Firmas suplementarias para sigtool: funciones que las bibliotecas del NDK exportan pero que sus cabeceras
 * publicas no declaran como funciones (FORTIFY, soporte de C++, variantes de 64 bits, macros de math.h).
 * Las firmas son las de bionic/zlib. NO incluir <math.h> aqui: define isinf/isnan como macros. */
#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>
#include <poll.h>
#include <signal.h>
#include <sys/socket.h>
#include <time.h>

/* FORTIFY */
void* __memchr_chk(const void*, int, size_t, size_t);
void* __memrchr_chk(const void*, int, size_t, size_t);
void* __mempcpy_chk(void*, const void*, size_t, size_t);
int __poll_chk(struct pollfd*, nfds_t, int, size_t);
int __ppoll_chk(struct pollfd*, nfds_t, const struct timespec*, const sigset_t*, size_t);
int __ppoll64_chk(struct pollfd*, nfds_t, const struct timespec*, const sigset64_t*, size_t);
ssize_t __pread64_chk(int, void*, size_t, off64_t, size_t);
ssize_t __pwrite_chk(int, const void*, size_t, off_t, size_t);
ssize_t __pwrite64_chk(int, const void*, size_t, off64_t, size_t);
ssize_t __readlinkat_chk(int, const char*, char*, size_t, size_t);
ssize_t __sendto_chk(int, const void*, size_t, size_t, int, const struct sockaddr*, socklen_t);
ssize_t __write_chk(int, const void*, size_t, size_t);
char* __stpncpy_chk(char*, const char*, size_t, size_t);
char* __stpncpy_chk2(char*, const char*, size_t, size_t, size_t);
char* __strncpy_chk2(char*, const char*, size_t, size_t, size_t);
size_t __strlcpy_chk(char*, const char*, size_t, size_t);
size_t __strlcat_chk(char*, const char*, size_t, size_t);
mode_t __umask_chk(mode_t);

/* soporte de C++ (libstdc++.so de bionic) */
int __cxa_guard_acquire(long long*);
void __cxa_guard_release(long long*);
void __cxa_guard_abort(long long*);

/* clasificacion de coma flotante (funciones reales detras de las macros de math.h) */
int __fpclassify(double);
int __fpclassifyd(double);
int __fpclassifyf(float);
int __fpclassifyl(long double);
int __isfinite(double);
int __isfinitef(float);
int __isfinitel(long double);
int __isinf(double);
int __isinff(float);
int __isinfl(long double);
int __isnan(double);
int __isnanf(float);
int __isnanl(long double);
int __isnormal(double);
int __isnormalf(float);
int __isnormall(long double);
int __signbit(double);
int __signbitf(float);
int __signbitl(long double);
int isinf(double);
int isnan(double);
int isfinite(double);
int isfinitef(float);
int isfinitel(long double);
int isnormal(double);
int isnormalf(float);
int isnormall(long double);

/* senales BSD */
int sigblock(int);
int sigsetmask(int);

/* zlib, variantes de 64 bits */
void* gzopen64(const char*, const char*);
long long gzseek64(void*, long long, int);
long long gztell64(void*);
long long gzoffset64(void*);
unsigned long adler32_combine64(unsigned long, unsigned long, long long);
unsigned long crc32_combine64(unsigned long, unsigned long, long long);

/* variantes GNU a las que enlazan strerror_r y basename con _GNU_SOURCE */
char* __gnu_strerror_r(int, char*, size_t);
char* __gnu_basename(const char*);
