// Define variables TLS que otra biblioteca importa (libt5use.so, a traves de libt5mid.so).
__thread int tv_ie = 42;
__thread int tv_gd = 1000;
__thread char tv_buf[64] = "tls";
int *tls_ie_addr(void) { return &tv_ie; }
int *tls_gd_addr(void) { return &tv_gd; }
