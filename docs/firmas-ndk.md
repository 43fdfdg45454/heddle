# Firmas de la frontera (generado)

Generado por `tools/sigtool` a partir del NDK 29.0.14206865 (API 35). No editar a mano.

Cabeceras analizadas: 320 (arm64) / 320 (x86-64). Con errores de analisis: 3 / 3.

| Biblioteca | Funciones | Directas | de ellas con callback | con JNIEnv | ABI incompatible | A mano / proxy | Sin firma |
|---|---|---|---|---|---|---|---|
| libEGL.so | 73 | 73 | 0 | 0 | 0 | 0 | 0 |
| libGLESv1_CM.so | 278 | 271 | 0 | 0 | 0 | 0 | 7 |
| libGLESv2.so | 107 | 107 | 0 | 0 | 0 | 0 | 0 |
| libGLESv3.so | 256 | 256 | 2 | 0 | 0 | 0 | 0 |
| libOpenMAXAL.so | 3 | 2 | 0 | 0 | 0 | 1 | 0 |
| libOpenSLES.so | 3 | 2 | 0 | 0 | 0 | 1 | 0 |
| libaaudio.so | 70 | 67 | 2 | 0 | 0 | 0 | 3 |
| libamidi.so | 14 | 14 | 0 | 1 | 0 | 0 | 0 |
| libandroid.so | 352 | 351 | 18 | 15 | 0 | 0 | 1 |
| libbinder_ndk.so | 148 | 148 | 32 | 3 | 0 | 0 | 0 |
| libc.so | 1409 | 1245 | 33 | 0 | 35 | 25 | 104 |
| libcamera2ndk.so | 72 | 72 | 1 | 1 | 0 | 0 | 0 |
| libdl.so | 12 | 9 | 1 | 0 | 0 | 0 | 3 |
| libicu.so | 306 | 304 | 2 | 0 | 2 | 0 | 0 |
| libjnigraphics.so | 39 | 39 | 1 | 5 | 0 | 0 | 0 |
| liblog.so | 18 | 17 | 2 | 0 | 1 | 0 | 0 |
| libm.so | 284 | 151 | 0 | 0 | 129 | 4 | 0 |
| libmediandk.so | 161 | 160 | 8 | 0 | 1 | 0 | 0 |
| libnativehelper.so | 6 | 6 | 0 | 5 | 0 | 0 | 0 |
| libnativewindow.so | 11 | 11 | 0 | 0 | 0 | 0 | 0 |
| libneuralnetworks.so | 69 | 69 | 0 | 0 | 0 | 0 | 0 |
| libstdc++.so | 4 | 3 | 0 | 0 | 0 | 0 | 1 |
| libsync.so | 3 | 3 | 0 | 0 | 0 | 0 | 0 |
| libvulkan.so | 233 | 233 | 0 | 0 | 0 | 0 | 0 |
| libz.so | 93 | 84 | 1 | 0 | 1 | 0 | 8 |
| **Total** | 4024 | 3697 | 103 | 30 | 169 | 31 | 127 |

## ABI incompatible (no se reenvian)

- **libc.so — argumento long double** (9): __fpclassifyl, __isfinitel, __isinfl, __isnanl, __isnormall, isfinitel, isinfl, isnanl, isnormall
- **libc.so — argumento va_list** (20): vasprintf, vdprintf, verr, verrx, vfprintf, vfscanf, vfwprintf, vfwscanf, vprintf, vscanf, vsnprintf, vsprintf, vsscanf, vswprintf, vswscanf, vsyslog, vwarn, vwarnx, vwprintf, vwscanf
- **libc.so — estructura por valor en el argumento 0 (80 bytes)** (1): ns_msg_getflag
- **libc.so — retorno de estructura (80 bytes)** (1): mallinfo
- **libc.so — retorno long double** (4): strtold, strtold_l, wcstold, wcstold_l
- **libicu.so — argumento 1: puntero a puntero a funcion** (1): ubidi_getClassCallback
- **libicu.so — argumento 3: puntero a puntero a funcion** (1): ubidi_setClassCallback
- **liblog.so — argumento va_list** (1): __android_log_vprint
- **libm.so — argumento complejo** (66): cabs, cabsf, cabsl, cacos, cacosf, cacosh, cacoshf, cacoshl, cacosl, carg, cargf, cargl, casin, casinf, casinh, casinhf, casinhl, casinl, catan, catanf, catanh, catanhf, catanhl, catanl, ccos, ccosf, ccosh, ccoshf, ccoshl, ccosl, cexp, cexpf, cexpl, cimag, cimagf, cimagl, clog, clogf, clogl, conj, conjf, conjl, cpow, cpowf, cpowl, cproj, cprojf, cprojl, creal, crealf, creall, csin, csinf, csinh, csinhf, csinhl, csinl, csqrt, csqrtf, csqrtl, ctan, ctanf, ctanh, ctanhf, ctanhl, ctanl
- **libm.so — argumento long double** (62): __signbitl, acoshl, acosl, asinhl, asinl, atan2l, atanhl, atanl, cbrtl, ceill, copysignl, coshl, cosl, erfcl, erfl, exp2l, expl, expm1l, fabsl, fdiml, floorl, fmal, fmaxl, fminl, fmodl, frexpl, hypotl, ilogbl, ldexpl, lgammal, lgammal_r, llrintl, llroundl, log10l, log1pl, log2l, logbl, logl, lrintl, lroundl, modfl, nearbyintl, nextafterl, nexttoward, nexttowardf, nexttowardl, powl, remainderl, remquol, rintl, roundl, scalblnl, scalbnl, significandl, sincosl, sinhl, sinl, sqrtl, tanhl, tanl, tgammal, truncl
- **libm.so — retorno long double** (1): nanl
- **libmediandk.so — estructura por valor en el argumento 1 (32 bytes)** (1): AMediaCodec_setAsyncNotifyCallback
- **libz.so — argumento va_list** (1): gzvprintf

## A mano o proxy

- **libOpenMAXAL.so — objeto con tabla de funciones del host (struct XAObjectItf_)** (1): xaCreateEngine
- **libOpenSLES.so — objeto con tabla de funciones del host (struct SLObjectItf_)** (1): slCreateEngine
- **libc.so — la estructura struct epoll_event difiere entre arm64 y x86-64** (6): epoll_ctl, epoll_pwait, epoll_pwait2, epoll_pwait2_64, epoll_pwait64, epoll_wait
- **libc.so — la estructura struct sigaction difiere entre arm64 y x86-64** (1): sigaction
- **libc.so — la estructura struct sigaction64 difiere entre arm64 y x86-64** (1): sigaction64
- **libc.so — la estructura struct stat difiere entre arm64 y x86-64** (11): fstat, fstatat, fts_children, fts_close, fts_open, fts_read, fts_set, ftw, lstat, nftw, stat
- **libc.so — la estructura struct stat64 difiere entre arm64 y x86-64** (6): fstat64, fstatat64, ftw64, lstat64, nftw64, stat64
- **libm.so — la estructura fenv_t difiere entre arm64 y x86-64** (4): fegetenv, feholdexcept, fesetenv, feupdateenv

## Exportadas sin declaracion en las cabeceras

- **libGLESv1_CM.so** (7): glColorPointerBounds, glMatrixIndexPointerOESBounds, glNormalPointerBounds, glPointSizePointerOESBounds, glTexCoordPointerBounds, glVertexPointerBounds, glWeightPointerOESBounds
- **libaaudio.so** (3): AAudioStream_isMMapUsed, AAudio_getMMapPolicy, AAudio_setMMapPolicy
- **libandroid.so** (1): AHardwareBuffer_getNativeHandle
- **libc.so** (104): _Unwind_Backtrace, _Unwind_DeleteException, _Unwind_FindEnclosingFunction, _Unwind_Find_FDE, _Unwind_ForcedUnwind, _Unwind_GetCFA, _Unwind_GetDataRelBase, _Unwind_GetGR, _Unwind_GetIP, _Unwind_GetIPInfo, _Unwind_GetLanguageSpecificData, _Unwind_GetRegionStart, _Unwind_GetTextRelBase, _Unwind_RaiseException, _Unwind_Resume, _Unwind_Resume_or_Rethrow, _Unwind_SetGR, _Unwind_SetIP, __cxa_atexit, __cxa_finalize, __cxa_thread_atexit_impl, __deregister_frame, __dn_count_labels, __dn_skipname, __fgets_chk, __fp_nquery, __fp_query, __fread_chk, __free_hook, __fwrite_chk, __getcwd_chk, __hostalias, __libc_init, __loc_aton, __loc_ntoa, __malloc_hook, __memalign_hook, __memcpy_chk, __memmove_chk, __memset_chk, __open_2, __openat_2, __p_cdname, __p_cdnname, __p_fqname, __p_fqnname, __p_option, __p_query, __p_rcode, __p_secstodate, __p_time, __pread_chk, __putlong, __putshort, __read_chk, __readlink_chk, __realloc_hook, __recvfrom_chk, __register_atfork, __register_frame, __res_close, __res_dnok, __res_hnok, __res_hostalias, __res_isourserver, __res_mailok, __res_nameinquery, __res_nclose, __res_ninit, __res_nmkquery, __res_nquery, __res_nquerydomain, __res_nsearch, __res_nsend, __res_ownok, __res_queriesmatch, __res_querydomain, __res_send, __res_send_setqhook, __res_send_setrhook, __snprintf_chk, __sprintf_chk, __stack_chk_fail, __stpcpy_chk, __strcat_chk, __strcpy_chk, __strncat_chk, __strncpy_chk, __sym_ntop, __sym_ntos, __sym_ston, __tls_get_addr, __vsnprintf_chk, __vsprintf_chk, _getlong, _getshort, _resolv_delete_cache_for_net, _resolv_flush_cache_for_net, _resolv_set_nameservers_for_net, android_reset_stack_guards, delete_module, gets, init_module, nsdispatch
- **libdl.so** (3): __cfi_shadow_size, __cfi_slowpath, __cfi_slowpath_diag
- **libstdc++.so** (1): __cxa_pure_virtual
- **libz.so** (8): _dist_code, _length_code, _tr_align, _tr_flush_bits, _tr_flush_block, _tr_init, _tr_stored_block, _tr_tally

## Interfaces con tabla de funciones (proxy)

El guest recibe objetos proxy cuyas tablas son slots con estas firmas (`src/proxy.rs`). Metodos sin proxy: responden "no soportado".

| Interfaz | Metodos | Sin proxy |
|---|---|---|
| SL3DCommitItf_ | 2 | - |
| SL3DDopplerItf_ | 5 | - |
| SL3DGroupingItf_ | 2 | - |
| SL3DLocationItf_ | 8 | - |
| SL3DMacroscopicItf_ | 6 | - |
| SL3DSourceItf_ | 14 | - |
| SLAndroidAcousticEchoCancellationItf_ | 2 | - |
| SLAndroidAutomaticGainControlItf_ | 2 | - |
| SLAndroidBufferQueueItf_ | 6 | - |
| SLAndroidConfigurationItf_ | 4 | - |
| SLAndroidEffectCapabilitiesItf_ | 2 | - |
| SLAndroidEffectItf_ | 5 | - |
| SLAndroidEffectSendItf_ | 6 | - |
| SLAndroidNoiseSuppressionItf_ | 2 | - |
| SLAndroidSimpleBufferQueueItf_ | 4 | - |
| SLAudioDecoderCapabilitiesItf_ | 2 | - |
| SLAudioEncoderCapabilitiesItf_ | 2 | - |
| SLAudioEncoderItf_ | 2 | - |
| SLAudioIODeviceCapabilitiesItf_ | 11 | - |
| SLBassBoostItf_ | 5 | - |
| SLBufferQueueItf_ | 4 | - |
| SLDeviceVolumeItf_ | 3 | - |
| SLDynamicInterfaceManagementItf_ | 4 | - |
| SLDynamicSourceItf_ | 1 | - |
| SLEffectSendItf_ | 6 | - |
| SLEngineCapabilitiesItf_ | 7 | - |
| SLEngineItf_ | 15 | - |
| SLEnvironmentalReverbItf_ | 22 | - |
| SLEqualizerItf_ | 13 | - |
| SLLEDArrayItf_ | 4 | - |
| SLMIDIMessageItf_ | 5 | - |
| SLMIDIMuteSoloItf_ | 9 | - |
| SLMIDITempoItf_ | 4 | - |
| SLMIDITimeItf_ | 5 | - |
| SLMetadataExtractionItf_ | 7 | - |
| SLMetadataTraversalItf_ | 5 | - |
| SLMuteSoloItf_ | 5 | - |
| SLObjectItf_ | 10 | - |
| SLOutputMixItf_ | 3 | - |
| SLPitchItf_ | 3 | - |
| SLPlayItf_ | 12 | - |
| SLPlaybackRateItf_ | 6 | - |
| SLPrefetchStatusItf_ | 7 | - |
| SLPresetReverbItf_ | 2 | - |
| SLRatePitchItf_ | 3 | - |
| SLRecordItf_ | 12 | - |
| SLSeekItf_ | 3 | - |
| SLThreadSyncItf_ | 2 | - |
| SLVibraItf_ | 6 | - |
| SLVirtualizerItf_ | 5 | - |
| SLVisualizationItf_ | 2 | - |
| SLVolumeItf_ | 9 | - |
| XAAndroidBufferQueueItf_ | 6 | - |
| XAAudioDecoderCapabilitiesItf_ | 2 | - |
| XAAudioEncoderCapabilitiesItf_ | 2 | - |
| XAAudioEncoderItf_ | 2 | - |
| XAAudioIODeviceCapabilitiesItf_ | 11 | - |
| XACameraCapabilitiesItf_ | 9 | - |
| XACameraItf_ | 26 | - |
| XAConfigExtensionsItf_ | 2 | - |
| XADeviceVolumeItf_ | 3 | - |
| XADynamicInterfaceManagementItf_ | 4 | - |
| XADynamicSourceItf_ | 1 | - |
| XAEngineItf_ | 18 | - |
| XAEqualizerItf_ | 13 | - |
| XAImageControlsItf_ | 7 | - |
| XAImageDecoderCapabilitiesItf_ | 2 | - |
| XAImageEffectsItf_ | 4 | - |
| XAImageEncoderCapabilitiesItf_ | 2 | - |
| XAImageEncoderItf_ | 3 | - |
| XALEDArrayItf_ | 4 | - |
| XAMetadataExtractionItf_ | 7 | - |
| XAMetadataInsertionItf_ | 7 | - |
| XAMetadataTraversalItf_ | 5 | - |
| XAObjectItf_ | 10 | - |
| XAOutputMixItf_ | 3 | - |
| XAPlayItf_ | 12 | - |
| XAPlaybackRateItf_ | 6 | - |
| XAPrefetchStatusItf_ | 7 | - |
| XARDSItf_ | 23 | - |
| XARadioItf_ | 18 | - |
| XARecordItf_ | 12 | - |
| XASeekItf_ | 3 | - |
| XASnapshotItf_ | 8 | InitiateSnapshot (Rec(16, false)) |
| XAStreamInformationItf_ | 7 | - |
| XAThreadSyncItf_ | 2 | - |
| XAVibraItf_ | 6 | - |
| XAVideoDecoderCapabilitiesItf_ | 2 | - |
| XAVideoEncoderCapabilitiesItf_ | 2 | - |
| XAVideoEncoderItf_ | 2 | - |
| XAVideoPostProcessingItf_ | 7 | - |
| XAVolumeItf_ | 9 | - |

## Estructuras con punteros a funcion

- **ACameraCaptureSession_capture**: argumento 1: 7 callbacks
- **ACameraCaptureSession_captureV2**: argumento 1: 7 callbacks
- **ACameraCaptureSession_logicalCamera_capture**: argumento 1: 7 callbacks
- **ACameraCaptureSession_logicalCamera_captureV2**: argumento 1: 7 callbacks
- **ACameraCaptureSession_logicalCamera_setRepeatingRequest**: argumento 1: 7 callbacks
- **ACameraCaptureSession_logicalCamera_setRepeatingRequestV2**: argumento 1: 7 callbacks
- **ACameraCaptureSession_setRepeatingRequest**: argumento 1: 7 callbacks
- **ACameraCaptureSession_setRepeatingRequestV2**: argumento 1: 7 callbacks
- **ACameraDevice_createCaptureSession**: argumento 2: 3 callbacks
- **ACameraDevice_createCaptureSessionWithSessionParameters**: argumento 3: 3 callbacks
- **ACameraManager_openCamera**: argumento 2: 2 callbacks
- **ACameraManager_registerAvailabilityCallback**: argumento 1: 2 callbacks
- **ACameraManager_registerExtendedAvailabilityCallback**: argumento 1: 5 callbacks
- **ACameraManager_unregisterAvailabilityCallback**: argumento 1: 2 callbacks
- **ACameraManager_unregisterExtendedAvailabilityCallback**: argumento 1: 5 callbacks
- **AImageReader_setBufferRemovedListener**: argumento 1: 1 callbacks
- **AImageReader_setImageListener**: argumento 1: 1 callbacks
- **AMediaCodec_setAsyncNotifyCallback**: argumento 1: 4 callbacks
- **__pthread_cleanup_pop**: argumento 0: punteros a funcion alcanzables por otro puntero
- **__pthread_cleanup_push**: argumento 0: punteros a funcion alcanzables por otro puntero
- **deflate**: argumento 0: 2 callbacks
- **deflateBound**: argumento 0: 2 callbacks
- **deflateCopy**: argumento 0: 2 callbacks
- **deflateCopy**: argumento 1: 2 callbacks
- **deflateEnd**: argumento 0: 2 callbacks
- **deflateGetDictionary**: argumento 0: 2 callbacks
- **deflateInit2_**: argumento 0: 2 callbacks
- **deflateInit_**: argumento 0: 2 callbacks
- **deflateParams**: argumento 0: 2 callbacks
- **deflatePending**: argumento 0: 2 callbacks
- **deflatePrime**: argumento 0: 2 callbacks
- **deflateReset**: argumento 0: 2 callbacks
- **deflateResetKeep**: argumento 0: 2 callbacks
- **deflateSetDictionary**: argumento 0: 2 callbacks
- **deflateSetHeader**: argumento 0: 2 callbacks
- **deflateTune**: argumento 0: 2 callbacks
- **fts_children**: argumento 0: callback variadico o sin prototipo
- **fts_close**: argumento 0: callback variadico o sin prototipo
- **fts_read**: argumento 0: callback variadico o sin prototipo
- **fts_set**: argumento 0: callback variadico o sin prototipo
- **glob**: argumento 3: la estructura struct stat difiere entre arm64 y x86-64
- **globfree**: argumento 0: la estructura struct stat difiere entre arm64 y x86-64
- **inflate**: argumento 0: 2 callbacks
- **inflateBack**: argumento 0: 2 callbacks
- **inflateBackEnd**: argumento 0: 2 callbacks
- **inflateBackInit_**: argumento 0: 2 callbacks
- **inflateCodesUsed**: argumento 0: 2 callbacks
- **inflateCopy**: argumento 0: 2 callbacks
- **inflateCopy**: argumento 1: 2 callbacks
- **inflateEnd**: argumento 0: 2 callbacks
- **inflateGetDictionary**: argumento 0: 2 callbacks
- **inflateGetHeader**: argumento 0: 2 callbacks
- **inflateInit2_**: argumento 0: 2 callbacks
- **inflateInit_**: argumento 0: 2 callbacks
- **inflateMark**: argumento 0: 2 callbacks
- **inflatePrime**: argumento 0: 2 callbacks
- **inflateReset**: argumento 0: 2 callbacks
- **inflateReset2**: argumento 0: 2 callbacks
- **inflateResetKeep**: argumento 0: 2 callbacks
- **inflateSetDictionary**: argumento 0: 2 callbacks
- **inflateSync**: argumento 0: 2 callbacks
- **inflateSyncPoint**: argumento 0: 2 callbacks
- **inflateUndermine**: argumento 0: 2 callbacks
- **inflateValidate**: argumento 0: 2 callbacks
- **sigaction**: argumento 1: 1 callbacks
- **sigaction**: argumento 2: 1 callbacks
- **sigaction64**: argumento 1: 1 callbacks
- **sigaction64**: argumento 2: 1 callbacks
- **timer_create**: argumento 1: puntero a funcion dentro de una union
- **utrans_trans**: argumento 2: 6 callbacks
- **utrans_transIncremental**: argumento 2: 6 callbacks
- **vkAllocateMemory**: argumento 2: 5 callbacks
- **vkCreateAndroidSurfaceKHR**: argumento 2: 5 callbacks
- **vkCreateBuffer**: argumento 2: 5 callbacks
- **vkCreateBufferView**: argumento 2: 5 callbacks
- **vkCreateCommandPool**: argumento 2: 5 callbacks
- **vkCreateComputePipelines**: argumento 4: 5 callbacks
- **vkCreateDescriptorPool**: argumento 2: 5 callbacks
- **vkCreateDescriptorSetLayout**: argumento 2: 5 callbacks
- **vkCreateDescriptorUpdateTemplate**: argumento 2: 5 callbacks
- **vkCreateDevice**: argumento 2: 5 callbacks
- **vkCreateEvent**: argumento 2: 5 callbacks
- **vkCreateFence**: argumento 2: 5 callbacks
- **vkCreateFramebuffer**: argumento 2: 5 callbacks
- **vkCreateGraphicsPipelines**: argumento 4: 5 callbacks
- **vkCreateImage**: argumento 2: 5 callbacks
- **vkCreateImageView**: argumento 2: 5 callbacks
- **vkCreateInstance**: argumento 1: 5 callbacks
- **vkCreatePipelineCache**: argumento 2: 5 callbacks
- **vkCreatePipelineLayout**: argumento 2: 5 callbacks
- **vkCreatePrivateDataSlot**: argumento 2: 5 callbacks
- **vkCreateQueryPool**: argumento 2: 5 callbacks
- **vkCreateRenderPass**: argumento 2: 5 callbacks
- **vkCreateRenderPass2**: argumento 2: 5 callbacks
- **vkCreateSampler**: argumento 2: 5 callbacks
- **vkCreateSamplerYcbcrConversion**: argumento 2: 5 callbacks
- **vkCreateSemaphore**: argumento 2: 5 callbacks
- **vkCreateShaderModule**: argumento 2: 5 callbacks
- **vkCreateSwapchainKHR**: argumento 2: 5 callbacks
- **vkDestroyBuffer**: argumento 2: 5 callbacks
- **vkDestroyBufferView**: argumento 2: 5 callbacks
- **vkDestroyCommandPool**: argumento 2: 5 callbacks
- **vkDestroyDescriptorPool**: argumento 2: 5 callbacks
- **vkDestroyDescriptorSetLayout**: argumento 2: 5 callbacks
- **vkDestroyDescriptorUpdateTemplate**: argumento 2: 5 callbacks
- **vkDestroyDevice**: argumento 1: 5 callbacks
- **vkDestroyEvent**: argumento 2: 5 callbacks
- **vkDestroyFence**: argumento 2: 5 callbacks
- **vkDestroyFramebuffer**: argumento 2: 5 callbacks
- **vkDestroyImage**: argumento 2: 5 callbacks
- **vkDestroyImageView**: argumento 2: 5 callbacks
- **vkDestroyInstance**: argumento 1: 5 callbacks
- **vkDestroyPipeline**: argumento 2: 5 callbacks
- **vkDestroyPipelineCache**: argumento 2: 5 callbacks
- **vkDestroyPipelineLayout**: argumento 2: 5 callbacks
- **vkDestroyPrivateDataSlot**: argumento 2: 5 callbacks
- **vkDestroyQueryPool**: argumento 2: 5 callbacks
- **vkDestroyRenderPass**: argumento 2: 5 callbacks
- **vkDestroySampler**: argumento 2: 5 callbacks
- **vkDestroySamplerYcbcrConversion**: argumento 2: 5 callbacks
- **vkDestroySemaphore**: argumento 2: 5 callbacks
- **vkDestroyShaderModule**: argumento 2: 5 callbacks
- **vkDestroySurfaceKHR**: argumento 2: 5 callbacks
- **vkDestroySwapchainKHR**: argumento 2: 5 callbacks
- **vkFreeMemory**: argumento 2: 5 callbacks

## Vulkan: pAllocator y cadenas pNext

Origen de las relaciones entre estructuras: `structextends` y `len` de vk.xml (version 275).

800 estructuras con sType; 7 pueden llevar callbacks directa o transitivamente (`VK_TYPES`); 101 funciones en `VK_FNS`, 4 de ellas con estructuras de entrada que pueden llevarlos (las demas solo `pAllocator`). `VkAllocationCallbacks`: 5 callbacks.

- estructura VkDebugReportCallbackCreateInfoEXT: 1 callbacks
- estructura VkDebugUtilsMessengerCreateInfoEXT: 1 callbacks
- estructura VkDeviceDeviceMemoryReportCreateInfoEXT: 1 callbacks
- estructura VkDirectDriverLoadingInfoLUNARG: callback con retorno no reenviable (Cb("V"))
- funcion vkCreateDebugReportCallbackEXT: pCreateInfo (VkDebugReportCallbackCreateInfoEXT) puede llevar callbacks
- funcion vkCreateDebugUtilsMessengerEXT: pCreateInfo (VkDebugUtilsMessengerCreateInfoEXT) puede llevar callbacks
- funcion vkCreateDevice: pCreateInfo (VkDeviceCreateInfo) puede llevar callbacks
- funcion vkCreateInstance: pCreateInfo (VkInstanceCreateInfo) puede llevar callbacks

Estructuras con cabecera sType/pNext sin constante `VK_STRUCTURE_TYPE_*` reconocida (no se copian en una cadena): VkBaseInStructure, VkBaseOutStructure

## Cabeceras con errores de analisis

arm64: EGL/Platform.h, sys/ucontext.h, time64.h

x86-64: EGL/Platform.h, sys/ucontext.h, time64.h
