// Registro comun de t8: los constructores y destructores de las bibliotecas del ciclo anotan aqui su orden.
int t8_log[64], t8_n;
void t8_note(int v) { if (t8_n < 64) t8_log[t8_n++] = v; }
