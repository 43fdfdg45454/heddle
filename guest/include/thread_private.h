/*
 * heddle: envoltorio de private/thread_private.h de bionic para gdtoa.
 *
 * gdtoa/misc.c usa como pthread_mutex_t una ranura de 8 bytes (static void *__dtoa_locks[2]): en bionic un mutex
 * normal solo usa su primera palabra (estado + futex). Reenviado al pthread_mutex_lock del host no serviria: otra
 * libc (glibc, en las pruebas en Linux) escribe sus 40 bytes y pisa la ranura vecina y la lista libre de gdtoa, y
 * cada llamada cruzaria la frontera (gdtoa bloquea en cada Balloc/Bfree). Aqui la ranura es ese mismo mutex normal
 * (palabra de estado y futex, como NormalMutexLock de bionic), ejecutado como codigo guest (stdio_ld.cpp).
 */
#pragma once
#include_next "thread_private.h"

__BEGIN_DECLS
__LIBC_HIDDEN__ void __heddle_slot_lock(void* slot);
__LIBC_HIDDEN__ void __heddle_slot_unlock(void* slot);
__END_DECLS

#undef _MUTEX_LOCK
#undef _MUTEX_UNLOCK
#define _MUTEX_LOCK(l) __heddle_slot_lock(l)
#define _MUTEX_UNLOCK(l) __heddle_slot_unlock(l)
