// Proxys del NDK: objetos con tabla de funciones del host (OpenSL ES, OpenMAX AL) y estructuras de callbacks
// (AMediaCodec asincrono, camara, AImageReader, timer_create con SIGEV_THREAD), mas callbacks tipados que el host llama
// desde sus propios hilos (AAudio, AMediaDataSource, AChoreographer); zlib con asignador propio, glob con
// GLOB_ALTDIRFUNC, Vulkan (pAllocator, cadenas pNext), registrador/abortador de liblog, pthread_cleanup_push y la salida de un hilo por la llamada al sistema exit. El host es tests/host/mock_ndk.c (LD_PRELOAD).
#include "../ndk_min.h"
typedef unsigned long size_t;
extern int printf(const char *, ...);
extern int usleep(unsigned int);
struct timespec {
    long tv_sec, tv_nsec;
};
extern int clock_gettime(int, struct timespec *);
static int fails;
#define CHECK(c) do { if (!(c)) { printf("FALLO linea %d: %s\n", __LINE__, #c); fails++; } } while (0)

static long now_ns(void) {
    struct timespec t;
    clock_gettime(1, &t);
    return t.tv_sec * 1000000000L + t.tv_nsec;
}
/* espera (hasta ~5 s) a que `*p` valga al menos `v` */
static int wait_for(volatile int *p, int v) {
    for (int i = 0; i < 50000 && *p < v; i++) usleep(100);
    return *p >= v;
}
static int same(const char *a, const char *b) {
    while (*a && *a == *b) a++, b++;
    return *a == *b;
}

// ------------------------------------------------------------------ OpenSL ES

static SLObjectItf g_mix;
static volatile int obj_cb_n, obj_cb_bad;
static void on_obj(SLObjectItf caller, const void *ctx, SLuint32 ev, SLresult res, SLuint32 param, void *itf) {
    // el host pasa su objeto; el guest debe ver SU proxy (el mismo puntero que recibio de CreateOutputMix)
    if (caller == g_mix && ctx == (void *)0x77 && ev == SL_OBJECT_EVENT_ASYNC_TERMINATION && res == 0 && param == SL_OBJECT_STATE_REALIZED && itf == 0)
        obj_cb_n++;
    else
        obj_cb_bad++;
}

#define BQ_TARGET 20000
static SLAndroidSimpleBufferQueueItf g_bq;
static SLPlayItf g_play;
static short bufs[2][64];
static volatile int bq_n, bq_bad, enq_bad;
static int enqueued;
static void on_bq(SLAndroidSimpleBufferQueueItf caller, void *ctx) {
    if (caller != g_bq || ctx != (void *)bufs) {
        bq_bad++;
        return;
    }
    int k = bq_n + 1;
    if (enqueued < BQ_TARGET) {
        short *b = bufs[enqueued % 2];
        for (int i = 0; i < 64; i++) b[i] = (short)(enqueued & 0xff);
        if ((*caller)->Enqueue(caller, b, sizeof bufs[0]) != 0) enq_bad++;
        enqueued++;
    }
    bq_n = k;
}
static volatile SLuint32 play_ev;
static void on_play(SLPlayItf caller, void *ctx, SLuint32 ev) {
    if (caller == g_play && ctx == (void *)5) play_ev = ev;
}

static SLuint32 host_objs(void) {
    SLuint32 n = 0;
    slQueryNumSupportedEngineInterfaces(&n);
    return n;
}

static void test_sles(void) {
    SLuint32 objs0 = host_objs();
    SLObjectItf eng = 0;
    SLEngineOption opt = {1 /* SL_ENGINEOPTION_THREADSAFE */, SL_BOOLEAN_TRUE};
    CHECK(slCreateEngine(&eng, 1, &opt, 0, 0, 0) == 0 && eng != 0);
    CHECK((*eng)->Realize(eng, SL_BOOLEAN_FALSE) == 0);
    SLuint32 st = 0;
    CHECK((*eng)->GetState(eng, &st) == 0 && st == SL_OBJECT_STATE_REALIZED);
    SLEngineItf e = 0, e2 = 0;
    CHECK((*eng)->GetInterface(eng, SL_IID_ENGINE, &e) == 0 && e != 0);
    CHECK((*eng)->GetInterface(eng, SL_IID_ENGINE, &e2) == 0 && e2 == e);
    SLuint32 q = 0;
    CHECK((*e)->QueryNumSupportedInterfaces(e, 41, &q) == 0 && q == 42);

    // output mix: Realize asincrono, el callback del objeto llega desde un hilo del host
    SLInterfaceID mids[1] = {SL_IID_ENVIRONMENTALREVERB};
    SLboolean mreq[1] = {SL_BOOLEAN_FALSE};
    CHECK((*e)->CreateOutputMix(e, &g_mix, 1, mids, mreq) == 0 && g_mix != 0);
    CHECK((*g_mix)->RegisterCallback(g_mix, on_obj, (void *)0x77) == 0);
    CHECK((*g_mix)->Realize(g_mix, SL_BOOLEAN_TRUE) == 0);
    CHECK(wait_for(&obj_cb_n, 1) && obj_cb_bad == 0);
    SLEnvironmentalReverbItf rev = 0;
    CHECK((*g_mix)->GetInterface(g_mix, SL_IID_ENVIRONMENTALREVERB, &rev) == 0 && rev != 0);
    SLmillibel rl = 0;
    CHECK((*rev)->SetRoomLevel(rev, -1000) == 0 && (*rev)->GetRoomLevel(rev, &rl) == 0 && rl == -1000);
    // interfaz que el objeto no tiene: el error del host llega tal cual y no se escribe nada
    SLPlayItf none = (SLPlayItf)0x1;
    CHECK((*g_mix)->GetInterface(g_mix, SL_IID_PLAY, &none) == SL_RESULT_FEATURE_UNSUPPORTED && none == (SLPlayItf)0x1);

    // reproductor con cola de buffers hacia el output mix (el localizador lleva el objeto: el host recibe el real)
    SLDataLocator_AndroidSimpleBufferQueue loc = {SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE, 2};
    SLDataFormat_PCM fmt = {SL_DATAFORMAT_PCM, 1, 48000000, 16, 16, 4, 2};
    SLDataSource src = {&loc, &fmt};
    SLDataLocator_OutputMix lom = {SL_DATALOCATOR_OUTPUTMIX, g_mix};
    SLDataSink snk = {&lom, 0};
    SLInterfaceID ids[4] = {SL_IID_ANDROIDSIMPLEBUFFERQUEUE, SL_IID_VOLUME, SL_IID_PLAY, SL_IID_EFFECTSEND};
    SLboolean req[4] = {SL_BOOLEAN_TRUE, SL_BOOLEAN_TRUE, SL_BOOLEAN_TRUE, SL_BOOLEAN_TRUE};
    SLObjectItf pl = 0;
    CHECK((*e)->CreateAudioPlayer(e, &pl, &src, &snk, 4, ids, req) == 0 && pl != 0);
    CHECK(lom.outputMix == g_mix); // la estructura del guest no se toca
    CHECK((*pl)->Realize(pl, SL_BOOLEAN_FALSE) == 0);
    SLVolumeItf vol = 0;
    SLEffectSendItf send = 0;
    SLAndroidSimpleBufferQueueItf bq2 = 0;
    CHECK((*pl)->GetInterface(pl, SL_IID_PLAY, &g_play) == 0 && g_play != 0);
    CHECK((*pl)->GetInterface(pl, SL_IID_VOLUME, &vol) == 0 && vol != 0);
    CHECK((*pl)->GetInterface(pl, SL_IID_EFFECTSEND, &send) == 0 && send != 0);
    CHECK((*pl)->GetInterface(pl, SL_IID_ANDROIDSIMPLEBUFFERQUEUE, &g_bq) == 0 && g_bq != 0);
    // las dos colas son la misma interfaz del host: el mismo proxy
    CHECK((*pl)->GetInterface(pl, SL_IID_BUFFERQUEUE, &bq2) == 0 && bq2 == g_bq);

    // SLmillibel es int16: AAPCS64 no obliga a extenderlo; el host x86-64 lo espera extendido
    SLresult (*set_raw)(SLVolumeItf, unsigned long) = (SLresult(*)(SLVolumeItf, unsigned long))(*vol)->SetVolumeLevel;
    CHECK(set_raw(vol, 0x7777000000000000ul | 0xF448ul) == 0);
    SLmillibel lv = 0;
    CHECK((*vol)->GetVolumeLevel(vol, &lv) == 0 && lv == -3000);
    SLpermille sp = 0;
    CHECK((*vol)->SetStereoPosition(vol, -500) == 0 && (*vol)->GetStereoPosition(vol, &sp) == 0 && sp == -500);
    SLboolean mute = 0;
    CHECK((*vol)->SetMute(vol, SL_BOOLEAN_TRUE) == 0 && (*vol)->GetMute(vol, &mute) == 0 && mute == SL_BOOLEAN_TRUE);
    // pAuxEffect (const void *) es una interfaz: el host recibe la del reverb real
    SLboolean en = 0;
    CHECK((*send)->EnableEffectSend(send, rev, SL_BOOLEAN_TRUE, -100) == 0);
    CHECK((*send)->IsEnabled(send, rev, &en) == 0 && en == 1);

    // reproduccion: el evento HEADMOVING llega dentro de SetPlayState; los de la cola, desde el hilo de audio
    CHECK((*g_play)->RegisterCallback(g_play, on_play, (void *)5) == 0);
    CHECK((*g_play)->SetCallbackEventsMask(g_play, SL_PLAYEVENT_HEADMOVING) == 0);
    CHECK((*g_bq)->RegisterCallback(g_bq, on_bq, (void *)bufs) == 0);
    for (enqueued = 0; enqueued < 2; enqueued++) CHECK((*g_bq)->Enqueue(g_bq, bufs[enqueued], sizeof bufs[0]) == 0);
    long t0 = now_ns();
    CHECK((*g_play)->SetPlayState(g_play, SL_PLAYSTATE_PLAYING) == 0);
    CHECK(play_ev == SL_PLAYEVENT_HEADMOVING);
    CHECK(wait_for(&bq_n, BQ_TARGET));
    long t1 = now_ns();
    CHECK(bq_n == BQ_TARGET && bq_bad == 0 && enq_bad == 0);
    printf("t9 OpenSL: %d callbacks de la cola (con Enqueue) en %ld us: %ld ns por buffer\n", bq_n, (t1 - t0) / 1000, (t1 - t0) / (bq_n ? bq_n : 1));
    SLuint32 ps = 0;
    CHECK((*g_play)->SetPlayState(g_play, SL_PLAYSTATE_STOPPED) == 0 && (*g_play)->GetPlayState(g_play, &ps) == 0 && ps == SL_PLAYSTATE_STOPPED);
    SLAndroidSimpleBufferQueueState bs = {9, 9};
    CHECK((*g_bq)->Clear(g_bq) == 0 && (*g_bq)->GetState(g_bq, &bs) == 0 && bs.count == 0 && bs.index == BQ_TARGET);
    CHECK(host_objs() == objs0 + 3);
    (*pl)->Destroy(pl);
    CHECK(host_objs() == objs0 + 2);

    // muchos reproductores creados y destruidos: los proxys se liberan con el objeto (5 por reproductor: si se
    // perdieran, la tabla de 4096 se agotaria antes de terminar)
    int ok = 0;
    for (int i = 0; i < 1200; i++) {
        SLObjectItf p = 0;
        SLPlayItf pi = 0;
        SLVolumeItf vi = 0;
        SLAndroidSimpleBufferQueueItf qi = 0;
        SLEffectSendItf si = 0;
        if ((*e)->CreateAudioPlayer(e, &p, &src, &snk, 4, ids, req) != 0 || (*p)->Realize(p, 0) != 0) break;
        if ((*p)->GetInterface(p, SL_IID_PLAY, &pi) || (*p)->GetInterface(p, SL_IID_VOLUME, &vi) || (*p)->GetInterface(p, SL_IID_ANDROIDSIMPLEBUFFERQUEUE, &qi) ||
            (*p)->GetInterface(p, SL_IID_EFFECTSEND, &si))
            break;
        (*p)->Destroy(p);
        ok++;
    }
    CHECK(ok == 1200);
    (*g_mix)->Destroy(g_mix);
    (*eng)->Destroy(eng);
    CHECK(host_objs() == objs0);
}

// ------------------------------------------------------------------ OpenMAX AL (11 argumentos: 5 en la pila del host)

static XAPlayItf g_xplay;
static volatile int xa_ev;
static void on_xplay(XAPlayItf caller, void *ctx, XAuint32 ev) {
    if (caller == g_xplay && ctx == (void *)9) xa_ev = (int)ev;
}
static void test_xa(void) {
    SLuint32 objs0 = host_objs();
    XAObjectItf xe = 0, xmix = 0, xp = 0;
    XAEngineItf xei = 0;
    CHECK(xaCreateEngine(&xe, 0, 0, 0, 0, 0) == 0 && xe != 0);
    CHECK((*xe)->Realize(xe, 0) == 0);
    CHECK((*xe)->GetInterface(xe, XA_IID_ENGINE, &xei) == 0 && xei != 0);
    CHECK((*xei)->CreateOutputMix(xei, &xmix, 0, 0, 0) == 0 && xmix != 0);
    SLDataLocator_OutputMix xl = {SL_DATALOCATOR_OUTPUTMIX, (SLObjectItf)xmix};
    XADataSink asnk = {&xl, 0};
    XADataLocator_NativeDisplay nd = {XA_DATALOCATOR_NATIVEDISPLAY, (void *)0x1234, 0};
    XADataSink vsnk = {&nd, 0};
    SLDataLocator_AndroidSimpleBufferQueue sl = {0x800007BEu, 4};
    XADataSource xsrc = {&sl, 0};
    XAInterfaceID xids[1] = {XA_IID_PLAY};
    XAboolean xreq[1] = {1};
    CHECK((*xei)->CreateMediaPlayer(xei, &xp, &xsrc, 0, &asnk, &vsnk, 0, 0, 1, xids, xreq) == 0 && xp != 0);
    CHECK((*xp)->Realize(xp, 0) == 0);
    CHECK((*xp)->GetInterface(xp, XA_IID_PLAY, &g_xplay) == 0 && g_xplay != 0);
    CHECK((*g_xplay)->RegisterCallback(g_xplay, on_xplay, (void *)9) == 0);
    CHECK((*g_xplay)->SetPlayState(g_xplay, XA_PLAYSTATE_PLAYING) == 0 && xa_ev == XA_PLAYEVENT_HEADATEND);
    (*xp)->Destroy(xp);
    (*xmix)->Destroy(xmix);
    (*xe)->Destroy(xe);
    CHECK(host_objs() == objs0);
}

// ------------------------------------------------------------------ AAudio (callback de datos en el hilo del host)

#define AA_TARGET 20000
static AAudioStream *g_st;
static volatile int aa_n, aa_bad, aa_err, aa_last;
static aaudio_data_callback_result_t on_data(AAudioStream *s, void *ud, void *data, SLint32 n) {
    if (s != g_st || ud != (void *)0x42 || n != 192) aa_bad++;
    float *f = data;
    int k = aa_n;
    for (int i = 0; i < n; i++) {
        f[2 * i] = (float)(k % 7) * 0.25f + (float)i / 256.0f;
        f[2 * i + 1] = -f[2 * i];
    }
    aa_n = k + 1;
    return aa_n >= AA_TARGET ? AAUDIO_CALLBACK_RESULT_STOP : AAUDIO_CALLBACK_RESULT_CONTINUE;
}
static void on_err(AAudioStream *s, void *ud, aaudio_result_t e) {
    if (s == g_st && ud == (void *)0x43) aa_last = e;
    aa_err++;
}
static void test_aaudio(void) {
    AAudioStreamBuilder *b = 0;
    CHECK(AAudio_createStreamBuilder(&b) == 0 && b);
    AAudioStreamBuilder_setFormat(b, AAUDIO_FORMAT_PCM_FLOAT);
    AAudioStreamBuilder_setChannelCount(b, 2);
    AAudioStreamBuilder_setDataCallback(b, on_data, (void *)0x42);
    AAudioStreamBuilder_setErrorCallback(b, on_err, (void *)0x43);
    CHECK(AAudioStreamBuilder_openStream(b, &g_st) == 0 && g_st);
    AAudioStreamBuilder_delete(b);
    long t0 = now_ns();
    CHECK(AAudioStream_requestStart(g_st) == 0);
    CHECK(wait_for(&aa_err, 1));
    long t1 = now_ns();
    CHECK(AAudioStream_requestStop(g_st) == 0);
    CHECK(aa_n == AA_TARGET && aa_bad == 0 && aa_err == 1 && aa_last == AAUDIO_ERROR_DISCONNECTED);
    printf("t9 AAudio: %d callbacks de 192 tramas estereo en %ld us: %ld ns por callback\n", aa_n, (t1 - t0) / 1000, (t1 - t0) / (aa_n ? aa_n : 1));
    CHECK(AAudioStream_close(g_st) == 0);
}

// ------------------------------------------------------------------ AMediaCodec: estructura de callbacks por valor

static volatile int mc_mask, mc_bad;
static void mc_in(AMediaCodec *c, void *ud, SLint32 idx) { (ud == (void *)0x99 && idx == 7) ? (mc_mask |= 1) : mc_bad++; }
static void mc_out(AMediaCodec *c, void *ud, SLint32 idx, AMediaCodecBufferInfo *bi) {
    (ud == (void *)0x99 && idx == 3 && bi->offset == 11 && bi->size == 22 && bi->presentationTimeUs == 33000000000L && bi->flags == 4) ? (mc_mask |= 2) : mc_bad++;
}
static void mc_fmt(AMediaCodec *c, void *ud, AMediaFormat *f) { (ud == (void *)0x99 && f == (AMediaFormat *)0x5555) ? (mc_mask |= 4) : mc_bad++; }
static void mc_err(AMediaCodec *c, void *ud, media_status_t e, SLint32 action, const char *d) {
    (ud == (void *)0x99 && e == -10000 && action == 2 && same(d, "detalle")) ? (mc_mask |= 8) : mc_bad++;
}
static void test_mediacodec(void) {
    AMediaCodec *c = AMediaCodec_createDecoderByType("audio/mp4a-latm");
    CHECK(c != 0);
    AMediaCodecOnAsyncNotifyCallback cb = {mc_in, mc_out, mc_fmt, mc_err};
    CHECK(AMediaCodec_setAsyncNotifyCallback(c, cb, (void *)0x99) == 0);
    CHECK(AMediaCodec_start(c) == 0);
    CHECK(AMediaCodec_stop(c) == 0);
    CHECK(mc_mask == 15 && mc_bad == 0);
    CHECK(cb.onAsyncInputAvailable == mc_in);
    AMediaCodec_delete(c);
}

// ------------------------------------------------------------------ AMediaDataSource

static __INT64_TYPE__ read_at(void *ud, __INT64_TYPE__ off, void *buf, size_t sz) {
    if (ud != (void *)0x31 || off != (1L << 40) || sz != 16) return -1;
    for (int i = 0; i < 16; i++) ((unsigned char *)buf)[i] = (unsigned char)(i * 3);
    return 16;
}
static void test_datasource(void) {
    AMediaDataSource *ds = AMediaDataSource_new();
    AMediaDataSource_setUserdata(ds, (void *)0x31);
    AMediaDataSource_setReadAt(ds, read_at);
    AMediaExtractor *ex = AMediaExtractor_new();
    CHECK(AMediaExtractor_setDataSourceCustom(ex, ds) == 0);
    AMediaExtractor_delete(ex);
    AMediaDataSource_delete(ds);
}

// ------------------------------------------------------------------ camara y AImageReader: estructuras copiadas

static volatile int cam_av, cam_unav, cam_bad, dev_err, dev_disc;
static void on_cam_av(void *ctx, const char *id) { (ctx == (void *)0x61 && same(id, "0")) ? cam_av++ : cam_bad++; }
static void on_cam_unav(void *ctx, const char *id) { (ctx == (void *)0x61 && same(id, "1")) ? cam_unav++ : cam_bad++; }
static ACameraDevice *g_dev;
static void on_disc(void *ctx, ACameraDevice *d) { (ctx == (void *)0x62) ? dev_disc++ : cam_bad++; }
static void on_dev_err(void *ctx, ACameraDevice *d, int e) { (ctx == (void *)0x62 && e == 3) ? (dev_err = 1) : cam_bad++; }
static volatile int img_n;
static AImageReader *g_reader;
static void on_img(void *ctx, AImageReader *r) {
    if (ctx == (void *)0x71 && r == g_reader) img_n++;
}
static void test_camera(void) {
    ACameraManager *m = ACameraManager_create();
    ACameraManager_AvailabilityCallbacks acb = {(void *)0x61, on_cam_av, on_cam_unav};
    CHECK(ACameraManager_registerAvailabilityCallback(m, &acb) == 0);
    CHECK(cam_av == 1 && cam_unav == 1 && cam_bad == 0);
    CHECK(acb.onCameraAvailable == on_cam_av && acb.context == (void *)0x61);
    // la baja compara la estructura copiada: la conversion debe dar los mismos trampolines
    CHECK(ACameraManager_unregisterAvailabilityCallback(m, &acb) == 0);
    CHECK(ACameraManager_unregisterAvailabilityCallback(m, &acb) != 0);
    ACameraDevice_StateCallbacks scb = {(void *)0x62, on_disc, on_dev_err};
    CHECK(ACameraManager_openCamera(m, "0", &scb, &g_dev) == 0 && g_dev);
    CHECK(ACameraDevice_close(g_dev) == 0);
    CHECK(dev_err == 1 && dev_disc == 1 && cam_bad == 0);
    ACameraManager_delete(m);

    CHECK(AImageReader_new(64, 64, 1, 2, &g_reader) == 0 && g_reader);
    AImageReader_ImageListener l = {(void *)0x71, on_img};
    CHECK(AImageReader_setImageListener(g_reader, &l) == 0);
    CHECK(img_n == 1);
    AImageReader_delete(g_reader);
}

// ------------------------------------------------------------------ AChoreographer y timer_create

static volatile long frame_t, rr_last;
static volatile int rr_n;
static void on_frame(__INT64_TYPE__ t, void *d) {
    if (d == (void *)0x81) frame_t = t;
}
static void on_rr(__INT64_TYPE__ p, void *d) {
    if (d == (void *)0x82) rr_last = p, rr_n++;
}
union sigval {
    int i;
    void *p;
};
struct sigev {
    union sigval value;
    int signo, notify;
    void (*fn)(union sigval);
    void *attr;
    long pad[4];
};
struct itimerspec {
    long isec, insec, vsec, vnsec;
};
extern int timer_create(int, struct sigev *, void **);
extern int timer_settime(void *, int, const struct itimerspec *, struct itimerspec *);
extern int timer_delete(void *);
static volatile int timer_n, timer_bad;
static void on_timer(union sigval v) { (v.p == (void *)0x1234) ? timer_n++ : timer_bad++; }
static void test_misc(void) {
    AChoreographer *ch = AChoreographer_getInstance();
    AChoreographer_postFrameCallback64(ch, on_frame, (void *)0x81);
    CHECK(frame_t == 123456789012345L);
    AChoreographer_registerRefreshRateCallback(ch, on_rr, (void *)0x82);
    CHECK(rr_n == 1 && rr_last == 16666666);
    AChoreographer_unregisterRefreshRateCallback(ch, on_rr, (void *)0x82);
    CHECK(rr_n == 2 && rr_last == -1);

    struct sigev ev = {{0}, 0, 2 /* SIGEV_THREAD */, on_timer, 0, {0}};
    ev.value.p = (void *)0x1234;
    void *t = 0;
    CHECK(timer_create(1 /* CLOCK_MONOTONIC */, &ev, &t) == 0);
    CHECK(ev.fn == on_timer);
    struct itimerspec its = {0, 0, 0, 1000000};
    CHECK(timer_settime(t, 0, &its, 0) == 0);
    CHECK(wait_for(&timer_n, 1) && timer_bad == 0);
    CHECK(timer_delete(t) == 0);
}

// ------------------------------------------------------------------ zlib (libz real del host)

typedef struct z_stream_s {
    const unsigned char *next_in;
    unsigned avail_in;
    unsigned long total_in;
    unsigned char *next_out;
    unsigned avail_out;
    unsigned long total_out;
    const char *msg;
    void *state;
    void *(*zalloc)(void *, unsigned, unsigned);
    void (*zfree)(void *, void *);
    void *opaque;
    int data_type;
    unsigned long adler, reserved;
} z_stream;
extern int deflateInit_(z_stream *, int, const char *, int);
extern int deflate(z_stream *, int);
extern int deflateEnd(z_stream *);
extern int deflateCopy(z_stream *, z_stream *);
extern int inflateInit_(z_stream *, const char *, int);
extern int inflate(z_stream *, int);
extern int inflateEnd(z_stream *);
extern void *malloc(size_t);
extern void free(void *);
extern int posix_memalign(void **, size_t, size_t);
extern void *memcpy(void *, const void *, size_t);
static volatile int z_alloc_n, z_free_n, z_bad;
static void *z_alloc(void *op, unsigned items, unsigned size) {
    if (op != (void *)0x7a) z_bad++;
    z_alloc_n++;
    return malloc((size_t)items * size);
}
static void z_free(void *op, void *p) {
    if (op != (void *)0x7a) z_bad++;
    z_free_n++;
    free(p);
}
static void test_zlib(void) {
    static unsigned char src[4096], comp[8192], out[4096];
    for (int i = 0; i < (int)sizeof src; i++) src[i] = 'a' + (i * 7) % 13;
    // asignador propio: zlib guarda la direccion del z_stream y llama a zalloc/zfree durante la llamada
    z_stream d = {0};
    d.zalloc = z_alloc;
    d.zfree = z_free;
    d.opaque = (void *)0x7a;
    CHECK(deflateInit_(&d, 6, "1.2.13", sizeof d) == 0);
    CHECK(d.zalloc == z_alloc && d.zfree == z_free && z_alloc_n > 0);
    // deflateCopy: el destino (sin inicializar) recibe las funciones del origen
    z_stream d2;
    for (int i = 0; i < (int)sizeof d2; i++) ((unsigned char *)&d2)[i] = 0xcc;
    int a0 = z_alloc_n;
    CHECK(deflateCopy(&d2, &d) == 0 && z_alloc_n > a0);
    CHECK(d2.zalloc == z_alloc && d2.zfree == z_free && d.zalloc == z_alloc);
    CHECK(deflateEnd(&d2) == 0);
    d.next_in = src;
    d.avail_in = sizeof src;
    d.next_out = comp;
    d.avail_out = sizeof comp;
    CHECK(deflate(&d, 4 /* Z_FINISH */) == 1 /* Z_STREAM_END */);
    unsigned long clen = d.total_out;
    CHECK(deflateEnd(&d) == 0 && z_alloc_n == z_free_n && z_bad == 0);
    // asignador por defecto: zlib escribe su zcalloc en el z_stream y el guest ve una funcion llamable
    z_stream in = {0};
    CHECK(inflateInit_(&in, "1.2.13", sizeof in) == 0);
    CHECK(in.zalloc != 0 && in.zfree != 0);
    if (in.zalloc && in.zfree) {
        void *p = in.zalloc(in.opaque, 4, 8);
        CHECK(p != 0);
        if (p) in.zfree(in.opaque, p);
    }
    in.next_in = comp;
    in.avail_in = clen;
    in.next_out = out;
    in.avail_out = sizeof out;
    CHECK(inflate(&in, 4) == 1 && in.total_out == sizeof src);
    int eq = 1;
    for (int i = 0; i < (int)sizeof src; i++) eq &= out[i] == src[i];
    CHECK(eq);
    CHECK(inflateEnd(&in) == 0);
}

// ------------------------------------------------------------------ glob con GLOB_ALTDIRFUNC

typedef struct {
    size_t gl_pathc, gl_matchc, gl_offs;
    int gl_flags;
    char **gl_pathv;
    int (*gl_errfunc)(const char *, int);
    void (*gl_closedir)(void *);
    void *(*gl_readdir)(void *);
    void *(*gl_opendir)(const char *);
    int (*gl_lstat)(const char *, void *);
    int (*gl_stat)(const char *, void *);
} glob_t; // bionic: 88 bytes, la misma disposicion en arm64 y x86-64
extern int glob(const char *, int, int (*)(const char *, int), glob_t *);
extern void globfree(glob_t *);
extern void *opendir(const char *);
extern void *readdir(void *);
extern int closedir(void *);
extern int stat(const char *, void *);
extern int lstat(const char *, void *);
static int g_od, g_rd, g_cd, g_stn, g_err;
static void *my_opendir(const char *p) { g_od++; return opendir(p); }
static void *my_readdir(void *d) { g_rd++; return readdir(d); }
static void my_closedir(void *d) { g_cd++; closedir(d); }
static int my_stat(const char *p, void *st) { g_stn++; return stat(p, st); }
static int my_lstat(const char *p, void *st) { g_stn++; return lstat(p, st); }
static int my_globerr(const char *p, int e) { g_err += same(p, "no_existe") && e == 2 /* ENOENT */; return 1; }
static void alt(glob_t *g) {
    g->gl_opendir = my_opendir;
    g->gl_readdir = my_readdir;
    g->gl_closedir = my_closedir;
    g->gl_stat = my_stat;
    g->gl_lstat = my_lstat;
}
static void test_glob(void) {
    glob_t g = {0};
    alt(&g);
    CHECK(glob("build/libt[0-9].so", 0x40 /* GLOB_ALTDIRFUNC */, 0, &g) == 0);
    CHECK(g.gl_pathc == 9 && same(g.gl_pathv[0], "build/libt1.so") && same(g.gl_pathv[8], "build/libt9.so"));
    CHECK(g_od > 0 && g_rd > 0 && g_cd == g_od && g.gl_opendir == my_opendir && g.gl_stat == my_stat);
    globfree(&g);
    // GLOB_MARK: el tipo del archivo sale de gl_stat/gl_lstat (struct stat convertida)
    glob_t m = {0};
    alt(&m);
    CHECK(glob("buil[d]", 0x40 | 0x08 /* GLOB_MARK */, 0, &m) == 0 && m.gl_pathc == 1 && same(m.gl_pathv[0], "build/") && g_stn > 0);
    globfree(&m);
    // gl_opendir falla: errfunc del guest y GLOB_ABORTED
    glob_t e = {0};
    alt(&e);
    CHECK(glob("no_existe/*", 0x40, my_globerr, &e) == -2 /* GLOB_ABORTED */ && g_err == 1);
    CHECK(e.gl_errfunc == my_globerr);
    globfree(&e);
    // sin GLOB_ALTDIRFUNC las funciones del glob_t no se usan
    glob_t n = {0};
    alt(&n);
    int od = g_od;
    CHECK(glob("build/libt[0-9].so", 0, 0, &n) == 0 && n.gl_pathc == 9 && g_od == od);
    globfree(&n);
}

// ------------------------------------------------------------------ Vulkan

static char vk_ud;
static volatile int va_n, vr_n, vf_n, vi_n, vif_n, vk_bad, dbg_n, rep_n, memrep_n;
static void *v_alloc(void *ud, size_t sz, size_t al, SLuint32 scope) {
    void *p = 0;
    (ud == &vk_ud && scope == 1) ? va_n++ : vk_bad++;
    return posix_memalign(&p, al < 8 ? 8 : al, sz) == 0 ? p : 0;
}
static void *v_realloc(void *ud, void *old, size_t sz, size_t al, SLuint32 scope) {
    void *p = 0;
    (ud == &vk_ud && sz == 128) ? vr_n++ : vk_bad++;
    if (posix_memalign(&p, al, sz) != 0) return 0;
    if (old) memcpy(p, old, 64), free(old);
    return p;
}
static void v_free(void *ud, void *p) {
    (ud == &vk_ud) ? vf_n++ : vk_bad++;
    free(p);
}
static void v_ialloc(void *ud, size_t sz, SLuint32 type, SLuint32 scope) { (ud == &vk_ud && sz == 4096) ? vi_n++ : vk_bad++; }
static void v_ifree(void *ud, size_t sz, SLuint32 type, SLuint32 scope) { (ud == &vk_ud && sz == 4096) ? vif_n++ : vk_bad++; }
static VkBool32 on_dbg(SLuint32 sev, VkFlags types, const VkDebugUtilsMessengerCallbackDataEXT *d, void *ud) {
    (ud == (void *)0xd1 && sev == 0x100 && types == 2 && d->messageIdNumber == 42 && same(d->pMessageIdName, "id")) ? dbg_n++ : vk_bad++;
    return 0;
}
static VkBool32 on_rep(VkFlags f, SLuint32 ot, __UINT64_TYPE__ obj, size_t loc, __INT32_TYPE__ code, const char *pre, const char *msg, void *ud) {
    (f == 4 && ot == 3 && obj == 0x123456789abcdefULL && loc == 77 && code == -5 && same(pre, "capa") && same(msg, "informe") && ud == (void *)0xd2) ? rep_n++ : vk_bad++;
    return 1;
}
static void on_memrep(const VkDeviceMemoryReportCallbackDataEXT *d, void *ud) {
    (ud == (void *)0xd3 && d->type == 1 && d->memoryObjectId == 99 && d->size == 4096 && d->objectType == 9 && d->objectHandle == 0xabc && d->heapIndex == 2) ? memrep_n++ : vk_bad++;
}
static void test_vulkan(void) {
    VkAllocationCallbacks ac = {&vk_ud, v_alloc, v_realloc, v_free, v_ialloc, v_ifree};
    SLuint32 feats[2] = {1, 3};
    // cadena: mensajero -> estructura sin callbacks -> informe (8 argumentos)
    VkDebugReportCallbackCreateInfoEXT rep = {VK_STRUCTURE_TYPE_DEBUG_REPORT_CALLBACK_CREATE_INFO_EXT, 0, 0xf, on_rep, (void *)0xd2};
    VkValidationFeaturesEXT vf = {VK_STRUCTURE_TYPE_VALIDATION_FEATURES_EXT, &rep, 2, feats, 0, 0};
    VkDebugUtilsMessengerCreateInfoEXT mci = {VK_STRUCTURE_TYPE_DEBUG_UTILS_MESSENGER_CREATE_INFO_EXT, &vf, 0, 0x1111, 0x7, on_dbg, (void *)0xd1};
    VkInstanceCreateInfo ici = {VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, &mci, 0, 0, 0, 0, 0, 0};
    VkInstance inst = 0;
    CHECK(vkCreateInstance(&ici, &ac, &inst) == 0 && inst != 0);
    CHECK(dbg_n == 1 && rep_n == 1 && va_n == 1 && vr_n == 1 && vf_n == 1 && vi_n == 1 && vif_n == 1);
    // lo del guest no se toca
    CHECK(ici.pNext == &mci && mci.pNext == &vf && vf.pNext == &rep && mci.pfnUserCallback == on_dbg && rep.pfnCallback == on_rep);
    CHECK(ac.pfnAllocation == v_alloc && ac.pfnFree == v_free);
    // sType que el puente no conoce delante del mensajero: no se copia; su pNext apunta a la copia durante la llamada
    // y vuelve a ser el del guest al terminar. Detras, una lista de controladores con su arreglo anidado.
    VkDirectDriverLoadingInfoLUNARG drv = {VK_STRUCTURE_TYPE_DIRECT_DRIVER_LOADING_INFO_LUNARG, 0, 0x33, 0};
    VkDirectDriverLoadingListLUNARG dl = {VK_STRUCTURE_TYPE_DIRECT_DRIVER_LOADING_LIST_LUNARG, 0, 0, 1, &drv};
    VkDebugUtilsMessengerCreateInfoEXT mci2 = {VK_STRUCTURE_TYPE_DEBUG_UTILS_MESSENGER_CREATE_INFO_EXT, &dl, 0, 0x1111, 0x7, on_dbg, (void *)0xd1};
    struct { SLuint32 sType; const void *pNext; void *self; } unk = {0x7ffe0001, &mci2, &unk};
    VkInstanceCreateInfo ici2 = {VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, &unk, 0, 0, 0, 0, 0, 0};
    VkInstance inst2 = 0;
    CHECK(vkCreateInstance(&ici2, 0, &inst2) == 0 && inst2 != 0 && dbg_n == 2);
    CHECK(unk.pNext == &mci2 && mci2.pNext == &dl && dl.pDrivers == &drv && mci2.pfnUserCallback == on_dbg);
    vkDestroyInstance(inst2, 0);
    dbg_n = 1;
    // extensiones por vkGetInstanceProcAddr: el mensajero guardado se llama despues, desde otro hilo del host
    PFN_vkCreateDebugUtilsMessengerEXT cm = (PFN_vkCreateDebugUtilsMessengerEXT)vkGetInstanceProcAddr(inst, "vkCreateDebugUtilsMessengerEXT");
    PFN_vkDestroyDebugUtilsMessengerEXT dm = (PFN_vkDestroyDebugUtilsMessengerEXT)vkGetInstanceProcAddr(inst, "vkDestroyDebugUtilsMessengerEXT");
    PFN_vkSubmitDebugUtilsMessageEXT sm = (PFN_vkSubmitDebugUtilsMessageEXT)vkGetInstanceProcAddr(inst, "vkSubmitDebugUtilsMessageEXT");
    CHECK(cm && dm && sm);
    if (cm && dm && sm) {
        VkDebugUtilsMessengerEXT msgr = 0;
        mci.pNext = 0;
        CHECK(cm(inst, &mci, &ac, &msgr) == 0 && msgr == 0x5151);
        VkDebugUtilsMessengerCallbackDataEXT data = {VK_STRUCTURE_TYPE_DEBUG_UTILS_MESSENGER_CALLBACK_DATA_EXT, 0, 0, "id", 42, "despues", 0, 0, 0, 0, 0, 0};
        sm(inst, 0x100, 2, &data);
        CHECK(dbg_n == 2);
        dm(inst, msgr, &ac);
        CHECK(vf_n == 2);
    }
    // dispositivo: el informe de memoria detras de una estructura sin callbacks
    VkDeviceDeviceMemoryReportCreateInfoEXT mr = {VK_STRUCTURE_TYPE_DEVICE_DEVICE_MEMORY_REPORT_CREATE_INFO_EXT, 0, 0, on_memrep, (void *)0xd3};
    VkValidationFeaturesEXT vf2 = {VK_STRUCTURE_TYPE_VALIDATION_FEATURES_EXT, &mr, 2, feats, 0, 0};
    VkDeviceCreateInfo dci = {VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, &vf2, 0, 0, 0, 0, 0, 0, 0, 0};
    VkDevice dev = 0;
    CHECK(vkCreateDevice((VkPhysicalDevice)1, &dci, 0, &dev) == 0 && memrep_n == 1 && mr.pfnUserCallback == on_memrep);
    VkBufferCreateInfo bci = {VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, 0, 0, 1000, 0x20, 0, 0, 0};
    VkBuffer b = 0;
    CHECK(vkCreateBuffer(dev, &bci, &ac, &b) == 0 && b != 0 && va_n == 2);
    vkDestroyBuffer(dev, b, &ac);
    CHECK(vf_n == 3);
    vkDestroyDevice(dev, 0);
    vkDestroyInstance(inst, &ac);
    CHECK(va_n == 3 && vf_n == 4 && vk_bad == 0);
    SLuint32 errs = 99;
    vkEnumerateInstanceVersion(&errs);
    CHECK(errs == 0);
}

// ------------------------------------------------------------------ liblog: registrador y abortador

typedef struct { int flags; void *h; unsigned long mask; void *restorer; } gsa; // struct sigaction de bionic LP64
extern int sigaction(int, const gsa *, gsa *);
static volatile int lg_n, lg_bad, ab_n;
static void my_logger(const struct __android_log_message *m) {
    if (m->struct_size == sizeof *m && same(m->tag, "t9")) lg_n++;
    else lg_bad++;
}
static void my_aborter(const char *msg) { same(msg, "adios") ? ab_n++ : lg_bad++; }
static void on_ill(int s, void *si, void *uc) { *(unsigned long *)((char *)uc + 440) += 4; } // pc de mcontext (aarch64)
static int logd_n(void) { return __android_log_is_loggable(0, "logd", 0); }
static void test_liblog(void) {
    int l0 = logd_n();
    __android_log_set_logger(my_logger);
    __android_log_write(4, "t9", "hola");
    CHECK(lg_n == 1 && logd_n() == l0);
    __android_log_buf_write(99, 4, "t9", "desde un hilo del host");
    CHECK(lg_n == 2);
    // un mensaje del propio puente (SIGILL entregada al manejador del guest) va a logd, no al registrador del guest
    gsa sa = {4 /* SA_SIGINFO */, (void *)on_ill, 0, 0}, old;
    sigaction(4, &sa, &old);
    __asm__ volatile(".inst 0x00000000" ::: "memory"); // udf #0
    sigaction(4, &old, 0);
    CHECK(lg_bad == 0 && lg_n == 2 && logd_n() > l0);
    // una funcion del host como registrador: el host recibe la suya
    __android_log_set_logger(__android_log_logd_logger);
    int l1 = logd_n();
    __android_log_write(4, "t9", "por defecto");
    CHECK(lg_n == 2 && logd_n() == l1 + 1);
    __android_log_set_logger(0);
    __android_log_set_aborter(my_aborter);
    __android_log_call_aborter("adios");
    CHECK(ab_n == 1);
    __android_log_buf_write(98, 4, "t9", "adios"); // el abortador desde un hilo del host
    CHECK(ab_n == 2 && lg_bad == 0);
    CHECK(__android_log_is_loggable(0, "err", 0) == 0);
}

// ------------------------------------------------------------------ pthread_cleanup_push/pop

struct __pthread_cleanup_t {
    struct __pthread_cleanup_t *prev;
    void (*routine)(void *);
    void *arg;
};
extern void __pthread_cleanup_push(struct __pthread_cleanup_t *, void (*)(void *), void *);
extern void __pthread_cleanup_pop(struct __pthread_cleanup_t *, int);
extern int pthread_create(unsigned long *, const void *, void *(*)(void *), void *);
extern int pthread_join(unsigned long, void **);
extern void pthread_exit(void *) __attribute__((noreturn));
extern int pthread_key_create(unsigned *, void (*)(void *));
extern int pthread_key_delete(unsigned);
extern int pthread_setspecific(unsigned, const void *);
extern int __cxa_thread_atexit_impl(void (*)(void *), void *, void *);
static char cl_order[16];
static volatile int cl_n;
static unsigned cl_key;
static void rec(void *a) {
    if (cl_n < 15) cl_order[cl_n++] = (char)(long)a;
}
/* la llamada al sistema exit directa (no pthread_exit): termina el hilo sin pasar por bionic */
__attribute__((noreturn)) static void raw_thread_exit(long code) {
    register long x0 __asm__("x0") = code;
    register long x8 __asm__("x8") = 93;
    __asm__ volatile("svc #0" ::"r"(x0), "r"(x8) : "memory");
    __builtin_unreachable();
}
static void *cl_thread(void *a) {
    struct __pthread_cleanup_t c1, c2, c3;
    __cxa_thread_atexit_impl(rec, (void *)'T', 0);
    pthread_setspecific(cl_key, (void *)'K');
    __pthread_cleanup_push(&c1, rec, (void *)'1');
    __pthread_cleanup_push(&c2, rec, (void *)'2');
    __pthread_cleanup_pop(&c2, 1);
    __pthread_cleanup_push(&c3, rec, (void *)'3');
    __pthread_cleanup_push(&c2, rec, (void *)'x');
    __pthread_cleanup_pop(&c2, 0);
    if (a == (void *)2) raw_thread_exit(5);
    if (a) pthread_exit((void *)7);
    __pthread_cleanup_pop(&c3, 0);
    __pthread_cleanup_pop(&c1, 0);
    return (void *)8;
}
static void test_cleanup(void) {
    CHECK(pthread_key_create(&cl_key, rec) == 0);
    unsigned long t;
    void *r = 0;
    // pthread_exit: destructores de thread_local, manejadores del mas reciente al mas antiguo, claves (bionic)
    cl_n = 0;
    CHECK(pthread_create(&t, 0, cl_thread, (void *)1) == 0 && pthread_join(t, &r) == 0 && r == (void *)7);
    cl_order[cl_n] = 0;
    CHECK(same(cl_order, "2T31K"));
    // la funcion del hilo vuelve con la cadena vacia
    cl_n = 0;
    CHECK(pthread_create(&t, 0, cl_thread, 0) == 0 && pthread_join(t, &r) == 0 && r == (void *)8);
    cl_order[cl_n] = 0;
    CHECK(same(cl_order, "2TK"));
    // exit directo (svc): como el nucleo en ARM, ni destructores de thread_local ni cadena de cleanup ni claves; el
    // valor de retorno del hilo no se escribe (pthread_join da NULL)
    cl_n = 0;
    r = (void *)1;
    CHECK(pthread_create(&t, 0, cl_thread, (void *)2) == 0 && pthread_join(t, &r) == 0 && r == 0);
    cl_order[cl_n] = 0;
    CHECK(same(cl_order, "2"));
    pthread_key_delete(cl_key);
}

int run_all(void) {
    test_sles();
    test_xa();
    test_aaudio();
    test_mediacodec();
    test_datasource();
    test_camera();
    test_misc();
    test_zlib();
    test_glob();
    test_vulkan();
    test_liblog();
    test_cleanup();
    printf("t9 run_all: %d fallos\n", fails);
    return fails;
}
