// t10: enlazada con p_align de 2 MiB (-z max-page-size=0x200000): con paginas enormes transparentes y
// targetSdkVersion >= 31, bionic la coloca alineada a 2 MiB.
int t10huge_id(void) { return 2; }
