//! Lint de arquitectura: impide reintroducir los patrones que causaron filtraciones de punteros y fugas de memoria.
//! Cada regla corresponde a un invariante de CLAUDE.md. Si una regla falla, NO se "arregla" subiendo el numero sin
//! mas: hay que justificar en CLAUDE.md por que el nuevo uso esta acotado o es seguro, y solo entonces actualizarla.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Codigo de produccion de cada archivo de src/ (se descarta todo desde el primer `#[cfg(test)]`) sin comentarios.
fn sources() -> HashMap<String, String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut m = HashMap::new();
    for e in fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().and_then(|x| x.to_str()) != Some("rs") {
            continue;
        }
        let text = fs::read_to_string(&p).unwrap();
        let prod = text.split("#[cfg(test)]").next().unwrap();
        let code: String = prod.lines().filter(|l| !l.trim_start().starts_with("//")).map(strip_strings).collect::<Vec<_>>().join("\n");
        m.insert(p.file_name().unwrap().to_str().unwrap().to_string(), code);
    }
    m
}

/// Quita el contenido de los literales de cadena de una linea (los mensajes de registro mencionan `dlsym(`, etc.).
fn strip_strings(l: &str) -> String {
    let (mut out, mut in_str, mut esc) = (String::new(), false, false);
    for ch in l.chars() {
        if in_str {
            if esc {
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_str = false;
                out.push('"');
            }
        } else {
            out.push(ch);
            if ch == '"' {
                in_str = true;
            }
        }
    }
    out
}

fn count(code: &str, pat: &str) -> usize {
    code.matches(pat).count()
}

/// `pat` solo puede aparecer en los archivos de `allowed`.
fn only_in(src: &HashMap<String, String>, pat: &str, allowed: &[&str], why: &str) {
    for (f, code) in src {
        if !allowed.contains(&f.as_str()) {
            assert_eq!(count(code, pat), 0, "`{}` aparece en src/{} y solo se permite en {:?}. {}", pat, f, allowed, why);
        }
    }
}

/// Numero exacto de apariciones de `pat` por archivo (los no listados deben tener 0).
fn exact(src: &HashMap<String, String>, pat: &str, expected: &[(&str, usize)], why: &str) {
    for (f, code) in src {
        let want = expected.iter().find(|(n, _)| n == f).map_or(0, |(_, k)| *k);
        assert_eq!(count(code, pat), want, "`{}` en src/{}: se esperaban {} usos. {}", pat, f, want, why);
    }
}

#[test]
fn solo_la_frontera_llama_al_host() {
    let s = sources();
    let why = "Toda llamada al host pasa por boundary.rs (HostFn validado, argumentos y retorno saneados).";
    only_in(&s, "hostcall::call(", &["boundary.rs"], why);
    only_in(&s, "heddle_hostcall", &["hostcall.rs"], why);
    only_in(&s, "Call::new(", &["boundary.rs"], why);
    only_in(&s, "HostFn(", &["boundary.rs"], "HostFn solo se construye en boundary.rs, validando la direccion.");
}

#[test]
fn solo_la_frontera_busca_simbolos_del_host() {
    let s = sources();
    let why = "La busqueda de simbolos esta cerrada por defecto (lista de bibliotecas permitidas en boundary.rs).";
    // monitor.rs consulta un dato interno de la libc (__rseq_offset) para el propio puente: no es un simbolo para el guest
    // y bridge.rs resuelve __android_log_write para el registro del propio puente. Ninguno entrega nada al guest.
    only_in(&s, "dlsym(", &["boundary.rs", "sys.rs", "monitor.rs", "bridge.rs"], why);
    assert_eq!(count(&s["monitor.rs"], "dlsym("), 2, "monitor.rs: solo __rseq_offset");
    assert_eq!(count(&s["bridge.rs"], "dlsym("), 2, "bridge.rs: solo __android_log_write");
    only_in(&s, "dlopen(", &["boundary.rs", "sys.rs"], why);
    only_in(&s, "RTLD_DEFAULT", &["boundary.rs", "sys.rs"], why);
}

#[test]
fn las_reservas_de_memoria_tienen_dueno() {
    let s = sources();
    let why = "Las reservas del puente usan mem::Region (se liberan en Drop). elf.rs mapea los segmentos de los modulos.";
    only_in(&s, "mmap(", &["mem.rs", "sys.rs", "elf.rs"], why);
    only_in(&s, "munmap(", &["mem.rs", "sys.rs", "elf.rs"], why);
    // elf.rs: reserva del modulo + segmento del archivo + bss + region del llamador devuelta sin acceso
    // (ANDROID_DLEXT_RESERVED_ADDRESS, `Rsv::drop`) + RELRO compartido (escrito y leido: ANDROID_DLEXT_*_RELRO)
    exact(&s, "mmap(", &[("mem.rs", 1), ("sys.rs", 1), ("elf.rs", 6)], why);
    // elf.rs: la reserva del modulo (y su hueco) se desmapea en el Drop de `Rsv`; `reserve` recorta el relleno de
    // alineacion como bionic (un intento descartado, delante, detras y tras el hueco)
    exact(&s, "munmap(", &[("mem.rs", 1), ("sys.rs", 1), ("elf.rs", 6)], why);
}

#[test]
fn nada_se_filtra_sin_estar_declarado() {
    let s = sources();
    let why = "Cada memoria que se cede para siempre debe estar acotada (ver CLAUDE.md, seccion Memoria).";
    // jni: las dos tablas de funciones (una vez). libc_hle: cadenas internas (acotado por TABLE_CAP) y celdas de
    // datos por nombre de simbolo. (cbthunk ya no: sus ThunkInfo viven en una tabla estatica.)
    exact(&s, "Box::leak", &[("jni.rs", 2), ("libc_hle.rs", 2)], why);
    // mem: Region::permanent (tamano fijo).
    exact(&s, "mem::forget", &[("mem.rs", 1)], why);
    // libc_hle: StartInfo de pthread_create (lo libera el hilo nuevo). rt: GuestThread (lo guarda el TLS del hilo).
    exact(&s, "into_raw", &[("libc_hle.rs", 1), ("rt.rs", 1)], why);
    exact(&s, "ManuallyDrop", &[], why);
}

#[test]
fn colecciones_globales_conocidas_y_acotadas() {
    let s = sources();
    // Cada coleccion global debe tener una cota (ver CLAUDE.md). Anadir una exige documentar su cota alli.
    let expected: &[(&str, usize)] = &[("boundary.rs", 2), ("elf.rs", 1), ("jni.rs", 2), ("libc_hle.rs", 4), ("mem.rs", 1), ("monitor.rs", 1), ("namespace.rs", 1), ("rt.rs", 2)];
    for (f, code) in &s {
        let n = code
            .lines()
            .filter(|l| {
                let t = l.trim_start().trim_start_matches("pub ");
                t.starts_with("static ") && (t.contains("Mutex<") || t.contains("RwLock<")) && (t.contains("Vec<") || t.contains("HashMap<"))
            })
            .count();
        let want = expected.iter().find(|(n, _)| n == f).map_or(0, |(_, k)| *k);
        assert_eq!(n, want, "src/{}: {} colecciones globales (se esperaban {}). Documenta su cota en CLAUDE.md.", f, n, want);
    }
}

#[test]
fn sin_conversiones_ciegas_de_punteros() {
    let s = sources();
    // transmute de un entero a puntero a funcion: solo el JIT (bloques que el mismo genero) y la tabla del bridge.
    exact(&s, "transmute", &[("jit.rs", 3), ("bridge.rs", 1)], "No convertir enteros a punteros a funcion fuera del JIT.");
    // la guardia generica que convertia argumentos por heuristica (corrompia cadenas) no debe volver
    for (f, code) in &s {
        assert_eq!(count(code, "guard_callbacks"), 0, "src/{}: no reintroducir conversiones heuristicas de argumentos", f);
        // ni conversiones por el aspecto del valor en argumentos o retornos del reenvio universal
        assert_eq!(count(code, "sanitize_ret"), 0, "src/{}: el retorno del host no tiene tipo; no convertirlo por heuristica", f);
        assert_eq!(count(code, "sanitize_arg"), 0, "src/{}: los argumentos no tienen tipo; no convertirlos por heuristica", f);
    }
}

#[test]
fn el_guest_no_ejecuta_codigo_del_host() {
    let s = sources();
    // el unico tratamiento de un salto del guest a codigo del host es boundary::guest_jumped_to_host (fallo)
    let rt = &s["rt.rs"];
    assert!(rt.contains("Event::HostCall => crate::boundary::guest_jumped_to_host(c)"), "rt.rs debe delegar el salto a host en la frontera");
    let b = &s["boundary.rs"];
    let body = b.split("pub fn guest_jumped_to_host").nth(1).unwrap().split("\n}\n").next().unwrap();
    assert!(!body.contains("call_host") && !body.contains("hostcall"), "guest_jumped_to_host no debe llamar al host");
    // el redireccionamiento host->guest valida que la direccion sea ejecutable por el guest (variante sin bloqueos:
    // se ejecuta en un manejador de senal)
    assert!(s["sig.rs"].contains("is_guest_executable_sig(rip)"), "try_exec_redirect debe validar la direccion");
}

#[test]
fn solo_se_reenvia_con_firma() {
    let s = sources();
    // La resolucion de simbolos consulta la tabla generada; no hay reenvio "universal" para nombres desconocidos.
    let l = &s["libc_hle.rs"];
    assert!(l.contains("crate::sigs::lookup(name)"), "resolve() debe decidir por la tabla de firmas");
    assert!(l.contains("typed_slot("), "el reenvio se construye a partir de la firma");
    // las listas mantenidas a mano que la tabla sustituye no deben volver
    for (f, code) in &s {
        for pat in ["CB_FUNCS", "JNI_LIBS", "abi_unsafe(", "symbol_slot("] {
            assert_eq!(count(code, pat), 0, "src/{}: `{}` fue sustituido por la tabla de firmas (sigs.rs)", f, pat);
        }
        // forward_handler crea un reenvio sin conversiones: solo la frontera decide cuando es valido
        if f != "boundary.rs" {
            assert_eq!(count(code, "forward_handler("), 0, "src/{}: usar boundary::typed_slot con la firma", f);
        }
    }
    // la tabla generada no se edita a mano
    let g = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/sigs_gen.rs")).unwrap();
    assert!(g.starts_with("// @generated por tools/sigtool"), "src/sigs_gen.rs debe ser la salida del generador");
}

#[test]
fn los_objetos_del_ndk_con_tabla_de_funciones_van_por_proxy() {
    let s = sources();
    // OpenSL ES / OpenMAX AL: el guest recibe proxys cuyas tablas salen de la tabla generada (sigs::ITFS)
    let p = &s["proxy.rs"];
    assert_eq!(count(p, "create_engine(c,"), 2, "slCreateEngine/xaCreateEngine deben entregar proxys");
    assert!(p.contains("ITFS"), "las tablas del proxy salen de la tabla generada");
    // ANativeActivity: el guest recibe un proxy; la estructura del host no se modifica (ni su vm/env ni su tabla de
    // callbacks)
    assert!(s["cbthunk.rs"].contains("ndkcb::activity_on_create"), "ANativeActivity_onCreate debe pasar por el proxy de ndkcb");
    // (los envoltorios de JNIEnv/JavaVM de cbthunk son solo los de los modos JNI, uno de cada)
    assert_eq!(count(&s["cbthunk.rs"], "guest_vm_for("), 1, "cbthunk.rs: la conversion de ANativeActivity vive en ndkcb.rs");
    assert_eq!(count(&s["cbthunk.rs"], "guest_env_for("), 1, "cbthunk.rs: la conversion de ANativeActivity vive en ndkcb.rs");
    assert_eq!(count(&s["cbthunk.rs"], "to_host_callable("), 0, "cbthunk.rs: la tabla de callbacks del host no se sustituye");
    // las estructuras de callbacks solo se convierten por copia donde el host la copia, y en su sitio donde el host la
    // guarda (listas escritas a mano); la resolucion las toma de boundary::Conv::of
    assert!(s["boundary.rs"].contains("STRUCT_COPIED.contains(&name)"), "la copia convertida solo para sigs::STRUCT_COPIED");
    assert!(s["boundary.rs"].contains("STRUCT_INPLACE.contains(&s.ty)"), "la sustitucion en su sitio solo para sigs::STRUCT_INPLACE");
    assert!(s["libc_hle.rs"].contains("boundary::Conv::of(name)"), "resolve() toma las conversiones de la tabla generada");
    // lo que vkGet*ProcAddr/eglGetProcAddress entregan por nombre lleva las mismas conversiones que la importacion
    let b = &s["boundary.rs"];
    let gs = b.split("fn guest_slot_for").nth(1).unwrap().split("\n}\n").next().unwrap();
    assert!(gs.contains("Conv::of(name)"), "guest_slot_for debe aplicar la firma del nombre pedido");
    // Vulkan: pAllocator y cadenas pNext por la tabla generada (sigs::VK_FNS, VK_STYPES)
    assert!(s["vk.rs"].contains("VK_ALLOC") && s["vk.rs"].contains("vk_stype("), "vk.rs convierte por las tablas generadas");
}

#[test]
fn semantica_escrita_a_mano_de_los_proxys() {
    let s = sources();
    // liblog: el registrador del guest va detras del despachador del puente, y los mensajes del puente no le llegan
    let n = &s["ndkcb.rs"];
    assert!(n.contains("fn heddle_logger(msg: u64)"), "el registrador del guest se instala detras de ndkcb::heddle_logger");
    let alog = s["bridge.rs"].split("fn alog_prio").nth(1).unwrap().split("\n}\n").next().unwrap();
    assert!(alog.contains("bridge_logging("), "los mensajes del puente se marcan para no llegar al registrador del guest");
    // pthread_exit como bionic: thread_local, manejadores de pthread_cleanup_push y claves
    assert!(s["libc_hle.rs"].contains("pub fn pthread_exit_dtors"));
    assert!(s["rt.rs"].contains("crate::libc_hle::pthread_exit_dtors()"), "la vuelta de la funcion del hilo es pthread_exit");
    let te = s["libc_hle.rs"].split("pub unsafe fn thread_exit_now").nth(1).unwrap().split("\n}\n").next().unwrap();
    assert!(te.contains("pthread_exit_dtors()"), "pthread_exit ejecuta la cadena de limpieza");
    // NativeActivity: el punto de entrada propio sale del manifiesto, no del aspecto de los argumentos
    let tr = s["bridge.rs"].split("fn trampoline(").nth(1).unwrap().split("\n}\n").next().unwrap();
    assert!(tr.contains("is_activity_entry("), "android.app.func_name se reconoce por el manifiesto");
}

#[test]
fn ci_ejecuta_las_pruebas() {
    let y = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/build.yml")).unwrap();
    assert!(y.contains("cargo test --release"), "el CI debe ejecutar las pruebas (incluido este lint) antes de publicar");
}

/// Rutas calientes del monitor (CLAUDE.md, "Monitor": alineacion): cada entrada de los stores y CAS rapidos en
/// ensamblador (`heddle_rst*`, `heddle_fst*`, `heddle_fcas*`) va precedida de `.p2align 6`, asi su ruta caliente no
/// cambia de linea de cache ni de ventana de 32 B segun donde la deje el enlazador. La prueba
/// `monitor::align_tests::rutas_calientes_alineadas` comprueba las direcciones en el binario.
#[test]
fn rutas_calientes_del_monitor_alineadas() {
    let t = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/monitor.rs")).unwrap();
    let l: Vec<&str> = t.lines().map(str::trim).collect();
    let mut n = 0;
    for p in ["heddle_rst", "heddle_fst", "heddle_fcas"] {
        for k in 0..4 {
            let lab = format!("\"{p}{k}:\",");
            let i = l.iter().position(|x| *x == lab).unwrap_or_else(|| panic!("falta la entrada {p}{k} en monitor.rs"));
            assert!(i >= 3 && l[i - 3] == "\".p2align 6\",", "{p}{k}: falta `.p2align 6` delante de la entrada");
            n += 1;
        }
    }
    assert_eq!(n, 12);
}
