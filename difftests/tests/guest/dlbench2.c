// Microbanco del cargador: biblioteca con 147 importaciones de libc/libm (relocaciones GLOB_DAT/ABS64).
extern void malloc(void);
extern void free(void);
extern void calloc(void);
extern void realloc(void);
extern void memcpy(void);
extern void memmove(void);
extern void memset(void);
extern void memcmp(void);
extern void strlen(void);
extern void strcmp(void);
extern void strncmp(void);
extern void strcpy(void);
extern void strncpy(void);
extern void strcat(void);
extern void strchr(void);
extern void strrchr(void);
extern void strstr(void);
extern void strdup(void);
extern void strtol(void);
extern void strtoul(void);
extern void strtoll(void);
extern void strtoull(void);
extern void strtod(void);
extern void strtof(void);
extern void atoi(void);
extern void atol(void);
extern void printf(void);
extern void fprintf(void);
extern void sprintf(void);
extern void snprintf(void);
extern void vsnprintf(void);
extern void puts(void);
extern void fputs(void);
extern void fwrite(void);
extern void fread(void);
extern void fopen(void);
extern void fclose(void);
extern void fflush(void);
extern void fseek(void);
extern void ftell(void);
extern void open(void);
extern void close(void);
extern void read(void);
extern void write(void);
extern void lseek(void);
extern void pread(void);
extern void pwrite(void);
extern void stat(void);
extern void fstat(void);
extern void lstat(void);
extern void access(void);
extern void unlink(void);
extern void rename(void);
extern void mkdir(void);
extern void rmdir(void);
extern void opendir(void);
extern void readdir(void);
extern void closedir(void);
extern void getenv(void);
extern void setenv(void);
extern void time(void);
extern void gettimeofday(void);
extern void clock_gettime(void);
extern void nanosleep(void);
extern void usleep(void);
extern void sleep(void);
extern void pthread_create(void);
extern void pthread_join(void);
extern void pthread_detach(void);
extern void pthread_self(void);
extern void pthread_mutex_init(void);
extern void pthread_mutex_lock(void);
extern void pthread_mutex_unlock(void);
extern void pthread_mutex_destroy(void);
extern void pthread_cond_init(void);
extern void pthread_cond_wait(void);
extern void pthread_cond_signal(void);
extern void pthread_cond_broadcast(void);
extern void pthread_cond_destroy(void);
extern void pthread_key_create(void);
extern void pthread_getspecific(void);
extern void pthread_setspecific(void);
extern void pthread_once(void);
extern void getpid(void);
extern void gettid(void);
extern void sysconf(void);
extern void mmap(void);
extern void munmap(void);
extern void mprotect(void);
extern void qsort(void);
extern void bsearch(void);
extern void abs(void);
extern void labs(void);
extern void rand(void);
extern void srand(void);
extern void isalpha(void);
extern void isdigit(void);
extern void isspace(void);
extern void toupper(void);
extern void tolower(void);
extern void sin(void);
extern void cos(void);
extern void tan(void);
extern void asin(void);
extern void acos(void);
extern void atan(void);
extern void atan2(void);
extern void sinf(void);
extern void cosf(void);
extern void tanf(void);
extern void sqrt(void);
extern void sqrtf(void);
extern void pow(void);
extern void powf(void);
extern void exp(void);
extern void expf(void);
extern void log(void);
extern void logf(void);
extern void log10(void);
extern void log10f(void);
extern void floor(void);
extern void floorf(void);
extern void ceil(void);
extern void ceilf(void);
extern void fmod(void);
extern void fmodf(void);
extern void fabs(void);
extern void fabsf(void);
extern void round(void);
extern void roundf(void);
extern void trunc(void);
extern void truncf(void);
extern void lrint(void);
extern void lrintf(void);
extern void hypot(void);
extern void hypotf(void);
extern void cbrt(void);
extern void sinh(void);
extern void cosh(void);
extern void tanh(void);
extern void exp2(void);
extern void log2(void);
extern void expm1(void);
extern void log1p(void);
extern void frexp(void);
extern void ldexp(void);
extern void modf(void);
void *dlbench2_tab[] = {
    (void *)malloc,
    (void *)free,
    (void *)calloc,
    (void *)realloc,
    (void *)memcpy,
    (void *)memmove,
    (void *)memset,
    (void *)memcmp,
    (void *)strlen,
    (void *)strcmp,
    (void *)strncmp,
    (void *)strcpy,
    (void *)strncpy,
    (void *)strcat,
    (void *)strchr,
    (void *)strrchr,
    (void *)strstr,
    (void *)strdup,
    (void *)strtol,
    (void *)strtoul,
    (void *)strtoll,
    (void *)strtoull,
    (void *)strtod,
    (void *)strtof,
    (void *)atoi,
    (void *)atol,
    (void *)printf,
    (void *)fprintf,
    (void *)sprintf,
    (void *)snprintf,
    (void *)vsnprintf,
    (void *)puts,
    (void *)fputs,
    (void *)fwrite,
    (void *)fread,
    (void *)fopen,
    (void *)fclose,
    (void *)fflush,
    (void *)fseek,
    (void *)ftell,
    (void *)open,
    (void *)close,
    (void *)read,
    (void *)write,
    (void *)lseek,
    (void *)pread,
    (void *)pwrite,
    (void *)stat,
    (void *)fstat,
    (void *)lstat,
    (void *)access,
    (void *)unlink,
    (void *)rename,
    (void *)mkdir,
    (void *)rmdir,
    (void *)opendir,
    (void *)readdir,
    (void *)closedir,
    (void *)getenv,
    (void *)setenv,
    (void *)time,
    (void *)gettimeofday,
    (void *)clock_gettime,
    (void *)nanosleep,
    (void *)usleep,
    (void *)sleep,
    (void *)pthread_create,
    (void *)pthread_join,
    (void *)pthread_detach,
    (void *)pthread_self,
    (void *)pthread_mutex_init,
    (void *)pthread_mutex_lock,
    (void *)pthread_mutex_unlock,
    (void *)pthread_mutex_destroy,
    (void *)pthread_cond_init,
    (void *)pthread_cond_wait,
    (void *)pthread_cond_signal,
    (void *)pthread_cond_broadcast,
    (void *)pthread_cond_destroy,
    (void *)pthread_key_create,
    (void *)pthread_getspecific,
    (void *)pthread_setspecific,
    (void *)pthread_once,
    (void *)getpid,
    (void *)gettid,
    (void *)sysconf,
    (void *)mmap,
    (void *)munmap,
    (void *)mprotect,
    (void *)qsort,
    (void *)bsearch,
    (void *)abs,
    (void *)labs,
    (void *)rand,
    (void *)srand,
    (void *)isalpha,
    (void *)isdigit,
    (void *)isspace,
    (void *)toupper,
    (void *)tolower,
    (void *)sin,
    (void *)cos,
    (void *)tan,
    (void *)asin,
    (void *)acos,
    (void *)atan,
    (void *)atan2,
    (void *)sinf,
    (void *)cosf,
    (void *)tanf,
    (void *)sqrt,
    (void *)sqrtf,
    (void *)pow,
    (void *)powf,
    (void *)exp,
    (void *)expf,
    (void *)log,
    (void *)logf,
    (void *)log10,
    (void *)log10f,
    (void *)floor,
    (void *)floorf,
    (void *)ceil,
    (void *)ceilf,
    (void *)fmod,
    (void *)fmodf,
    (void *)fabs,
    (void *)fabsf,
    (void *)round,
    (void *)roundf,
    (void *)trunc,
    (void *)truncf,
    (void *)lrint,
    (void *)lrintf,
    (void *)hypot,
    (void *)hypotf,
    (void *)cbrt,
    (void *)sinh,
    (void *)cosh,
    (void *)tanh,
    (void *)exp2,
    (void *)log2,
    (void *)expm1,
    (void *)log1p,
    (void *)frexp,
    (void *)ldexp,
    (void *)modf,
};
int dlbench2(void) { return sizeof dlbench2_tab / sizeof dlbench2_tab[0]; }
