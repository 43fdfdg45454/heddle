// t8: TLS initial-exec de la propia biblioteca (tambien abierta con dlopen): bionic tambien la rechaza.
__thread int own_tls __attribute__((tls_model("initial-exec"))) = 3;
int own_get(void) { return own_tls; }
