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

    // take_perf_counters debe resetear los contadores a cero
    let reset = m.take_perf_counters();
    assert_eq!(reset.ppu_ns, 0);
    assert_eq!(reset.apu_ns, 0);
    assert_eq!(reset.reads, 0);
    assert_eq!(reset.writes, 0);
}

#[test]
#[cfg(not(feature = "perf-counters"))]
fn test_normal_build_has_no_perf_overhead() {
    let mut m = Machine::new(dummy_rom());
    m.run_frame(300_000);
    assert!(m.bus.frames >= 1);
}
