// t8: TLS initial-exec (R_AARCH64_TLS_TPREL64) hacia una biblioteca abierta con dlopen: bionic rechaza la carga.
extern __thread int t8_tls_var __attribute__((tls_model("initial-exec")));
int ie_get(void) { return t8_tls_var; }
