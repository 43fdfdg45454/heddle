/* Host simulado (x86-64) de las API del NDK con punteros a funcion en objetos o estructuras: OpenSL ES, OpenMAX AL,
 * AAudio, AMediaCodec asincrono, AMediaDataSource, camara, AImageReader y AChoreographer. Se carga con LD_PRELOAD en
 * heddle-run (el puente resuelve contra el espacio global fuera de Android). Cada objeto tiene tablas de funciones
 * reales del host y llama a los callbacks como lo haria Android: desde hilos propios del host (sin estado guest) o
 * dentro de la llamada. Las comprobaciones de lo que llega al host se devuelven como codigos de error. Tambien Vulkan
 * (pAllocator y cadenas pNext con callbacks) y el registrador/abortador de liblog. */
#define _GNU_SOURCE
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include "../ndk_min.h"

/* ------------------------------------------------------------------ OpenSL ES */

#define IID(n, a) static const struct SLInterfaceID_ iid_##n = {a, 0x1111, 0x2222, 0x3333, {1, 2, 3, 4, 5, 6}}; const SLInterfaceID SL_IID_##n = &iid_##n
IID(ENGINE, 0x10);
IID(PLAY, 0x11);
IID(VOLUME, 0x12);
IID(EFFECTSEND, 0x13);
IID(ENVIRONMENTALREVERB, 0x14);
IID(ANDROIDSIMPLEBUFFERQUEUE, 0x15);
IID(BUFFERQUEUE, 0x16);
IID(SEEK, 0x17);
IID(OBJECT, 0x18);

enum { K_ENGINE = 1, K_MIX, K_PLAYER, K_XENGINE, K_XMIX, K_XPLAYER };

struct Obj;
struct Itf {
    const void *vt;
    struct Obj *o;
};
struct Obj {
    struct Itf obj, engine, play, volume, bq, send, reverb;
    int kind, realized;
    slObjectCallback ocb;
    void *octx;
    slPlayCallback pcb;
    void *pctx;
    SLuint32 mask, state;
    slAndroidSimpleBufferQueueCallback bqcb;
    void *bqctx;
    const void *queue[16];
    SLuint32 qsize[16];
    int qhead, qtail;
    SLmillibel level;
    SLpermille stereo;
    SLboolean mute;
    pthread_t th;
    int th_live;
    pthread_mutex_t mu;
    long consumed, sum;
    struct Obj *mix;
};
static atomic_int live_objs;
/* objetos vivos del host simulado (el guest lo consulta con slQueryNumSupportedEngineInterfaces: 1000 + vivos) */
static struct Obj *all_objs[64];

#define O(self) (((struct Itf *)(self))->o)

static int is_obj(const void *p, int kind) {
    for (int i = 0; i < 64; i++)
        if (all_objs[i] && &all_objs[i]->obj == p && all_objs[i]->kind == kind) return 1;
    return 0;
}

static const struct SLObjectItf_ obj_vt;
static const struct SLEngineItf_ engine_vt;
static const struct SLPlayItf_ play_vt;
static const struct SLVolumeItf_ volume_vt;
static const struct SLAndroidSimpleBufferQueueItf_ bq_vt;
static const struct SLEffectSendItf_ send_vt;
static const struct SLEnvironmentalReverbItf_ reverb_vt;

static struct Obj *new_obj(int kind) {
    struct Obj *o = calloc(1, sizeof *o);
    o->kind = kind;
    o->obj = (struct Itf){&obj_vt, o};
    o->engine = (struct Itf){&engine_vt, o};
    o->play = (struct Itf){&play_vt, o};
    o->volume = (struct Itf){&volume_vt, o};
    o->bq = (struct Itf){&bq_vt, o};
    o->send = (struct Itf){&send_vt, o};
    o->reverb = (struct Itf){&reverb_vt, o};
    pthread_mutex_init(&o->mu, 0);
    for (int i = 0; i < 64; i++)
        if (!all_objs[i]) { all_objs[i] = o; break; }
    atomic_fetch_add(&live_objs, 1);
    return o;
}

static void *realize_thread(void *a) {
    struct Obj *o = a;
    usleep(1000);
    /* SL_OBJECT_EVENT_ASYNC_TERMINATION desde un hilo del host, como el Realize asincrono de Android */
    o->ocb((SLObjectItf)&o->obj, o->octx, SL_OBJECT_EVENT_ASYNC_TERMINATION, SL_RESULT_SUCCESS, SL_OBJECT_STATE_REALIZED, 0);
    return 0;
}

static SLresult o_realize(SLObjectItf self, SLboolean async) {
    struct Obj *o = O(self);
    o->realized = 1;
    if (async && o->ocb) {
        pthread_t t;
        pthread_create(&t, 0, realize_thread, o);
        pthread_detach(t);
    }
    return SL_RESULT_SUCCESS;
}
static SLresult o_getstate(SLObjectItf self, SLuint32 *p) { *p = O(self)->realized ? SL_OBJECT_STATE_REALIZED : 1; return 0; }
static SLresult o_getinterface(SLObjectItf self, const SLInterfaceID iid, void *out) {
    struct Obj *o = O(self);
    void **p = out;
    if (!o->realized) return 7; /* SL_RESULT_PRECONDITIONS_VIOLATED */
    if (o->kind == K_ENGINE && iid == SL_IID_ENGINE) *p = &o->engine;
    else if (o->kind == K_MIX && iid == SL_IID_ENVIRONMENTALREVERB) *p = &o->reverb;
    else if (o->kind == K_PLAYER && iid == SL_IID_PLAY) *p = &o->play;
    else if (o->kind == K_PLAYER && iid == SL_IID_VOLUME) *p = &o->volume;
    else if (o->kind == K_PLAYER && iid == SL_IID_EFFECTSEND) *p = &o->send;
    /* como en Android: las dos colas son la misma interfaz */
    else if (o->kind == K_PLAYER && (iid == SL_IID_ANDROIDSIMPLEBUFFERQUEUE || iid == SL_IID_BUFFERQUEUE)) *p = &o->bq;
    else if (o->kind == K_PLAYER && iid == SL_IID_SEEK) *p = &o->volume; /* interfaz del host sin firma en el guest: se usa para probar el rechazo */
    else return SL_RESULT_FEATURE_UNSUPPORTED;
    return SL_RESULT_SUCCESS;
}
static SLresult o_regcb(SLObjectItf self, slObjectCallback cb, void *ctx) { O(self)->ocb = cb; O(self)->octx = ctx; return 0; }
static void stop_player(struct Obj *o);
static void o_destroy(SLObjectItf self) {
    struct Obj *o = O(self);
    if (o->kind == K_PLAYER) stop_player(o);
    for (int i = 0; i < 64; i++)
        if (all_objs[i] == o) all_objs[i] = 0;
    atomic_fetch_sub(&live_objs, 1);
    free(o);
}
static const struct SLObjectItf_ obj_vt = {o_realize, 0, o_getstate, o_getinterface, o_regcb, 0, o_destroy, 0, 0, 0};

static SLresult e_create_mix(SLEngineItf self, SLObjectItf *pMix, SLuint32 n, const SLInterfaceID *ids, const SLboolean *req) {
    if (n != 1 || ids[0] != SL_IID_ENVIRONMENTALREVERB || req[0] != SL_BOOLEAN_FALSE) return SL_RESULT_PARAMETER_INVALID;
    *pMix = (SLObjectItf)&new_obj(K_MIX)->obj;
    return 0;
}
static SLresult e_create_player(SLEngineItf self, SLObjectItf *pPlayer, SLDataSource *src, SLDataSink *snk, SLuint32 n, const SLInterfaceID *ids, const SLboolean *req) {
    *pPlayer = 0;
    SLDataLocator_AndroidSimpleBufferQueue *q = src->pLocator;
    SLDataFormat_PCM *f = src->pFormat;
    SLDataLocator_OutputMix *m = snk->pLocator;
    if (q->locatorType != SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE || q->numBuffers != 2) return 100;
    if (f->formatType != SL_DATAFORMAT_PCM || f->samplesPerSec != 48000000 || f->bitsPerSample != 16) return 101;
    /* el localizador lleva el objeto REAL del host (no el proxy del guest) */
    if (m->locatorType != SL_DATALOCATOR_OUTPUTMIX || !is_obj(m->outputMix, K_MIX)) return 102;
    if (n != 4 || ids[0] != SL_IID_ANDROIDSIMPLEBUFFERQUEUE || ids[3] != SL_IID_EFFECTSEND || req[0] != SL_BOOLEAN_TRUE) return 103;
    struct Obj *o = new_obj(K_PLAYER);
    o->mix = ((struct Itf *)m->outputMix)->o;
    o->state = SL_PLAYSTATE_STOPPED;
    *pPlayer = (SLObjectItf)&o->obj;
    return 0;
}
static SLresult e_query(SLEngineItf self, SLuint32 id, SLuint32 *n) { *n = id + 1; return 0; }
static const struct SLEngineItf_ engine_vt = {.CreateAudioPlayer = e_create_player, .CreateOutputMix = e_create_mix, .QueryNumSupportedInterfaces = e_query};

/* Hilo de audio: consume los buffers encolados y llama al callback de la cola por cada uno (como AudioTrack). */
static void *audio_thread(void *a) {
    struct Obj *o = a;
    while (1) {
        pthread_mutex_lock(&o->mu);
        int playing = o->state == SL_PLAYSTATE_PLAYING;
        const void *buf = 0;
        SLuint32 sz = 0;
        if (playing && o->qhead != o->qtail) {
            buf = o->queue[o->qhead % 16];
            sz = o->qsize[o->qhead % 16];
            o->qhead++;
        }
        pthread_mutex_unlock(&o->mu);
        if (!playing) break;
        if (!buf) { usleep(100); continue; }
        const SLint16 *s = buf;
        long acc = 0;
        for (SLuint32 i = 0; i < sz / 2; i++) acc += s[i];
        o->sum += acc;
        o->consumed++;
        if (o->bqcb) o->bqcb((SLAndroidSimpleBufferQueueItf)&o->bq, o->bqctx);
    }
    return 0;
}
static void stop_player(struct Obj *o) {
    pthread_mutex_lock(&o->mu);
    o->state = SL_PLAYSTATE_STOPPED;
    pthread_mutex_unlock(&o->mu);
    if (o->th_live) pthread_join(o->th, 0);
    o->th_live = 0;
}
static SLresult p_setstate(SLPlayItf self, SLuint32 st) {
    struct Obj *o = O(self);
    if (st == SL_PLAYSTATE_PLAYING && o->state != SL_PLAYSTATE_PLAYING) {
        o->state = st;
        /* dentro de la llamada: el guest recibe el evento antes de que vuelva SetPlayState */
        if (o->pcb && (o->mask & SL_PLAYEVENT_HEADMOVING)) o->pcb((SLPlayItf)&o->play, o->pctx, SL_PLAYEVENT_HEADMOVING);
        o->th_live = 1;
        pthread_create(&o->th, 0, audio_thread, o);
    } else if (st != SL_PLAYSTATE_PLAYING) {
        stop_player(o);
        o->state = st;
    }
    return 0;
}
static SLresult p_getstate(SLPlayItf self, SLuint32 *p) { *p = O(self)->state; return 0; }
static SLresult p_regcb(SLPlayItf self, slPlayCallback cb, void *ctx) { O(self)->pcb = cb; O(self)->pctx = ctx; return 0; }
static SLresult p_mask(SLPlayItf self, SLuint32 m) { O(self)->mask = m; return 0; }
static const struct SLPlayItf_ play_vt = {.SetPlayState = p_setstate, .GetPlayState = p_getstate, .RegisterCallback = p_regcb, .SetCallbackEventsMask = p_mask};

static SLresult v_set(SLVolumeItf self, SLmillibel l) { O(self)->level = l; return l == -3000 ? 0 : SL_RESULT_PARAMETER_INVALID; }
static SLresult v_get(SLVolumeItf self, SLmillibel *l) { *l = O(self)->level; return 0; }
static SLresult v_mute(SLVolumeItf self, SLboolean m) { O(self)->mute = m; return 0; }
static SLresult v_getmute(SLVolumeItf self, SLboolean *m) { *m = O(self)->mute; return 0; }
static SLresult v_stereo(SLVolumeItf self, SLpermille p) { O(self)->stereo = p; return p == -500 ? 0 : SL_RESULT_PARAMETER_INVALID; }
static SLresult v_getstereo(SLVolumeItf self, SLpermille *p) { *p = O(self)->stereo; return 0; }
static const struct SLVolumeItf_ volume_vt = {.SetVolumeLevel = v_set, .GetVolumeLevel = v_get, .SetMute = v_mute, .GetMute = v_getmute, .SetStereoPosition = v_stereo, .GetStereoPosition = v_getstereo};

static SLresult q_enqueue(SLAndroidSimpleBufferQueueItf self, const void *b, SLuint32 sz) {
    struct Obj *o = O(self);
    pthread_mutex_lock(&o->mu);
    int full = o->qtail - o->qhead >= 2;
    if (!full) {
        o->queue[o->qtail % 16] = b;
        o->qsize[o->qtail % 16] = sz;
        o->qtail++;
    }
    pthread_mutex_unlock(&o->mu);
    return full ? 14 /* SL_RESULT_BUFFER_INSUFFICIENT */ : 0;
}
static SLresult q_clear(SLAndroidSimpleBufferQueueItf self) { struct Obj *o = O(self); pthread_mutex_lock(&o->mu); o->qhead = o->qtail; pthread_mutex_unlock(&o->mu); return 0; }
static SLresult q_state(SLAndroidSimpleBufferQueueItf self, SLAndroidSimpleBufferQueueState *s) { struct Obj *o = O(self); s->count = o->qtail - o->qhead; s->index = o->consumed; return 0; }
static SLresult q_regcb(SLAndroidSimpleBufferQueueItf self, slAndroidSimpleBufferQueueCallback cb, void *ctx) { O(self)->bqcb = cb; O(self)->bqctx = ctx; return 0; }
static const struct SLAndroidSimpleBufferQueueItf_ bq_vt = {q_enqueue, q_clear, q_state, q_regcb};

/* pAuxEffect (const void*) debe ser la interfaz REAL del reverb del output mix con el que se creo el reproductor */
static SLresult s_enable(SLEffectSendItf self, const void *aux, SLboolean en, SLmillibel lvl) {
    struct Obj *o = O(self);
    return (aux == &o->mix->reverb && en == 1 && lvl == -100) ? 0 : SL_RESULT_PARAMETER_INVALID;
}
static SLresult s_isenabled(SLEffectSendItf self, const void *aux, SLboolean *en) { *en = aux == &O(self)->mix->reverb; return 0; }
static const struct SLEffectSendItf_ send_vt = {.EnableEffectSend = s_enable, .IsEnabled = s_isenabled};
static SLresult r_set(SLEnvironmentalReverbItf self, SLmillibel l) { O(self)->level = l; return 0; }
static SLresult r_get(SLEnvironmentalReverbItf self, SLmillibel *l) { *l = O(self)->level; return 0; }
static const struct SLEnvironmentalReverbItf_ reverb_vt = {.SetRoomLevel = r_set, .GetRoomLevel = r_get};

SLresult slCreateEngine(SLObjectItf *pEngine, SLuint32 numOptions, const SLEngineOption *opt, SLuint32 n, const SLInterfaceID *ids, const SLboolean *req) {
    if (numOptions != 1 || opt[0].feature != 1 || opt[0].data != SL_BOOLEAN_TRUE) return SL_RESULT_PARAMETER_INVALID;
    *pEngine = (SLObjectItf)&new_obj(K_ENGINE)->obj;
    return 0;
}
SLresult slQueryNumSupportedEngineInterfaces(SLuint32 *p) { *p = 1000 + live_objs; return 0; }

/* ------------------------------------------------------------------ OpenMAX AL */

static const struct SLInterfaceID_ xiid_engine = {0x20, 1, 2, 3, {9, 9, 9, 9, 9, 9}}, xiid_play = {0x21, 1, 2, 3, {9, 9, 9, 9, 9, 9}};
const XAInterfaceID XA_IID_ENGINE = &xiid_engine, XA_IID_PLAY = &xiid_play;
struct XObj {
    struct Itf obj, engine, play;
    int kind;
    xaPlayCallback cb;
    void *ctx;
};
static const struct XAObjectItf_ xobj_vt;
static const struct XAEngineItf_ xengine_vt;
static const struct XAPlayItf_ xplay_vt;
static struct XObj *xall[16];
static struct XObj *new_xobj(int kind) {
    struct XObj *o = calloc(1, sizeof *o);
    o->kind = kind;
    o->obj = (struct Itf){&xobj_vt, (struct Obj *)o};
    o->engine = (struct Itf){&xengine_vt, (struct Obj *)o};
    o->play = (struct Itf){&xplay_vt, (struct Obj *)o};
    for (int i = 0; i < 16; i++)
        if (!xall[i]) { xall[i] = o; break; }
    atomic_fetch_add(&live_objs, 1);
    return o;
}
#define XO(self) ((struct XObj *)((struct Itf *)(self))->o)
static XAresult x_realize(XAObjectItf self, XAboolean a) { return 0; }
static XAresult x_getinterface(XAObjectItf self, const XAInterfaceID iid, void *out) {
    struct XObj *o = XO(self);
    if (o->kind == K_XENGINE && iid == XA_IID_ENGINE) *(void **)out = &o->engine;
    else if (o->kind == K_XPLAYER && iid == XA_IID_PLAY) *(void **)out = &o->play;
    else return SL_RESULT_FEATURE_UNSUPPORTED;
    return 0;
}
static void x_destroy(XAObjectItf self) {
    struct XObj *o = XO(self);
    for (int i = 0; i < 16; i++)
        if (xall[i] == o) xall[i] = 0;
    atomic_fetch_sub(&live_objs, 1);
    free(o);
}
static const struct XAObjectItf_ xobj_vt = {.Realize = x_realize, .GetInterface = x_getinterface, .Destroy = x_destroy};
static XAresult x_create_mix(XAEngineItf self, XAObjectItf *pMix, XAuint32 n, const XAInterfaceID *ids, const XAboolean *req) {
    *pMix = (XAObjectItf)&new_xobj(K_XMIX)->obj;
    return 0;
}
/* 11 argumentos: en x86-64 los 5 ultimos van en la pila */
static XAresult x_create_player(XAEngineItf self, XAObjectItf *pPlayer, XADataSource *src, XADataSource *bank, XADataSink *audio, XADataSink *video, XADataSink *vibra, XADataSink *led, XAuint32 n,
                                const XAInterfaceID *ids, const XAboolean *req) {
    *pPlayer = 0;
    if (src == 0 || bank != 0 || vibra != 0 || led != 0) return 110;
    SLDataLocator_OutputMix *m = audio->pLocator;
    int mix_ok = 0;
    for (int i = 0; i < 16; i++)
        if (xall[i] && xall[i]->kind == K_XMIX && (void *)&xall[i]->obj == (void *)m->outputMix) mix_ok = 1;
    if (m->locatorType != SL_DATALOCATOR_OUTPUTMIX || !mix_ok) return 111;
    XADataLocator_NativeDisplay *d = video->pLocator;
    if (d->locatorType != XA_DATALOCATOR_NATIVEDISPLAY || d->hWindow != (void *)0x1234 || d->hDisplay != 0) return 112;
    if (n != 1 || ids[0] != XA_IID_PLAY || req[0] != 1) return 113;
    *pPlayer = (XAObjectItf)&new_xobj(K_XPLAYER)->obj;
    return 0;
}
static const struct XAEngineItf_ xengine_vt = {.CreateMediaPlayer = x_create_player, .CreateOutputMix = x_create_mix};
static XAresult xp_setstate(XAPlayItf self, XAuint32 st) {
    struct XObj *o = XO(self);
    if (o->cb) o->cb((XAPlayItf)&o->play, o->ctx, XA_PLAYEVENT_HEADATEND);
    return 0;
}
static XAresult xp_regcb(XAPlayItf self, xaPlayCallback cb, void *ctx) { XO(self)->cb = cb; XO(self)->ctx = ctx; return 0; }
static const struct XAPlayItf_ xplay_vt = {.SetPlayState = xp_setstate, .RegisterCallback = xp_regcb};
XAresult xaCreateEngine(XAObjectItf *pEngine, XAuint32 numOptions, const void *opt, XAuint32 n, const XAInterfaceID *ids, const XAboolean *req) {
    *pEngine = (XAObjectItf)&new_xobj(K_XENGINE)->obj;
    return 0;
}

/* ------------------------------------------------------------------ AAudio */

struct AAudioStreamBuilderStruct {
    int fmt, ch;
    AAudioStream_dataCallback dcb;
    AAudioStream_errorCallback ecb;
    void *dud, *eud;
};
struct AAudioStreamStruct {
    struct AAudioStreamBuilderStruct b;
    pthread_t th;
    atomic_int run;
    float buf[2 * 192];
};
aaudio_result_t AAudio_createStreamBuilder(AAudioStreamBuilder **b) { *b = calloc(1, sizeof **b); return 0; }
void AAudioStreamBuilder_setFormat(AAudioStreamBuilder *b, SLint32 f) { b->fmt = f; }
void AAudioStreamBuilder_setChannelCount(AAudioStreamBuilder *b, SLint32 c) { b->ch = c; }
void AAudioStreamBuilder_setDataCallback(AAudioStreamBuilder *b, AAudioStream_dataCallback cb, void *ud) { b->dcb = cb; b->dud = ud; }
void AAudioStreamBuilder_setErrorCallback(AAudioStreamBuilder *b, AAudioStream_errorCallback cb, void *ud) { b->ecb = cb; b->eud = ud; }
aaudio_result_t AAudioStreamBuilder_openStream(AAudioStreamBuilder *b, AAudioStream **s) {
    if (b->fmt != AAUDIO_FORMAT_PCM_FLOAT || b->ch != 2) return -898;
    *s = calloc(1, sizeof **s);
    (*s)->b = *b;
    return 0;
}
aaudio_result_t AAudioStreamBuilder_delete(AAudioStreamBuilder *b) { free(b); return 0; }
/* Hilo de audio (como el de AAudio): pide 192 tramas por llamada hasta que el guest devuelve STOP, comprobando que
 * escribio las muestras en coma flotante esperadas; un error de datos se notifica por el callback de error. */
static void *aaudio_thread(void *a) {
    AAudioStream *s = a;
    for (long k = 0; s->run; k++) {
        for (int i = 0; i < 2 * 192; i++) s->buf[i] = -1.0f;
        int r = s->b.dcb(s, s->b.dud, s->buf, 192);
        int ok = 1;
        for (int i = 0; i < 192; i++)
            if (s->buf[2 * i] != (float)(k % 7) * 0.25f + (float)i / 256.0f || s->buf[2 * i + 1] != -s->buf[2 * i]) ok = 0;
        if (!ok) { s->b.ecb(s, s->b.eud, -900); break; }
        if (r == AAUDIO_CALLBACK_RESULT_STOP) break;
    }
    if (s->b.ecb) s->b.ecb(s, s->b.eud, AAUDIO_ERROR_DISCONNECTED);
    return 0;
}
aaudio_result_t AAudioStream_requestStart(AAudioStream *s) {
    s->run = 1;
    pthread_create(&s->th, 0, aaudio_thread, s);
    return 0;
}
aaudio_result_t AAudioStream_requestStop(AAudioStream *s) {
    s->run = 0;
    pthread_join(s->th, 0);
    return 0;
}
aaudio_result_t AAudioStream_close(AAudioStream *s) { free(s); return 0; }

/* ------------------------------------------------------------------ AMediaCodec asincrono */

struct AMediaCodec {
    AMediaCodecOnAsyncNotifyCallback cb;
    void *ud;
    pthread_t th;
};
AMediaCodec *AMediaCodec_createDecoderByType(const char *mime) { return strcmp(mime, "audio/mp4a-latm") ? 0 : calloc(1, sizeof(AMediaCodec)); }
/* la estructura llega POR VALOR (en la pila en x86-64) y el host guarda su copia */
media_status_t AMediaCodec_setAsyncNotifyCallback(AMediaCodec *c, AMediaCodecOnAsyncNotifyCallback cb, void *ud) {
    c->cb = cb;
    c->ud = ud;
    return 0;
}
static void *codec_thread(void *a) {
    AMediaCodec *c = a;
    static AMediaCodecBufferInfo info = {11, 22, 33000000000LL, 4};
    c->cb.onAsyncInputAvailable(c, c->ud, 7);
    c->cb.onAsyncOutputAvailable(c, c->ud, 3, &info);
    c->cb.onAsyncFormatChanged(c, c->ud, (AMediaFormat *)0x5555);
    c->cb.onAsyncError(c, c->ud, -10000, 2, "detalle");
    return 0;
}
media_status_t AMediaCodec_start(AMediaCodec *c) { return pthread_create(&c->th, 0, codec_thread, c) ? -10000 : 0; }
media_status_t AMediaCodec_stop(AMediaCodec *c) { pthread_join(c->th, 0); return 0; }
media_status_t AMediaCodec_delete(AMediaCodec *c) { free(c); return 0; }

/* ------------------------------------------------------------------ AMediaDataSource */

struct AMediaDataSource {
    void *ud;
    AMediaDataSourceReadAt read;
};
struct AMediaExtractor {
    int x;
};
AMediaDataSource *AMediaDataSource_new(void) { return calloc(1, sizeof(AMediaDataSource)); }
void AMediaDataSource_setUserdata(AMediaDataSource *d, void *ud) { d->ud = ud; }
void AMediaDataSource_setReadAt(AMediaDataSource *d, AMediaDataSourceReadAt r) { d->read = r; }
void AMediaDataSource_delete(AMediaDataSource *d) { free(d); }
AMediaExtractor *AMediaExtractor_new(void) { return calloc(1, sizeof(AMediaExtractor)); }
media_status_t AMediaExtractor_setDataSourceCustom(AMediaExtractor *ex, AMediaDataSource *d) {
    unsigned char b[16];
    int64_t n = d->read(d->ud, 1LL << 40, b, sizeof b);
    if (n != 16) return -10001;
    for (int i = 0; i < 16; i++)
        if (b[i] != (unsigned char)(i * 3)) return -10002;
    return 0;
}
media_status_t AMediaExtractor_delete(AMediaExtractor *ex) { free(ex); return 0; }

/* ------------------------------------------------------------------ camara */

struct ACameraManager {
    ACameraManager_AvailabilityCallbacks reg[4];
    int nreg;
};
struct ACameraDevice {
    ACameraDevice_StateCallbacks cb;
    pthread_t th;
};
ACameraManager *ACameraManager_create(void) { return calloc(1, sizeof(ACameraManager)); }
void ACameraManager_delete(ACameraManager *m) { free(m); }
static void *avail_thread(void *a) {
    ACameraManager_AvailabilityCallbacks *c = a;
    c->onCameraAvailable(c->context, "0");
    c->onCameraUnavailable(c->context, "1");
    return 0;
}
/* Como ACameraManager: copia la estructura; la baja busca una copia IGUAL (mismos punteros y contexto) */
camera_status_t ACameraManager_registerAvailabilityCallback(ACameraManager *m, const ACameraManager_AvailabilityCallbacks *cb) {
    if (m->nreg == 4) return -10000;
    m->reg[m->nreg] = *cb;
    pthread_t t;
    pthread_create(&t, 0, avail_thread, &m->reg[m->nreg]);
    pthread_join(t, 0);
    m->nreg++;
    return 0;
}
camera_status_t ACameraManager_unregisterAvailabilityCallback(ACameraManager *m, const ACameraManager_AvailabilityCallbacks *cb) {
    for (int i = 0; i < m->nreg; i++)
        if (!memcmp(&m->reg[i], cb, sizeof *cb)) {
            m->reg[i] = m->reg[--m->nreg];
            return 0;
        }
    return -10001;
}
static void *dev_thread(void *a) {
    ACameraDevice *d = a;
    d->cb.onError(d->cb.context, d, 3);
    d->cb.onDisconnected(d->cb.context, d);
    return 0;
}
camera_status_t ACameraManager_openCamera(ACameraManager *m, const char *id, ACameraDevice_StateCallbacks *cb, ACameraDevice **out) {
    ACameraDevice *d = calloc(1, sizeof *d);
    d->cb = *cb;
    *out = d;
    pthread_create(&d->th, 0, dev_thread, d);
    return 0;
}
camera_status_t ACameraDevice_close(ACameraDevice *d) { pthread_join(d->th, 0); free(d); return 0; }

/* ------------------------------------------------------------------ AImageReader */

struct AImageReader {
    AImageReader_ImageListener l;
};
media_status_t AImageReader_new(SLint32 w, SLint32 h, SLint32 f, SLint32 n, AImageReader **r) { *r = calloc(1, sizeof **r); return 0; }
static void *image_thread(void *a) {
    AImageReader *r = a;
    r->l.onImageAvailable(r->l.context, r);
    return 0;
}
media_status_t AImageReader_setImageListener(AImageReader *r, AImageReader_ImageListener *l) {
    r->l = *l; /* copia, como FrameListener::setImageListener */
    pthread_t t;
    pthread_create(&t, 0, image_thread, r);
    pthread_join(t, 0);
    return 0;
}
void AImageReader_delete(AImageReader *r) { free(r); }

/* ------------------------------------------------------------------ AChoreographer */

struct AChoreographer {
    AChoreographer_refreshRateCallback rr[4];
    void *rrd[4];
};
static struct AChoreographer choreo;
AChoreographer *AChoreographer_getInstance(void) { return &choreo; }
struct Frame {
    AChoreographer_frameCallback64 cb;
    void *d;
};
static void *frame_thread(void *a) {
    struct Frame *f = a;
    f->cb(123456789012345LL, f->d);
    free(f);
    return 0;
}
void AChoreographer_postFrameCallback64(AChoreographer *c, AChoreographer_frameCallback64 cb, void *d) {
    struct Frame *f = malloc(sizeof *f);
    f->cb = cb;
    f->d = d;
    pthread_t t;
    pthread_create(&t, 0, frame_thread, f);
    pthread_join(t, 0);
}
void AChoreographer_registerRefreshRateCallback(AChoreographer *c, AChoreographer_refreshRateCallback cb, void *d) {
    for (int i = 0; i < 4; i++)
        if (!c->rr[i]) { c->rr[i] = cb; c->rrd[i] = d; cb(16666666, d); return; }
}
/* la baja compara el puntero: el trampolin de la misma funcion guest debe ser el mismo */
void AChoreographer_unregisterRefreshRateCallback(AChoreographer *c, AChoreographer_refreshRateCallback cb, void *d) {
    for (int i = 0; i < 4; i++)
        if (c->rr[i] == cb && c->rrd[i] == d) { c->rr[i] = 0; cb(-1, d); return; }
}

/* ------------------------------------------------------------------ comprobacion comun */

/* `p` esta en una region ejecutable del proceso? (el codigo guest nunca se mapea ejecutable: un callback que llegue
 * sin convertir al host no lo esta) */
static int host_exec(const void *p) {
    FILE *f = fopen("/proc/self/maps", "r");
    if (!f) return 0;
    char line[512];
    int ok = 0;
    while (fgets(line, sizeof line, f)) {
        unsigned long a, b;
        char perm[8];
        if (sscanf(line, "%lx-%lx %7s", &a, &b, perm) == 3 && (unsigned long)p >= a && (unsigned long)p < b) {
            ok = perm[2] == 'x';
            break;
        }
    }
    fclose(f);
    return ok;
}

/* ------------------------------------------------------------------ Vulkan */

static atomic_int vk_err;
#define VK_EXPECT(c) do { if (!(c)) { fprintf(stderr, "mock vulkan: falla %s (linea %d)\n", #c, __LINE__); atomic_fetch_add(&vk_err, 1); } } while (0)

struct VkInstance_T {
    VkAllocationCallbacks a;
    int has_a;
    PFN_vkDebugUtilsMessengerCallbackEXT mcb;
    void *mud;
};
struct VkDevice_T {
    int unused;
};

static void check_alloc(const VkAllocationCallbacks *a) {
    VK_EXPECT(host_exec(a->pfnAllocation) && host_exec(a->pfnReallocation) && host_exec(a->pfnFree));
    VK_EXPECT(!a->pfnInternalAllocation || host_exec(a->pfnInternalAllocation));
    VK_EXPECT(!a->pfnInternalFree || host_exec(a->pfnInternalFree));
}

struct DbgCall {
    PFN_vkDebugUtilsMessengerCallbackEXT cb;
    void *ud;
    const char *msg;
    VkBool32 ret;
};
static void *dbg_thread(void *p) {
    struct DbgCall *d = p;
    VkDebugUtilsMessengerCallbackDataEXT data = {VK_STRUCTURE_TYPE_DEBUG_UTILS_MESSENGER_CALLBACK_DATA_EXT, 0, 0, "id", 42, d->msg, 0, 0, 0, 0, 0, 0};
    d->ret = d->cb(0x100 /* WARNING */, 0x2 /* VALIDATION */, &data, d->ud);
    return 0;
}
/* el callback del mensajero desde un hilo propio del host (como una capa de validacion) */
static VkBool32 call_dbg(PFN_vkDebugUtilsMessengerCallbackEXT cb, void *ud, const char *msg) {
    struct DbgCall d = {cb, ud, msg, 7};
    pthread_t t;
    pthread_create(&t, 0, dbg_thread, &d);
    pthread_join(t, 0);
    return d.ret;
}

static VkResult m_create_messenger(VkInstance i, const VkDebugUtilsMessengerCreateInfoEXT *ci, const VkAllocationCallbacks *a, VkDebugUtilsMessengerEXT *out) {
    VK_EXPECT(ci->sType == VK_STRUCTURE_TYPE_DEBUG_UTILS_MESSENGER_CREATE_INFO_EXT && host_exec(ci->pfnUserCallback));
    if (a) check_alloc(a);
    i->mcb = ci->pfnUserCallback; /* se guarda: la estructura de entrada no */
    i->mud = ci->pUserData;
    *out = 0x5151;
    return 0;
}
static void m_destroy_messenger(VkInstance i, VkDebugUtilsMessengerEXT m, const VkAllocationCallbacks *a) {
    VK_EXPECT(m == 0x5151);
    if (a) { check_alloc(a); a->pfnFree(a->pUserData, 0); }
    i->mcb = 0;
}
static void m_submit(VkInstance i, SLuint32 sev, VkFlags types, const VkDebugUtilsMessengerCallbackDataEXT *d) {
    if (i->mcb) VK_EXPECT(call_dbg(i->mcb, i->mud, d->pMessage) == 0);
}

VkResult vkCreateInstance(const VkInstanceCreateInfo *ci, const VkAllocationCallbacks *a, VkInstance *out) {
    VK_EXPECT(ci->sType == VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO);
    VkInstance i = calloc(1, sizeof *i);
    for (const VkBaseInStructure *s = ci->pNext; s; s = s->pNext) {
        if (s->sType == VK_STRUCTURE_TYPE_DEBUG_UTILS_MESSENGER_CREATE_INFO_EXT) {
            const VkDebugUtilsMessengerCreateInfoEXT *m = (const void *)s;
            VK_EXPECT(host_exec(m->pfnUserCallback));
            VK_EXPECT(call_dbg(m->pfnUserCallback, m->pUserData, "durante vkCreateInstance") == 0);
        } else if (s->sType == (VkStructureType)0x7ffe0001) {
            /* sType desconocido para el puente: llega la estructura del guest (no una copia), con su pNext cambiado */
            VK_EXPECT(((const void *const *)s)[2] == s);
        } else if (s->sType == VK_STRUCTURE_TYPE_DIRECT_DRIVER_LOADING_LIST_LUNARG) {
            /* anidada: la lista y su arreglo llegan copiados, con el contenido del guest */
            const VkDirectDriverLoadingListLUNARG *l = (const void *)s;
            VK_EXPECT(l->driverCount == 1 && l->pDrivers[0].sType == VK_STRUCTURE_TYPE_DIRECT_DRIVER_LOADING_INFO_LUNARG && l->pDrivers[0].flags == 0x33);
        } else if (s->sType == VK_STRUCTURE_TYPE_DEBUG_REPORT_CALLBACK_CREATE_INFO_EXT) {
            const VkDebugReportCallbackCreateInfoEXT *r = (const void *)s;
            VK_EXPECT(host_exec(r->pfnCallback));
            /* 8 argumentos: dos van en la pila del host y en registros del guest */
            VK_EXPECT(r->pfnCallback(0x4, 3, 0x123456789abcdefULL, 77, -5, "capa", "informe", r->pUserData) == 1);
        }
    }
    if (a) {
        check_alloc(a);
        i->a = *a; /* el cargador copia pAllocator y lo usa despues */
        i->has_a = 1;
        void *p = a->pfnAllocation(a->pUserData, 64, 16, 1);
        VK_EXPECT(p && ((unsigned long)p & 15) == 0);
        if (a->pfnInternalAllocation) a->pfnInternalAllocation(a->pUserData, 4096, 0, 1);
        p = a->pfnReallocation(a->pUserData, p, 128, 16, 1);
        VK_EXPECT(p != 0);
        a->pfnFree(a->pUserData, p);
        if (a->pfnInternalFree) a->pfnInternalFree(a->pUserData, 4096, 0, 1);
    }
    *out = i;
    return 0;
}
static void *alloc_thread(void *p) {
    VkInstance i = p;
    void *m = i->a.pfnAllocation(i->a.pUserData, 32, 8, 1);
    i->a.pfnFree(i->a.pUserData, m);
    return 0;
}
void vkDestroyInstance(VkInstance i, const VkAllocationCallbacks *a) {
    if (!i) return;
    VK_EXPECT(!a == !i->has_a);
    if (a) {
        /* "compatible" con el de la creacion: las mismas funciones */
        VK_EXPECT(a->pfnAllocation == i->a.pfnAllocation && a->pfnFree == i->a.pfnFree && a->pUserData == i->a.pUserData);
        /* la copia guardada se usa despues de volver, desde otro hilo del host */
        pthread_t t;
        pthread_create(&t, 0, alloc_thread, i);
        pthread_join(t, 0);
    }
    free(i);
}
PFN_vkVoidFunction vkGetInstanceProcAddr(VkInstance i, const char *name) {
    if (!strcmp(name, "vkCreateDebugUtilsMessengerEXT")) return (PFN_vkVoidFunction)m_create_messenger;
    if (!strcmp(name, "vkDestroyDebugUtilsMessengerEXT")) return (PFN_vkVoidFunction)m_destroy_messenger;
    if (!strcmp(name, "vkSubmitDebugUtilsMessageEXT")) return (PFN_vkVoidFunction)m_submit;
    return 0;
}
/* canal de consulta: fallos vistos por el host simulado */
VkResult vkEnumerateInstanceVersion(SLuint32 *v) {
    *v = atomic_load(&vk_err);
    return 0;
}
struct MemRep {
    PFN_vkDeviceMemoryReportCallbackEXT cb;
    void *ud;
};
static void *memrep_thread(void *p) {
    struct MemRep *m = p;
    VkDeviceMemoryReportCallbackDataEXT d = {0, 0, 0, 1, 99, 4096, 9, 0xabc, 2};
    m->cb(&d, m->ud);
    return 0;
}
VkResult vkCreateDevice(VkPhysicalDevice pd, const VkDeviceCreateInfo *ci, const VkAllocationCallbacks *a, VkDevice *out) {
    VK_EXPECT(ci->sType == VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO);
    int seen = 0;
    for (const VkBaseInStructure *s = ci->pNext; s; s = s->pNext) {
        if (s->sType == VK_STRUCTURE_TYPE_DEVICE_DEVICE_MEMORY_REPORT_CREATE_INFO_EXT) {
            const VkDeviceDeviceMemoryReportCreateInfoEXT *r = (const void *)s;
            VK_EXPECT(host_exec(r->pfnUserCallback));
            struct MemRep m = {r->pfnUserCallback, r->pUserData};
            pthread_t t;
            pthread_create(&t, 0, memrep_thread, &m);
            pthread_join(t, 0);
            seen++;
        } else if (s->sType == VK_STRUCTURE_TYPE_VALIDATION_FEATURES_EXT) {
            const VkValidationFeaturesEXT *v = (const void *)s;
            VK_EXPECT(v->enabledValidationFeatureCount == 2 && v->pEnabledValidationFeatures[1] == 3);
            seen++;
        }
    }
    VK_EXPECT(seen == 2);
    if (a) check_alloc(a);
    *out = calloc(1, sizeof **out);
    return 0;
}
void vkDestroyDevice(VkDevice d, const VkAllocationCallbacks *a) {
    if (a) check_alloc(a);
    free(d);
}
VkResult vkCreateBuffer(VkDevice d, const VkBufferCreateInfo *ci, const VkAllocationCallbacks *a, VkBuffer *out) {
    VK_EXPECT(ci->sType == VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO && ci->pNext == 0);
    if (!a) { *out = (VkBuffer)(unsigned long)malloc(ci->size); return 0; }
    check_alloc(a);
    void *p = a->pfnAllocation(a->pUserData, ci->size, 64, 1);
    VK_EXPECT(p && ((unsigned long)p & 63) == 0);
    *out = (VkBuffer)(unsigned long)p;
    return p ? 0 : -1;
}
void vkDestroyBuffer(VkDevice d, VkBuffer b, const VkAllocationCallbacks *a) {
    if (!a) { free((void *)(unsigned long)b); return; }
    check_alloc(a);
    a->pfnFree(a->pUserData, (void *)(unsigned long)b);
}

/* ------------------------------------------------------------------ liblog (API 30) */

static __android_logger_function log_logger;
static __android_aborter_function log_aborter;
static atomic_int logd_n, log_err;
void __android_log_logd_logger(const struct __android_log_message *m) {
    atomic_fetch_add(&logd_n, 1);
    fprintf(stderr, "[logd] %d %s: %s\n", m->priority, m->tag ? m->tag : "", m->message);
}
void __android_log_stderr_logger(const struct __android_log_message *m) { fprintf(stderr, "%s: %s\n", m->tag ? m->tag : "", m->message); }
void __android_log_set_logger(__android_logger_function f) {
    if (f && !host_exec(f)) atomic_fetch_add(&log_err, 1);
    log_logger = f;
}
void __android_log_write_log_message(struct __android_log_message *m) {
    if (m->struct_size != sizeof *m) { atomic_fetch_add(&log_err, 1); return; }
    (log_logger ? log_logger : __android_log_logd_logger)(m);
}
static void *log_thread(void *p) {
    __android_log_write_log_message(p);
    return 0;
}
static void *abort_thread(void *p) {
    __android_log_call_aborter(p);
    return 0;
}
/* bufID 99: el mensaje se escribe desde un hilo propio del host; 98: el abortador, desde un hilo propio */
int __android_log_buf_write(int buf, int prio, const char *tag, const char *text) {
    struct __android_log_message m = {sizeof m, buf, prio, tag, 0, 0, text};
    pthread_t t;
    if (buf == 98) {
        pthread_create(&t, 0, abort_thread, (void *)text);
        pthread_join(t, 0);
    } else if (buf == 99) {
        pthread_create(&t, 0, log_thread, &m);
        pthread_join(t, 0);
    } else {
        __android_log_write_log_message(&m);
    }
    return 1;
}
int __android_log_write(int prio, const char *tag, const char *text) { return __android_log_buf_write(0, prio, tag, text); }
void __android_log_set_aborter(__android_aborter_function f) {
    if (f && !host_exec(f)) atomic_fetch_add(&log_err, 1);
    log_aborter = f;
}
void __android_log_call_aborter(const char *msg) {
    if (log_aborter) log_aborter(msg);
    else abort();
}
/* canal de consulta: "logd" -> mensajes que llegaron al registrador por defecto; "err" -> fallos vistos */
int __android_log_is_loggable(int prio, const char *tag, int def) {
    if (!strcmp(tag, "logd")) return atomic_load(&logd_n);
    if (!strcmp(tag, "err")) return atomic_load(&log_err);
    return prio >= def;
}
