/* Subconjunto de las cabeceras publicas del NDK (OpenSL ES, OpenMAX AL, AAudio, NdkMediaCodec, camera2ndk,
 * NdkImageReader, NdkMediaDataSource, AChoreographer) con la misma disposicion: lo comparten la biblioteca guest de
 * prueba (t9, aarch64) y el host simulado (mock_ndk.c, x86-64). Sin dependencias de una libc (el guest se compila con
 * -nostdlib). Las tablas de funciones conservan el orden de las cabeceras; los metodos que las pruebas no usan son
 * `void *`. */
#ifndef NDK_MIN_H
#define NDK_MIN_H

typedef __UINT8_TYPE__ SLuint8;
typedef __INT16_TYPE__ SLint16;
typedef __UINT16_TYPE__ SLuint16;
typedef __INT32_TYPE__ SLint32;
typedef __UINT32_TYPE__ SLuint32;
typedef SLuint32 SLboolean;
typedef SLuint32 SLresult;
typedef SLint16 SLmillibel;
typedef SLuint32 SLmillisecond;
typedef SLuint32 SLmilliHertz;
typedef SLint16 SLpermille;

#define SL_RESULT_SUCCESS 0u
#define SL_RESULT_PARAMETER_INVALID 2u
#define SL_RESULT_FEATURE_UNSUPPORTED 12u
#define SL_BOOLEAN_TRUE 1u
#define SL_BOOLEAN_FALSE 0u
#define SL_PLAYSTATE_STOPPED 1u
#define SL_PLAYSTATE_PAUSED 2u
#define SL_PLAYSTATE_PLAYING 3u
#define SL_PLAYEVENT_HEADATEND 1u
#define SL_PLAYEVENT_HEADMOVING 0x10u
#define SL_OBJECT_EVENT_ASYNC_TERMINATION 2u
#define SL_OBJECT_STATE_REALIZED 2u
#define SL_DATALOCATOR_OUTPUTMIX 4u
#define SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE 0x800007BDu
#define SL_DATAFORMAT_PCM 2u

typedef const struct SLInterfaceID_ {
    SLuint32 time_low;
    SLuint16 time_mid;
    SLuint16 time_hi_and_version;
    SLuint16 clock_seq;
    SLuint8 node[6];
} *SLInterfaceID;

typedef const struct SLObjectItf_ *const *SLObjectItf;
typedef const struct SLEngineItf_ *const *SLEngineItf;
typedef const struct SLPlayItf_ *const *SLPlayItf;
typedef const struct SLVolumeItf_ *const *SLVolumeItf;
typedef const struct SLEffectSendItf_ *const *SLEffectSendItf;
typedef const struct SLEnvironmentalReverbItf_ *const *SLEnvironmentalReverbItf;
typedef const struct SLAndroidSimpleBufferQueueItf_ *const *SLAndroidSimpleBufferQueueItf;

typedef void (*slObjectCallback)(SLObjectItf caller, const void *pContext, SLuint32 event, SLresult result, SLuint32 param, void *pInterface);
typedef void (*slPlayCallback)(SLPlayItf caller, void *pContext, SLuint32 event);
typedef void (*slAndroidSimpleBufferQueueCallback)(SLAndroidSimpleBufferQueueItf caller, void *pContext);

typedef struct SLDataSource_ {
    void *pLocator;
    void *pFormat;
} SLDataSource;
typedef struct SLDataSink_ {
    void *pLocator;
    void *pFormat;
} SLDataSink;
typedef struct SLDataLocator_OutputMix {
    SLuint32 locatorType;
    SLObjectItf outputMix;
} SLDataLocator_OutputMix;
typedef struct SLDataLocator_AndroidSimpleBufferQueue {
    SLuint32 locatorType;
    SLuint32 numBuffers;
} SLDataLocator_AndroidSimpleBufferQueue;
typedef struct SLDataFormat_PCM_ {
    SLuint32 formatType, numChannels, samplesPerSec, bitsPerSample, containerSize, channelMask, endianness;
} SLDataFormat_PCM;
typedef struct SLAndroidSimpleBufferQueueState_ {
    SLuint32 count;
    SLuint32 index;
} SLAndroidSimpleBufferQueueState;
typedef struct SLEngineOption_ {
    SLuint32 feature;
    SLuint32 data;
} SLEngineOption;

struct SLObjectItf_ {
    SLresult (*Realize)(SLObjectItf self, SLboolean async);
    SLresult (*Resume)(SLObjectItf self, SLboolean async);
    SLresult (*GetState)(SLObjectItf self, SLuint32 *pState);
    SLresult (*GetInterface)(SLObjectItf self, const SLInterfaceID iid, void *pInterface);
    SLresult (*RegisterCallback)(SLObjectItf self, slObjectCallback callback, void *pContext);
    void (*AbortAsyncOperation)(SLObjectItf self);
    void (*Destroy)(SLObjectItf self);
    SLresult (*SetPriority)(SLObjectItf self, SLint32 priority, SLboolean preemptable);
    SLresult (*GetPriority)(SLObjectItf self, SLint32 *pPriority, SLboolean *pPreemptable);
    SLresult (*SetLossOfControlInterfaces)(SLObjectItf self, SLint16 numInterfaces, SLInterfaceID *pInterfaceIDs, SLboolean enabled);
};
struct SLEngineItf_ {
    void *CreateLEDDevice, *CreateVibraDevice;
    SLresult (*CreateAudioPlayer)(SLEngineItf self, SLObjectItf *pPlayer, SLDataSource *pAudioSrc, SLDataSink *pAudioSnk, SLuint32 numInterfaces, const SLInterfaceID *pInterfaceIds, const SLboolean *pInterfaceRequired);
    void *CreateAudioRecorder, *CreateMidiPlayer, *CreateListener, *Create3DGroup;
    SLresult (*CreateOutputMix)(SLEngineItf self, SLObjectItf *pMix, SLuint32 numInterfaces, const SLInterfaceID *pInterfaceIds, const SLboolean *pInterfaceRequired);
    void *CreateMetadataExtractor, *CreateExtensionObject;
    SLresult (*QueryNumSupportedInterfaces)(SLEngineItf self, SLuint32 objectID, SLuint32 *pNumSupportedInterfaces);
    void *QuerySupportedInterfaces, *QueryNumSupportedExtensions, *QuerySupportedExtension, *IsExtensionSupported;
};
struct SLPlayItf_ {
    SLresult (*SetPlayState)(SLPlayItf self, SLuint32 state);
    SLresult (*GetPlayState)(SLPlayItf self, SLuint32 *pState);
    void *GetDuration, *GetPosition;
    SLresult (*RegisterCallback)(SLPlayItf self, slPlayCallback callback, void *pContext);
    SLresult (*SetCallbackEventsMask)(SLPlayItf self, SLuint32 eventFlags);
    void *GetCallbackEventsMask, *SetMarkerPosition, *ClearMarkerPosition, *GetMarkerPosition, *SetPositionUpdatePeriod, *GetPositionUpdatePeriod;
};
struct SLVolumeItf_ {
    SLresult (*SetVolumeLevel)(SLVolumeItf self, SLmillibel level);
    SLresult (*GetVolumeLevel)(SLVolumeItf self, SLmillibel *pLevel);
    void *GetMaxVolumeLevel;
    SLresult (*SetMute)(SLVolumeItf self, SLboolean mute);
    SLresult (*GetMute)(SLVolumeItf self, SLboolean *pMute);
    void *EnableStereoPosition, *IsEnabledStereoPosition;
    SLresult (*SetStereoPosition)(SLVolumeItf self, SLpermille stereoPosition);
    SLresult (*GetStereoPosition)(SLVolumeItf self, SLpermille *pStereoPosition);
};
struct SLEffectSendItf_ {
    SLresult (*EnableEffectSend)(SLEffectSendItf self, const void *pAuxEffect, SLboolean enable, SLmillibel initialLevel);
    SLresult (*IsEnabled)(SLEffectSendItf self, const void *pAuxEffect, SLboolean *pEnable);
    void *SetDirectLevel, *GetDirectLevel, *SetSendLevel, *GetSendLevel;
};
struct SLEnvironmentalReverbItf_ {
    SLresult (*SetRoomLevel)(SLEnvironmentalReverbItf self, SLmillibel room);
    SLresult (*GetRoomLevel)(SLEnvironmentalReverbItf self, SLmillibel *pRoom);
    void *rest[20];
};
struct SLAndroidSimpleBufferQueueItf_ {
    SLresult (*Enqueue)(SLAndroidSimpleBufferQueueItf self, const void *pBuffer, SLuint32 size);
    SLresult (*Clear)(SLAndroidSimpleBufferQueueItf self);
    SLresult (*GetState)(SLAndroidSimpleBufferQueueItf self, SLAndroidSimpleBufferQueueState *pState);
    SLresult (*RegisterCallback)(SLAndroidSimpleBufferQueueItf self, slAndroidSimpleBufferQueueCallback callback, void *pContext);
};

extern const SLInterfaceID SL_IID_ENGINE, SL_IID_PLAY, SL_IID_VOLUME, SL_IID_EFFECTSEND, SL_IID_ENVIRONMENTALREVERB, SL_IID_ANDROIDSIMPLEBUFFERQUEUE, SL_IID_BUFFERQUEUE, SL_IID_SEEK;
SLresult slCreateEngine(SLObjectItf *pEngine, SLuint32 numOptions, const SLEngineOption *pEngineOptions, SLuint32 numInterfaces, const SLInterfaceID *pInterfaceIds, const SLboolean *pInterfaceRequired);
SLresult slQueryNumSupportedEngineInterfaces(SLuint32 *pNumSupportedInterfaces);

/* ---- OpenMAX AL ---- */
typedef SLuint32 XAuint32;
typedef SLuint32 XAboolean;
typedef SLuint32 XAresult;
typedef const struct SLInterfaceID_ *XAInterfaceID;
typedef const struct XAObjectItf_ *const *XAObjectItf;
typedef const struct XAEngineItf_ *const *XAEngineItf;
typedef const struct XAPlayItf_ *const *XAPlayItf;
typedef void (*xaPlayCallback)(XAPlayItf caller, void *pContext, XAuint32 event);
typedef SLDataSource XADataSource;
typedef SLDataSink XADataSink;
typedef struct XADataLocator_NativeDisplay_ {
    XAuint32 locatorType;
    void *hWindow;
    void *hDisplay;
} XADataLocator_NativeDisplay;
#define XA_DATALOCATOR_NATIVEDISPLAY 5u
#define XA_PLAYSTATE_PLAYING 3u
#define XA_PLAYEVENT_HEADATEND 1u

struct XAObjectItf_ {
    XAresult (*Realize)(XAObjectItf self, XAboolean async);
    void *Resume, *GetState;
    XAresult (*GetInterface)(XAObjectItf self, const XAInterfaceID iid, void *pInterface);
    void *RegisterCallback, *AbortAsyncOperation;
    void (*Destroy)(XAObjectItf self);
    void *SetPriority, *GetPriority, *SetLossOfControlInterfaces;
};
struct XAEngineItf_ {
    void *CreateCameraDevice, *CreateRadioDevice, *CreateLEDDevice, *CreateVibraDevice;
    XAresult (*CreateMediaPlayer)(XAEngineItf self, XAObjectItf *pPlayer, XADataSource *pDataSrc, XADataSource *pBankSrc, XADataSink *pAudioSnk, XADataSink *pImageVideoSnk, XADataSink *pVibra, XADataSink *pLEDArray,
                                  XAuint32 numInterfaces, const XAInterfaceID *pInterfaceIds, const XAboolean *pInterfaceRequired);
    void *CreateMediaRecorder;
    XAresult (*CreateOutputMix)(XAEngineItf self, XAObjectItf *pMix, XAuint32 numInterfaces, const XAInterfaceID *pInterfaceIds, const XAboolean *pInterfaceRequired);
    void *rest[11];
};
struct XAPlayItf_ {
    XAresult (*SetPlayState)(XAPlayItf self, XAuint32 state);
    void *GetPlayState, *GetDuration, *GetPosition;
    XAresult (*RegisterCallback)(XAPlayItf self, xaPlayCallback callback, void *pContext);
    void *rest[7];
};
extern const XAInterfaceID XA_IID_ENGINE, XA_IID_PLAY;
XAresult xaCreateEngine(XAObjectItf *pEngine, XAuint32 numOptions, const void *pEngineOptions, XAuint32 numInterfaces, const XAInterfaceID *pInterfaceIds, const XAboolean *pInterfaceRequired);

/* ---- AAudio ---- */
typedef struct AAudioStreamStruct AAudioStream;
typedef struct AAudioStreamBuilderStruct AAudioStreamBuilder;
typedef SLint32 aaudio_result_t;
typedef SLint32 aaudio_data_callback_result_t;
typedef aaudio_data_callback_result_t (*AAudioStream_dataCallback)(AAudioStream *stream, void *userData, void *audioData, SLint32 numFrames);
typedef void (*AAudioStream_errorCallback)(AAudioStream *stream, void *userData, aaudio_result_t error);
#define AAUDIO_FORMAT_PCM_FLOAT 2
#define AAUDIO_CALLBACK_RESULT_CONTINUE 0
#define AAUDIO_CALLBACK_RESULT_STOP 1
#define AAUDIO_ERROR_DISCONNECTED (-899)
aaudio_result_t AAudio_createStreamBuilder(AAudioStreamBuilder **builder);
void AAudioStreamBuilder_setFormat(AAudioStreamBuilder *builder, SLint32 format);
void AAudioStreamBuilder_setChannelCount(AAudioStreamBuilder *builder, SLint32 channelCount);
void AAudioStreamBuilder_setDataCallback(AAudioStreamBuilder *builder, AAudioStream_dataCallback callback, void *userData);
void AAudioStreamBuilder_setErrorCallback(AAudioStreamBuilder *builder, AAudioStream_errorCallback callback, void *userData);
aaudio_result_t AAudioStreamBuilder_openStream(AAudioStreamBuilder *builder, AAudioStream **stream);
aaudio_result_t AAudioStreamBuilder_delete(AAudioStreamBuilder *builder);
aaudio_result_t AAudioStream_requestStart(AAudioStream *stream);
aaudio_result_t AAudioStream_requestStop(AAudioStream *stream);
aaudio_result_t AAudioStream_close(AAudioStream *stream);

/* ---- NdkMediaCodec (callbacks asincronos, estructura por valor) ---- */
typedef struct AMediaCodec AMediaCodec;
typedef struct AMediaFormat AMediaFormat;
typedef SLint32 media_status_t;
typedef struct AMediaCodecBufferInfo {
    SLint32 offset;
    SLint32 size;
    __INT64_TYPE__ presentationTimeUs;
    SLuint32 flags;
} AMediaCodecBufferInfo;
typedef void (*AMediaCodecOnAsyncInputAvailable)(AMediaCodec *codec, void *userdata, SLint32 index);
typedef void (*AMediaCodecOnAsyncOutputAvailable)(AMediaCodec *codec, void *userdata, SLint32 index, AMediaCodecBufferInfo *bufferInfo);
typedef void (*AMediaCodecOnAsyncFormatChanged)(AMediaCodec *codec, void *userdata, AMediaFormat *format);
typedef void (*AMediaCodecOnAsyncError)(AMediaCodec *codec, void *userdata, media_status_t error, SLint32 actionCode, const char *detail);
typedef struct AMediaCodecOnAsyncNotifyCallback {
    AMediaCodecOnAsyncInputAvailable onAsyncInputAvailable;
    AMediaCodecOnAsyncOutputAvailable onAsyncOutputAvailable;
    AMediaCodecOnAsyncFormatChanged onAsyncFormatChanged;
    AMediaCodecOnAsyncError onAsyncError;
} AMediaCodecOnAsyncNotifyCallback;
AMediaCodec *AMediaCodec_createDecoderByType(const char *mime_type);
media_status_t AMediaCodec_setAsyncNotifyCallback(AMediaCodec *codec, AMediaCodecOnAsyncNotifyCallback callback, void *userdata);
media_status_t AMediaCodec_start(AMediaCodec *codec);
media_status_t AMediaCodec_stop(AMediaCodec *codec);
media_status_t AMediaCodec_delete(AMediaCodec *codec);

/* ---- NdkMediaDataSource ---- */
typedef struct AMediaDataSource AMediaDataSource;
typedef struct AMediaExtractor AMediaExtractor;
typedef __INT64_TYPE__ (*AMediaDataSourceReadAt)(void *userdata, __INT64_TYPE__ offset, void *buffer, __SIZE_TYPE__ size);
AMediaDataSource *AMediaDataSource_new(void);
void AMediaDataSource_setUserdata(AMediaDataSource *ds, void *userdata);
void AMediaDataSource_setReadAt(AMediaDataSource *ds, AMediaDataSourceReadAt readAt);
void AMediaDataSource_delete(AMediaDataSource *ds);
AMediaExtractor *AMediaExtractor_new(void);
media_status_t AMediaExtractor_setDataSourceCustom(AMediaExtractor *ex, AMediaDataSource *src);
media_status_t AMediaExtractor_delete(AMediaExtractor *ex);

/* ---- camera2ndk ---- */
typedef struct ACameraManager ACameraManager;
typedef struct ACameraDevice ACameraDevice;
typedef SLint32 camera_status_t;
typedef void (*ACameraManager_AvailabilityCallback)(void *context, const char *cameraId);
typedef struct ACameraManager_AvailabilityListener {
    void *context;
    ACameraManager_AvailabilityCallback onCameraAvailable;
    ACameraManager_AvailabilityCallback onCameraUnavailable;
} ACameraManager_AvailabilityCallbacks;
typedef void (*ACameraDevice_StateCallback)(void *context, ACameraDevice *device);
typedef void (*ACameraDevice_ErrorStateCallback)(void *context, ACameraDevice *device, int error);
typedef struct ACameraDevice_StateCallbacks {
    void *context;
    ACameraDevice_StateCallback onDisconnected;
    ACameraDevice_ErrorStateCallback onError;
} ACameraDevice_StateCallbacks;
ACameraManager *ACameraManager_create(void);
void ACameraManager_delete(ACameraManager *manager);
camera_status_t ACameraManager_registerAvailabilityCallback(ACameraManager *manager, const ACameraManager_AvailabilityCallbacks *callback);
camera_status_t ACameraManager_unregisterAvailabilityCallback(ACameraManager *manager, const ACameraManager_AvailabilityCallbacks *callback);
camera_status_t ACameraManager_openCamera(ACameraManager *manager, const char *cameraId, ACameraDevice_StateCallbacks *callback, ACameraDevice **device);
camera_status_t ACameraDevice_close(ACameraDevice *device);

/* ---- NdkImageReader ---- */
typedef struct AImageReader AImageReader;
typedef void (*AImageReader_ImageCallback)(void *context, AImageReader *reader);
typedef struct AImageReader_ImageListener {
    void *context;
    AImageReader_ImageCallback onImageAvailable;
} AImageReader_ImageListener;
media_status_t AImageReader_new(SLint32 width, SLint32 height, SLint32 format, SLint32 maxImages, AImageReader **reader);
media_status_t AImageReader_setImageListener(AImageReader *reader, AImageReader_ImageListener *listener);
void AImageReader_delete(AImageReader *reader);

/* ---- AChoreographer ---- */
typedef struct AChoreographer AChoreographer;
typedef void (*AChoreographer_frameCallback64)(__INT64_TYPE__ frameTimeNanos, void *data);
typedef void (*AChoreographer_refreshRateCallback)(__INT64_TYPE__ vsyncPeriodNanos, void *data);
AChoreographer *AChoreographer_getInstance(void);
void AChoreographer_postFrameCallback64(AChoreographer *choreographer, AChoreographer_frameCallback64 callback, void *data);
void AChoreographer_registerRefreshRateCallback(AChoreographer *choreographer, AChoreographer_refreshRateCallback, void *data);
void AChoreographer_unregisterRefreshRateCallback(AChoreographer *choreographer, AChoreographer_refreshRateCallback, void *data);

/* ---- Vulkan (vulkan_core.h): VkAllocationCallbacks y las estructuras con callbacks de las cadenas pNext ---- */
typedef __INT32_TYPE__ VkResult;
typedef __UINT32_TYPE__ VkFlags;
typedef __UINT32_TYPE__ VkBool32;
typedef __UINT32_TYPE__ VkStructureType;
typedef struct VkInstance_T *VkInstance;
typedef struct VkPhysicalDevice_T *VkPhysicalDevice;
typedef struct VkDevice_T *VkDevice;
typedef __UINT64_TYPE__ VkDebugUtilsMessengerEXT;
typedef __UINT64_TYPE__ VkBuffer;
#define VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO 1
#define VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO 3
#define VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO 12
#define VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2 1000059000
#define VK_STRUCTURE_TYPE_DEBUG_REPORT_CALLBACK_CREATE_INFO_EXT 1000011000
#define VK_STRUCTURE_TYPE_DEBUG_UTILS_MESSENGER_CALLBACK_DATA_EXT 1000128003
#define VK_STRUCTURE_TYPE_DEBUG_UTILS_MESSENGER_CREATE_INFO_EXT 1000128004
#define VK_STRUCTURE_TYPE_VALIDATION_FEATURES_EXT 1000247000
#define VK_STRUCTURE_TYPE_DEVICE_DEVICE_MEMORY_REPORT_CREATE_INFO_EXT 1000284001
#define VK_STRUCTURE_TYPE_DIRECT_DRIVER_LOADING_INFO_LUNARG 1000459000
#define VK_STRUCTURE_TYPE_DIRECT_DRIVER_LOADING_LIST_LUNARG 1000459001
typedef void *(*PFN_vkAllocationFunction)(void *pUserData, __SIZE_TYPE__ size, __SIZE_TYPE__ alignment, SLuint32 scope);
typedef void *(*PFN_vkReallocationFunction)(void *pUserData, void *pOriginal, __SIZE_TYPE__ size, __SIZE_TYPE__ alignment, SLuint32 scope);
typedef void (*PFN_vkFreeFunction)(void *pUserData, void *pMemory);
typedef void (*PFN_vkInternalAllocationNotification)(void *pUserData, __SIZE_TYPE__ size, SLuint32 type, SLuint32 scope);
typedef void (*PFN_vkInternalFreeNotification)(void *pUserData, __SIZE_TYPE__ size, SLuint32 type, SLuint32 scope);
typedef void (*PFN_vkVoidFunction)(void);
typedef struct VkAllocationCallbacks {
    void *pUserData;
    PFN_vkAllocationFunction pfnAllocation;
    PFN_vkReallocationFunction pfnReallocation;
    PFN_vkFreeFunction pfnFree;
    PFN_vkInternalAllocationNotification pfnInternalAllocation;
    PFN_vkInternalFreeNotification pfnInternalFree;
} VkAllocationCallbacks;
typedef struct VkBaseInStructure {
    VkStructureType sType;
    const struct VkBaseInStructure *pNext;
} VkBaseInStructure;
typedef struct VkInstanceCreateInfo {
    VkStructureType sType;
    const void *pNext;
    VkFlags flags;
    const void *pApplicationInfo;
    SLuint32 enabledLayerCount;
    const char *const *ppEnabledLayerNames;
    SLuint32 enabledExtensionCount;
    const char *const *ppEnabledExtensionNames;
} VkInstanceCreateInfo;
typedef struct VkDeviceCreateInfo {
    VkStructureType sType;
    const void *pNext;
    VkFlags flags;
    SLuint32 queueCreateInfoCount;
    const void *pQueueCreateInfos;
    SLuint32 enabledLayerCount;
    const char *const *ppEnabledLayerNames;
    SLuint32 enabledExtensionCount;
    const char *const *ppEnabledExtensionNames;
    const void *pEnabledFeatures;
} VkDeviceCreateInfo;
typedef struct VkBufferCreateInfo {
    VkStructureType sType;
    const void *pNext;
    VkFlags flags;
    __UINT64_TYPE__ size;
    VkFlags usage;
    SLuint32 sharingMode;
    SLuint32 queueFamilyIndexCount;
    const SLuint32 *pQueueFamilyIndices;
} VkBufferCreateInfo;
typedef struct VkValidationFeaturesEXT {
    VkStructureType sType;
    const void *pNext;
    SLuint32 enabledValidationFeatureCount;
    const SLuint32 *pEnabledValidationFeatures;
    SLuint32 disabledValidationFeatureCount;
    const SLuint32 *pDisabledValidationFeatures;
} VkValidationFeaturesEXT;
typedef struct VkDebugUtilsMessengerCallbackDataEXT {
    VkStructureType sType;
    const void *pNext;
    VkFlags flags;
    const char *pMessageIdName;
    __INT32_TYPE__ messageIdNumber;
    const char *pMessage;
    SLuint32 queueLabelCount;
    const void *pQueueLabels;
    SLuint32 cmdBufLabelCount;
    const void *pCmdBufLabels;
    SLuint32 objectCount;
    const void *pObjects;
} VkDebugUtilsMessengerCallbackDataEXT;
typedef VkBool32 (*PFN_vkDebugUtilsMessengerCallbackEXT)(SLuint32 severity, VkFlags types, const VkDebugUtilsMessengerCallbackDataEXT *data, void *ud);
typedef struct VkDirectDriverLoadingInfoLUNARG {
    VkStructureType sType;
    void *pNext;
    VkFlags flags;
    PFN_vkVoidFunction (*pfnGetInstanceProcAddr)(VkInstance, const char *);
} VkDirectDriverLoadingInfoLUNARG;
typedef struct VkDirectDriverLoadingListLUNARG {
    VkStructureType sType;
    const void *pNext;
    SLuint32 mode;
    SLuint32 driverCount;
    const VkDirectDriverLoadingInfoLUNARG *pDrivers;
} VkDirectDriverLoadingListLUNARG;
typedef struct VkDebugUtilsMessengerCreateInfoEXT {
    VkStructureType sType;
    const void *pNext;
    VkFlags flags;
    VkFlags messageSeverity;
    VkFlags messageType;
    PFN_vkDebugUtilsMessengerCallbackEXT pfnUserCallback;
    void *pUserData;
} VkDebugUtilsMessengerCreateInfoEXT;
typedef VkBool32 (*PFN_vkDebugReportCallbackEXT)(VkFlags flags, SLuint32 objectType, __UINT64_TYPE__ object, __SIZE_TYPE__ location, __INT32_TYPE__ messageCode,
                                                  const char *pLayerPrefix, const char *pMessage, void *pUserData);
typedef struct VkDebugReportCallbackCreateInfoEXT {
    VkStructureType sType;
    const void *pNext;
    VkFlags flags;
    PFN_vkDebugReportCallbackEXT pfnCallback;
    void *pUserData;
} VkDebugReportCallbackCreateInfoEXT;
typedef struct VkDeviceMemoryReportCallbackDataEXT {
    VkStructureType sType;
    void *pNext;
    VkFlags flags;
    SLuint32 type;
    __UINT64_TYPE__ memoryObjectId;
    __UINT64_TYPE__ size;
    SLuint32 objectType;
    __UINT64_TYPE__ objectHandle;
    SLuint32 heapIndex;
} VkDeviceMemoryReportCallbackDataEXT;
typedef void (*PFN_vkDeviceMemoryReportCallbackEXT)(const VkDeviceMemoryReportCallbackDataEXT *data, void *ud);
typedef struct VkDeviceDeviceMemoryReportCreateInfoEXT {
    VkStructureType sType;
    const void *pNext;
    VkFlags flags;
    PFN_vkDeviceMemoryReportCallbackEXT pfnUserCallback;
    void *pUserData;
} VkDeviceDeviceMemoryReportCreateInfoEXT;
typedef VkResult (*PFN_vkCreateDebugUtilsMessengerEXT)(VkInstance, const VkDebugUtilsMessengerCreateInfoEXT *, const VkAllocationCallbacks *, VkDebugUtilsMessengerEXT *);
typedef void (*PFN_vkDestroyDebugUtilsMessengerEXT)(VkInstance, VkDebugUtilsMessengerEXT, const VkAllocationCallbacks *);
typedef void (*PFN_vkSubmitDebugUtilsMessageEXT)(VkInstance, SLuint32 severity, VkFlags types, const VkDebugUtilsMessengerCallbackDataEXT *);
VkResult vkCreateInstance(const VkInstanceCreateInfo *ci, const VkAllocationCallbacks *alloc, VkInstance *out);
void vkDestroyInstance(VkInstance inst, const VkAllocationCallbacks *alloc);
PFN_vkVoidFunction vkGetInstanceProcAddr(VkInstance inst, const char *name);
VkResult vkEnumerateInstanceVersion(SLuint32 *v);
VkResult vkCreateDevice(VkPhysicalDevice pd, const VkDeviceCreateInfo *ci, const VkAllocationCallbacks *alloc, VkDevice *out);
void vkDestroyDevice(VkDevice d, const VkAllocationCallbacks *alloc);
VkResult vkCreateBuffer(VkDevice d, const VkBufferCreateInfo *ci, const VkAllocationCallbacks *alloc, VkBuffer *out);
void vkDestroyBuffer(VkDevice d, VkBuffer b, const VkAllocationCallbacks *alloc);

/* ---- liblog (android/log.h, API 30) ---- */
struct __android_log_message {
    __SIZE_TYPE__ struct_size;
    __INT32_TYPE__ buffer_id;
    __INT32_TYPE__ priority;
    const char *tag;
    const char *file;
    SLuint32 line;
    const char *message;
};
typedef void (*__android_logger_function)(const struct __android_log_message *log_message);
typedef void (*__android_aborter_function)(const char *abort_message);
int __android_log_write(int prio, const char *tag, const char *text);
int __android_log_buf_write(int bufID, int prio, const char *tag, const char *text);
void __android_log_write_log_message(struct __android_log_message *log_message);
void __android_log_set_logger(__android_logger_function logger);
void __android_log_logd_logger(const struct __android_log_message *log_message);
void __android_log_stderr_logger(const struct __android_log_message *log_message);
void __android_log_set_aborter(__android_aborter_function aborter);
void __android_log_call_aborter(const char *abort_message);
int __android_log_is_loggable(int prio, const char *tag, int default_prio);

#endif
