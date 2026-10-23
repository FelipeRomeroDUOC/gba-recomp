use gba_core::{Bus, Machine};

fn dummy_rom() -> Vec<u8> {
    let mut rom = vec![0u8; 1024 * 1024];
    // Instrucción ARM: b . (bucle infinito sobre sí mismo)
    rom[0] = 0xFE;
    rom[1] = 0xFF;
    rom[2] = 0xFF;
    rom[3] = 0xEA;
    rom
}

#[test]
#[cfg(feature = "perf-counters")]
fn test_perf_counters_accumulate_and_reset() {
    let mut m = Machine::new(dummy_rom());
    m.bus.perf_audio_timing = true;

    // Ejecutar hasta completar un fotograma
    m.run_frame(300_000);
    assert!(m.bus.frames >= 1, "Debe haber completado al menos 1 frame");

    let c = m.take_perf_counters();

    assert!(c.ppu_ns > 0, "PPU debe registrar tiempo acumulado tras 160 scanlines (obtenido: {} ns)", c.ppu_ns);
    assert!(c.apu_ns > 0, "APU debe registrar tiempo acumulado de muestreo (obtenido: {} ns)", c.apu_ns);
    assert!(c.reads > 0, "Debe haber registrado lecturas de memoria (obtenido: {})", c.reads);

    // Verificación de las etapas desglosadas de la PPU (Objetivo 2)
    assert!(c.bg_ns > 0, "bg_ns debe acumular tiempo de renderizado de fondos");
    assert!(c.compose_ns > 0, "compose_ns debe acumular tiempo de composición de líneas");
    let stage_sum = c.bg_ns + c.sprite_ns + c.compose_ns + c.copy_ns;
    assert!(stage_sum > 0, "La suma de etapas debe ser > 0");

    // take_perf_counters debe resetear los contadores a cero
    let reset = m.take_perf_counters();
    assert_eq!(reset.ppu_ns, 0);
    assert_eq!(reset.apu_ns, 0);
    assert_eq!(reset.reads, 0);
    assert_eq!(reset.writes, 0);
    assert_eq!(reset.bg_ns, 0);
    assert_eq!(reset.sprite_ns, 0);
    assert_eq!(reset.compose_ns, 0);
    assert_eq!(reset.copy_ns, 0);
    assert_eq!(reset.sprites_visibles, 0);
    assert_eq!(reset.fondos_activos, 0);
    assert_eq!(reset.lineas_inertes, 0);
    assert_eq!(reset.lineas_efecto, 0);
    assert_eq!(reset.lineas_efecto_alpha, 0);
    assert_eq!(reset.lineas_efecto_brillo, 0);
    assert_eq!(reset.lineas_efecto_oscuridad, 0);
}

/// Un oscurecido real (`BLDCNT` en modo 3 con `BLDY` distinto de cero) ya no descarta la línea:
/// la vía rápida aplica el efecto sin mirar la segunda capa. El reparto por modo tiene que
/// cuadrar con `mezcla`, y con este efecto no debe quedar ninguna línea descartada.
#[test]
#[cfg(feature = "perf-counters")]
fn test_el_desglose_de_efectos_cuadra_con_mezcla() {
    let mut m = Machine::new(dummy_rom());
    m.bus.write16(0x0400_0050, 0x3FFF); // oscurecer con todos los objetivos
    m.bus.write16(0x0400_0054, 8); // BLDY = 8: efecto real
    m.run_frame(300_000);
    let c = m.take_perf_counters();

    let dibujadas = c.lineas_rectas + c.descartes_mezcla + c.descartes_ventana + c.descartes_semi;
    assert!(dibujadas > 0, "el fotograma debe dibujar líneas (obtenido: {dibujadas})");
    assert_eq!(
        c.lineas_efecto_alpha + c.lineas_efecto_brillo + c.lineas_efecto_oscuridad,
        c.lineas_efecto + c.descartes_mezcla,
        "las líneas con efecto real se reparten por modo, estén en la vía rápida o descartadas"
    );
    assert_eq!(
        c.lineas_rectas, dibujadas,
        "un oscurecido no debe descartar ninguna línea (descartadas: {})",
        c.descartes_mezcla
    );
    assert_eq!(c.lineas_efecto, c.lineas_rectas, "todas las líneas aplicaron el efecto por la vía rápida");
    assert_eq!(c.lineas_efecto_oscuridad, c.lineas_rectas, "el modo era oscurecer, así que el reparto lo tiene que decir");
    assert_eq!(c.lineas_inertes, 0, "con BLDY distinto de cero el efecto no es la identidad");
}

#[test]
#[cfg(not(feature = "perf-counters"))]
fn test_normal_build_has_no_perf_overhead() {
    let mut m = Machine::new(dummy_rom());
    m.run_frame(300_000);
    assert!(m.bus.frames >= 1);
}

#[test]
fn test_fidelidad_visual_framebuffer_con_y_sin_perf_counters() {
    let mut m1 = Machine::new(dummy_rom());
    m1.run_frame(300_000);
    let fb1 = m1.bus.framebuffer;

    let mut m2 = Machine::new(dummy_rom());
    m2.run_frame(300_000);
    let fb2 = m2.bus.framebuffer;

    assert_eq!(fb1, fb2, "El resultado de dibujo debe ser 100% determinista e idéntico");
}

#[test]
#[cfg(feature = "perf-counters")]
fn test_apu_usa_perf_clock_y_acumula_tiempo() {
    let mem_src_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("mem.rs");
    let mem_src = std::fs::read_to_string(&mem_src_path).expect("Debe poder leerse src/mem.rs");
    assert!(
        !mem_src.contains("Instant::now"),
        "src/mem.rs no debe invocar std::time::Instant::now en el camino caliente de pump_audio; debe usar crate::perf_clock::ticks()"
    );

    let mut m = Machine::new(dummy_rom());
    m.bus.perf_audio_timing = true;
    m.run_frame(300_000);
    let c = m.take_perf_counters();
    assert!(c.apu_ns > 0, "apu_ns debe registrar tiempo acumulado positivo con perf_clock (obtenido: {} ns)", c.apu_ns);

    let reset = m.take_perf_counters();
    assert_eq!(reset.apu_ns, 0, "take_perf_counters debe reiniciar apu_ns a 0");
}

#[test]
fn test_apu_silencio_conserva_frame_acc_y_expiracion() {
    let mut m = Machine::new(dummy_rom());
    // Encender audio maestro (SOUNDCNT_X = 0x80) con los 4 canales PSG apagados
    m.bus.write8(0x0400_0084, 0x80);
    assert_eq!(m.bus.read8(0x0400_0084) & 0x0F, 0);

    // Avanzar medio período del secuenciador de 64 Hz (131_072 ciclos = 512 muestras de 256 ciclos)
    for _ in 0..512 {
        m.bus.tick(256);
    }
    assert!(
        !m.bus.audio_buf.is_empty() && m.bus.audio_buf.iter().all(|&s| s == 0),
        "Todas las muestras con canales apagados y FIFO en cero deben ser 0"
    );

    // Activar canal 1 con duración mínima (length = 64 - 63 = 1 tick de 256 Hz)
    m.bus.write8(0x0400_0062, 0x3F);
    m.bus.write8(0x0400_0063, 0xF0);
    m.bus.write8(0x0400_0065, 0xC0);
    assert_eq!(m.bus.read8(0x0400_0084) & 0x01, 1, "Canal 1 debe estar activo tras el trigger");

    // Como frame_acc ya llevaba 131_072 ciclos acumulados en silencio, al avanzar otros
    // 131_072 ciclos (512 muestras) debe completarse exactamente el frame de 64 Hz (262_144 ciclos)
    // y expirar el canal 1.
    for _ in 0..512 {
        m.bus.tick(256);
    }
    assert_eq!(
        m.bus.read8(0x0400_0084) & 0x01,
        0,
        "El canal 1 debe expirar al completarse los 262_144 ciclos acumulados desde el silencio"
    );
}

#[test]
fn test_regresion_paridad_audio_y_video() {
    let setup = |m: &mut Machine| {
        m.bus.tap_channels = true;
        // Configurar audio maestro, PSG canal 1 y FIFO A con Timer 0
        m.bus.write8(0x0400_0084, 0x80);
        m.bus.write8(0x0400_0080, 0x77);
        m.bus.write8(0x0400_0081, 0x11);
        m.bus.write16(0x0400_0082, 0x0B02);
        m.bus.write8(0x0400_0062, 0x80);
        m.bus.write8(0x0400_0063, 0xF0);
        m.bus.write8(0x0400_0064, 0x00);
        m.bus.write8(0x0400_0065, 0x84);
        m.bus.write32(0x0400_00A0, 0x4030_2010);
        m.bus.write16(0x0400_0100, 0xFF00);
        m.bus.write16(0x0400_0102, 0x0080);
        // Configurar fondo Mode 0 BG0
        m.bus.write16(0x0400_0000, 0x0100);
        m.bus.write16(0x0500_0000, 0x1234);
    };

    let mut m1 = Machine::new(dummy_rom());
    setup(&mut m1);
    m1.bus.frame_ready = false;
    let mut steps = 0;
    while !m1.bus.frame_ready && steps < 300_000 {
        m1.step();
        let c = m1.bus.clock;
        m1.bus.tick(0);
        assert_eq!(m1.bus.clock, c);
        steps += 1;
    }

    let mut m2 = Machine::new(dummy_rom());
    setup(&mut m2);
    m2.run_frame(300_000);

    assert!(!m1.bus.audio_buf.is_empty(), "audio_buf debe contener muestras generadas");
    assert!(!m1.bus.psg_tap.is_empty(), "psg_tap debe contener muestras cuando tap_channels=true");
    assert!(!m1.bus.fifo_tap[0].is_empty(), "fifo_tap[0] debe registrar eventos DAC");
    assert_eq!(m1.bus.audio_buf, m2.bus.audio_buf);
    assert_eq!(m1.bus.psg_tap, m2.bus.psg_tap);
    assert_eq!(m1.bus.fifo_tap[0], m2.bus.fifo_tap[0]);
    assert_eq!(m1.bus.framebuffer, m2.bus.framebuffer);
}

#[test]
#[cfg(feature = "perf-counters")]
fn test_bombeo_audio_diferido_en_bloques_y_eventos_video() {
    let mut m = Machine::new(dummy_rom());
    m.bus.perf_audio_timing = true;
    let mptr = &mut m as *mut Machine as *mut core::ffi::c_void;

    // Avanzar ~5 líneas visibles mediante RT_API.tick (24 bloques de 256 ciclos = 6144 ciclos)
    for _ in 0..24 {
        (gba_core::capi::RT_API.tick)(mptr, 256);
    }
    // Avanzar otras 10 fases de video (HBlank / LineEnd) mediante skip_to_next_event
    for _ in 0..10 {
        m.bus.skip_to_next_event();
    }
    assert!(
        m.bus.scanline() > 0 && m.bus.scanline() < 160,
        "Debe seguir dentro de las líneas visibles antes de VBlank"
    );
    assert_eq!(
        m.bus.audio_buf.len(),
        0,
        "No debe bombear audio en ticks ni en eventos HBlank/LineEnd sin eventos de audio"
    );
    let c_mid = m.take_perf_counters();
    assert_eq!(c_mid.apu_ns, 0, "apu_ns debe permanecer en 0 mientras no se invoque pump_audio");

    // Completar el fotograma hasta VBlank (line == 160)
    m.run_frame(300_000);
    assert!(m.bus.frame_ready);
    assert_eq!(
        m.bus.audio_buf.len(),
        1542,
        "Al llegar a VBlank debe haber generado exactamente las 771 muestras estéreo (1542 i16) del fotograma"
    );
    let c_end = m.take_perf_counters();
    assert!(c_end.apu_ns > 0, "Al bombear en VBlank debe registrar el tiempo acumulado en apu_ns");
}

#[test]
fn test_bombeo_audio_sincroniza_en_lectura_escritura_y_fifo() {
    let mut m = Machine::new(dummy_rom());
    let mptr = &mut m as *mut Machine as *mut core::ffi::c_void;

    // 1. Encender audio maestro y canal 1 con duración mínima (expira tras 262_144 ciclos)
    m.bus.write8(0x0400_0084, 0x80);
    m.bus.write8(0x0400_0062, 0x3F);
    m.bus.write8(0x0400_0063, 0xF0);
    m.bus.write8(0x0400_0065, 0xC0);
    m.bus.audio_buf.clear();

    // Avanzar 1026 bloques de 256 ciclos (262_656 ciclos > 262_144) usando RT_API.tick.
    // Cruza VBlank en 197_120 ciclos (bombeando hasta ahí) y luego acumula ~65_536 ciclos sin bombear.
    for _ in 0..1026 {
        (gba_core::capi::RT_API.tick)(mptr, 256);
    }
    let len_antes_lectura = m.bus.audio_buf.len();
    assert!(
        len_antes_lectura < 2054,
        "Antes de leer SOUNDCNT_X, el tramo posterior a VBlank sigue diferido (len={})",
        len_antes_lectura
    );

    // Al leer SOUNDCNT_X (0x0400_0084), io_read8 debe bombear hasta self.clock y expirar el canal 1
    assert_eq!(
        m.bus.read8(0x0400_0084) & 0x01,
        0,
        "La lectura de SOUNDCNT_X debe sincronizar el APU y reportar el canal 1 expirado"
    );
    assert_eq!(
        m.bus.audio_buf.len(),
        2052,
        "La lectura de SOUNDCNT_X debe haber bombeado el audio hasta el ciclo actual"
    );
}

#[test]
#[cfg(feature = "perf-counters")]
fn test_contadores_de_pump_cuentan_llamadas_y_muestras() {
    let mut m = Machine::new(dummy_rom());
    m.run_frame(300_000);
    let c = m.take_perf_counters();
    assert!(c.pump_llamadas > 0, "pump_audio debe registrar llamadas (obtenido: {})", c.pump_llamadas);
    assert!(
        c.pump_muestras >= 770,
        "un fotograma completo genera unas 771 muestras de 65.536 Hz (obtenido: {})",
        c.pump_muestras
    );
    let reset = m.take_perf_counters();
    assert_eq!(reset.pump_llamadas, 0, "take_perf_counters debe reiniciar pump_llamadas");
    assert_eq!(reset.pump_muestras, 0, "take_perf_counters debe reiniciar pump_muestras");
}

#[test]
#[cfg(feature = "perf-counters")]
fn test_sin_interruptor_no_se_lee_el_reloj_del_apu() {
    let mut m = Machine::new(dummy_rom());
    m.run_frame(300_000);
    let c = m.take_perf_counters();
    assert_eq!(c.apu_ns, 0, "con el interruptor apagado apu_ns no debe acumularse");
    assert!(c.pump_llamadas > 0, "los contadores enteros siguen activos (obtenido: {})", c.pump_llamadas);
    assert!(c.pump_muestras >= 770, "las muestras se cuentan con el interruptor apagado");
}
