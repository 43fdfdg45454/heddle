# bench: microbenchmarks de heddle

Microbenchmarks de la traducción ARM64→x86-64 (flags, monitor exclusivo LL/SC, SIMD) y su versión nativa AArch64
para comparar contra hardware ARM real (`native/arm_native.c`, pendiente de portar a Rust).

`./build.sh` y ejecutar `out/arm_xlat_suite`. Las columnas "ARM ns" requieren un archivo generado en un ARM64 real
(`./arm_native > arm_results.txt`); sin él muestran n/d. Nunca se midió en hardware ARM.
