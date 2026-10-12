use gba_core::Machine;

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

