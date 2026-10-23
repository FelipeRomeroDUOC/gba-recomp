//! Scanline PPU renderer (hardware reference, "LCD Video Controller").
//!
//! Renders one visible scanline at HBlank into the RGB555 framebuffer:
//! - modes 0-2: text backgrounds (4/8bpp tiles, flips, scrolling) and
//!   affine backgrounds (8bpp, optional wrap, per-line reference stepping);
//! - modes 3-5: bitmap backgrounds (16bpp, 8bpp paged, small paged);
//! - sprites: 1D/2D mapping, 4/8bpp, flips, affine (incl. double-size),
//!   per-pixel priority against backgrounds, semi-transparent mode,
//!   OBJ-window mode;
//! - windows 0/1, OBJ window, outside region;
//! - color effects: alpha blend, brightness up/down, semi-transparent
//!   sprites forcing alpha against second targets;
//! - backdrop (palette entry 0).
//!
//! Mosaic is not yet implemented. Mid-scanline raster effects are out of
//! scope by design (scanline granularity; see docs/architecture.md §6).

use crate::mem::{MemMap, VISIBLE_WIDTH};

const TRANSPARENT: u16 = 0xFFFF;

/// One line of layer output: RGB555 color or TRANSPARENT.
type Line = [u16; VISIBLE_WIDTH];

#[derive(Clone, Copy)]
struct ObjPixel {
    color: u16,
    priority: u8,
    semi: bool,
}

const OBJ_EMPTY: ObjPixel = ObjPixel {
    color: TRANSPARENT,
    priority: 0xFF,
    semi: false,
};

fn io16(mem: &MemMap, off: usize) -> u16 {
    u16::from_le_bytes([mem.io[off], mem.io[off + 1]])
}

#[inline(always)]
fn pal16(mem: &MemMap, index: usize) -> u16 {
    let off = index * 2;
    u16::from_le_bytes([mem.palette[off], mem.palette[off + 1]]) & 0x7FFF
}

/// Marca de compilación del camino caliente de la composición.
///
/// Viaja al encabezado de la telemetría del port para que un registro diga por sí
/// solo qué versión del renderizador lo produjo; se cambia al tocar ese camino.
pub const PPU_CAMINO_VERSION: &str = "recto-3";

pub fn render_scanline(mem: &mut MemMap, line: usize) {
    let dispcnt = io16(mem, 0x00);

    if dispcnt & 0x80 != 0 {
        // Forced blank: white.
        #[cfg(feature = "perf-counters")]
        let t_copy = crate::perf_clock::ticks();

        let row = &mut mem.framebuffer[line * VISIBLE_WIDTH..(line + 1) * VISIBLE_WIDTH];
        row.fill(0x7FFF);

        #[cfg(feature = "perf-counters")]
        {
            let copy_ns = crate::perf_clock::ns_since(t_copy);
            mem.perf_counters.copy_ns += copy_ns;
            mem.perf_counters.ppu_ns += copy_ns;
        }
        return;
    }

    // Latch / step the affine reference accumulators.
    if line == 0 || mem.aff_dirty[0] {
        mem.aff_ref[0] = sign_extend28(read_io32(mem, 0x28));
        mem.aff_ref[1] = sign_extend28(read_io32(mem, 0x2C));
        mem.aff_dirty[0] = false;
    }
    if line == 0 || mem.aff_dirty[1] {
        mem.aff_ref[2] = sign_extend28(read_io32(mem, 0x38));
        mem.aff_ref[3] = sign_extend28(read_io32(mem, 0x3C));
        mem.aff_dirty[1] = false;
    }

    #[cfg(feature = "perf-counters")]
    let t_bg = crate::perf_clock::ticks();

    let mode = dispcnt & 7;
    let mut bg_lines = [[TRANSPARENT; VISIBLE_WIDTH]; 4];
    let mut bg_used = [false; 4];

    match mode {
        0 => {
            for bg in 0..4 {
                if dispcnt & (0x100 << bg) != 0 {
                    render_text_bg(mem, bg, line, &mut bg_lines[bg]);
                    bg_used[bg] = true;
                }
            }
        }
        1 => {
            for bg in 0..2 {
                if dispcnt & (0x100 << bg) != 0 {
                    render_text_bg(mem, bg, line, &mut bg_lines[bg]);
                    bg_used[bg] = true;
                }
            }
            if dispcnt & 0x400 != 0 {
                render_affine_bg(mem, 2, &mut bg_lines[2]);
                bg_used[2] = true;
            }
        }
        2 => {
            for bg in 2..4 {
                if dispcnt & (0x100 << bg) != 0 {
                    render_affine_bg(mem, bg, &mut bg_lines[bg]);
                    bg_used[bg] = true;
                }
            }
        }
        3 | 4 | 5 => {
            if dispcnt & 0x400 != 0 {
                render_bitmap_bg(mem, mode, dispcnt, line, &mut bg_lines[2]);
                bg_used[2] = true;
            }
        }
        _ => {}
    }

    #[cfg(feature = "perf-counters")]
    let t_sprite = crate::perf_clock::ticks();
    #[cfg(feature = "perf-counters")]
    {
        // `ppu_ns` is the sum of its parts: the scanline carries no timer of
        // its own, so nothing is attributed twice and nothing is left over.
        let bg_ns = crate::perf_clock::ns_between(t_bg, t_sprite);
        mem.perf_counters.bg_ns += bg_ns;
        mem.perf_counters.ppu_ns += bg_ns;
        mem.perf_counters.fondos_activos += bg_used.iter().filter(|&&u| u).count() as u64;
    }

    // Sprites.
    let mut obj_line = [OBJ_EMPTY; VISIBLE_WIDTH];
    let mut obj_window = [false; VISIBLE_WIDTH];
    let mut obj_semi = false;
    if dispcnt & 0x1000 != 0 {
        obj_semi = render_sprites(mem, dispcnt, line, &mut obj_line, &mut obj_window);
        #[cfg(feature = "perf-counters")]
        if obj_semi {
            mem.perf_counters.lineas_con_sprite_semi += 1;
        }
    }

    #[cfg(feature = "perf-counters")]
    let t_compose = crate::perf_clock::ticks();
    #[cfg(feature = "perf-counters")]
    {
        let sprite_ns = crate::perf_clock::ns_between(t_sprite, t_compose);
        mem.perf_counters.sprite_ns += sprite_ns;
        mem.perf_counters.ppu_ns += sprite_ns;
    }

    // Step affine accumulators for the next line (PB/PD).
    mem.aff_ref[0] = mem.aff_ref[0].wrapping_add(io16(mem, 0x22) as i16 as i32);
    mem.aff_ref[1] = mem.aff_ref[1].wrapping_add(io16(mem, 0x26) as i16 as i32);
    mem.aff_ref[2] = mem.aff_ref[2].wrapping_add(io16(mem, 0x32) as i16 as i32);
    mem.aff_ref[3] = mem.aff_ref[3].wrapping_add(io16(mem, 0x36) as i16 as i32);

    compose(
        mem,
        dispcnt,
        line,
        &bg_lines,
        &bg_used,
        &obj_line,
        &obj_window,
        obj_semi,
    );

    #[cfg(feature = "perf-counters")]
    {
        let compose_ns = crate::perf_clock::ns_since(t_compose);
        mem.perf_counters.compose_ns += compose_ns;
        mem.perf_counters.ppu_ns += compose_ns;
    }
}

fn read_io32(mem: &MemMap, off: usize) -> u32 {
    u32::from_le_bytes([
        mem.io[off],
        mem.io[off + 1],
        mem.io[off + 2],
        mem.io[off + 3],
    ])
}

fn sign_extend28(v: u32) -> i32 {
    ((v << 4) as i32) >> 4
}

// ---- text backgrounds ----

fn render_text_bg(mem: &MemMap, bg: usize, line: usize, out: &mut Line) {
    let cnt = io16(mem, 0x08 + bg * 2);
    let char_base = ((cnt >> 2) & 3) as usize * 0x4000;
    let eight_bpp = cnt & 0x80 != 0;
    let screen_base = ((cnt >> 8) & 0x1F) as usize * 0x800;
    let size = (cnt >> 14) & 3;

    let hofs = (io16(mem, 0x10 + bg * 4) & 0x1FF) as usize;
    let vofs = (io16(mem, 0x12 + bg * 4) & 0x1FF) as usize;

    let (w_tiles, h_tiles) = match size {
        0 => (32, 32),
        1 => (64, 32),
        2 => (32, 64),
        _ => (64, 64),
    };

    let y = (line + vofs) & (h_tiles * 8 - 1);
    let ty = y / 8;
    let py = y % 8;

    // Walk the line in tile spans: all pixels within one span share the
    // same map entry, so fetch and decode it once (per-pixel results are
    // identical to the previous per-pixel formulation).
    let mut sx = 0usize;
    while sx < VISIBLE_WIDTH {
        let x = (sx + hofs) & (w_tiles * 8 - 1);
        let tx = x / 8;
        let px = x % 8;
        let span = (8 - px).min(VISIBLE_WIDTH - sx);

        // Screenblock layout: 32x32-tile blocks, left-to-right then down.
        let block = (ty / 32) * (w_tiles / 32) + tx / 32;
        let entry_idx = block * 1024 + (ty % 32) * 32 + (tx % 32);
        let entry_off = screen_base + entry_idx * 2;
        let entry = u16::from_le_bytes([mem.vram[entry_off], mem.vram[entry_off + 1]]);

        let tile = (entry & 0x3FF) as usize;
        let hflip = entry & 0x400 != 0;
        let vflip = entry & 0x800 != 0;
        let fy = if vflip { 7 - py } else { py };

        if eight_bpp {
            let base = char_base + tile * 64 + fy * 8;
            // The whole tile row either fits in VRAM or it does not, so the
            // check is hoisted out of the pixel loop.
            let visible = base + 8 <= 0x1_0000;
            for i in 0..span {
                let fx = if hflip { 7 - (px + i) } else { px + i };
                let color_idx = if visible { mem.vram[base + fx] as usize } else { 0 };
                out[sx + i] = if color_idx == 0 {
                    TRANSPARENT
                } else {
                    pal16(mem, color_idx)
                };
            }
        } else {
            let base = char_base + tile * 32 + fy * 4;
            let pal_base = ((entry >> 12) as usize & 0xF) * 16;
            let row_bytes = if base + 4 <= 0x1_0000 {
                u32::from_le_bytes([
                    mem.vram[base],
                    mem.vram[base + 1],
                    mem.vram[base + 2],
                    mem.vram[base + 3],
                ])
            } else {
                0
            };

            if row_bytes == 0 {
                // Empty tile row: all eight pixels are transparent, so the span
                // is filled at once instead of decoded pixel by pixel.
                out[sx..sx + span].fill(TRANSPARENT);
                sx += span;
                continue;
            }
            for i in 0..span {
                let fx = if hflip { 7 - (px + i) } else { px + i };
                let nib = ((row_bytes >> (fx * 4)) & 0xF) as usize;
                out[sx + i] = if nib == 0 {
                    TRANSPARENT
                } else {
                    pal16(mem, pal_base + nib)
                };
            }
        }
        sx += span;
    }
}

// ---- affine backgrounds ----

fn render_affine_bg(mem: &MemMap, bg: usize, out: &mut Line) {
    let cnt = io16(mem, 0x08 + bg * 2);
    let char_base = ((cnt >> 2) & 3) as usize * 0x4000;
    let screen_base = ((cnt >> 8) & 0x1F) as usize * 0x800;
    let wrap = cnt & 0x2000 != 0;
    let size_tiles = 16usize << ((cnt >> 14) & 3); // 16,32,64,128
    let size_px = (size_tiles * 8) as i32;

    let (pa, pc, ref_base) = if bg == 2 {
        (
            io16(mem, 0x20) as i16 as i32,
            io16(mem, 0x24) as i16 as i32,
            0,
        )
    } else {
        (
            io16(mem, 0x30) as i16 as i32,
            io16(mem, 0x34) as i16 as i32,
            2,
        )
    };
    let mut fx = mem.aff_ref[ref_base];
    let mut fy = mem.aff_ref[ref_base + 1];

    for out_px in out.iter_mut() {
        let mut tx = fx >> 8;
        let mut ty = fy >> 8;
        fx = fx.wrapping_add(pa);
        fy = fy.wrapping_add(pc);

        if wrap {
            tx = tx.rem_euclid(size_px);
            ty = ty.rem_euclid(size_px);
        } else if tx < 0 || ty < 0 || tx >= size_px || ty >= size_px {
            *out_px = TRANSPARENT;
            continue;
        }
        let (tx, ty) = (tx as usize, ty as usize);

        let entry_off = screen_base + (ty / 8) * size_tiles + tx / 8;
        let tile = mem.vram[entry_off] as usize;
        let addr = char_base + tile * 64 + (ty % 8) * 8 + tx % 8;
        let color_idx = if addr < 0x1_0000 {
            mem.vram[addr] as usize
        } else {
            0
        };
        *out_px = if color_idx == 0 {
            TRANSPARENT
        } else {
            pal16(mem, color_idx)
        };
    }
}

// ---- bitmap backgrounds ----

fn render_bitmap_bg(mem: &MemMap, mode: u16, dispcnt: u16, line: usize, out: &mut Line) {
    let page = if dispcnt & 0x10 != 0 { 0xA000usize } else { 0 };
    match mode {
        3 => {
            for (x, out_px) in out.iter_mut().enumerate() {
                let off = (line * VISIBLE_WIDTH + x) * 2;
                let c = u16::from_le_bytes([mem.vram[off], mem.vram[off + 1]]) & 0x7FFF;
                *out_px = c;
            }
        }
        4 => {
            for (x, out_px) in out.iter_mut().enumerate() {
                let idx = mem.vram[page + line * VISIBLE_WIDTH + x] as usize;
                *out_px = if idx == 0 {
                    TRANSPARENT
                } else {
                    pal16(mem, idx)
                };
            }
        }
        _ => {
            // Mode 5: 160x128 16bpp, paged.
            for (x, out_px) in out.iter_mut().enumerate() {
                if x < 160 && line < 128 {
                    let off = page + (line * 160 + x) * 2;
                    let c = u16::from_le_bytes([mem.vram[off], mem.vram[off + 1]]) & 0x7FFF;
                    *out_px = c;
                } else {
                    *out_px = TRANSPARENT;
                }
            }
        }
    }
}

// ---- sprites ----

const OBJ_SIZES: [[(i32, i32); 4]; 3] = [
    [(8, 8), (16, 16), (32, 32), (64, 64)], // square
    [(16, 8), (32, 8), (32, 16), (64, 32)], // horizontal
    [(8, 16), (8, 32), (16, 32), (32, 64)], // vertical
];

/// Returns true if any semi-transparent sprite pixel was written
/// (conservative: a later overwrite by an opaque sprite still counts).
fn render_sprites(
    mem: &mut MemMap,
    dispcnt: u16,
    line: usize,
    out: &mut [ObjPixel; VISIBLE_WIDTH],
    window: &mut [bool; VISIBLE_WIDTH],
) -> bool {
    let one_dim = dispcnt & 0x40 != 0;
    let bitmap_mode = (dispcnt & 7) >= 3;
    let line = line as i32;
    // Reported to `compose`, which otherwise resolves the line with the straight
    // path: finding it here is free, whereas scanning the line for it costs more
    // than the path saves.
    let mut semi = false;

    for i in 0..128 {
        // Attribute 0 decides whether the rest is worth reading: disabled and
        // prohibited entries are the bulk of OAM, and reading all three
        // attributes cost three 16-bit reads per entry per scanline.
        let a0 = u16::from_le_bytes([mem.oam[i * 8], mem.oam[i * 8 + 1]]);

        let affine = a0 & 0x100 != 0;
        if !affine && a0 & 0x200 != 0 {
            continue; // disabled
        }
        let shape = ((a0 >> 14) & 3) as usize;
        if shape == 3 {
            continue; // prohibited
        }
        let a1 = u16::from_le_bytes([mem.oam[i * 8 + 2], mem.oam[i * 8 + 3]]);
        let size_idx = ((a1 >> 14) & 3) as usize;
        let (w, h) = OBJ_SIZES[shape][size_idx];
        let double = affine && a0 & 0x200 != 0;
        let (bw, bh) = if double { (w * 2, h * 2) } else { (w, h) };

        let mut y = (a0 & 0xFF) as i32;
        if y >= 160 {
            y -= 256;
        }
        let mut x = (a1 & 0x1FF) as i32;
        if x >= 240 {
            x -= 512;
        }
        if line < y || line >= y + bh {
            continue;
        }

        // Only entries that cover this scanline need their tile/palette data.
        let a2 = u16::from_le_bytes([mem.oam[i * 8 + 4], mem.oam[i * 8 + 5]]);
        let mode = (a0 >> 10) & 3;
        let eight_bpp = a0 & 0x2000 != 0;
        let tile_base = (a2 & 0x3FF) as usize;
        let priority = ((a2 >> 10) & 3) as u8;
        let palette = ((a2 >> 12) & 0xF) as usize;

        // Affine parameters.
        let (pa, pb, pc, pd) = if affine {
            let g = ((a1 >> 9) & 0x1F) as usize * 32;
            let p = |off: usize| {
                u16::from_le_bytes([mem.oam[g + off], mem.oam[g + off + 1]]) as i16 as i32
            };
            (p(6), p(14), p(22), p(30))
        } else {
            (0x100, 0, 0, 0x100)
        };

        let row_tiles = if one_dim {
            (w / 8) * if eight_bpp { 2 } else { 1 }
        } else {
            32
        };

        let ly = line - y;
        let min_bx = (-x).max(0);
        let max_bx = (VISIBLE_WIDTH as i32 - x).min(bw);
        if min_bx >= max_bx {
            continue;
        }

        #[cfg(feature = "perf-counters")]
        {
            mem.perf_counters.sprites_visibles += 1;
        }

        let non_affine_v = if !affine {
            let mut v = ly;
            if a1 & 0x2000 != 0 {
                v = h - 1 - v;
            }
            Some((v / 8, v % 8))
        } else {
            None
        };

        for bx in min_bx..max_bx {
            let sx_usize = (x + bx) as usize;

            // Texture coordinates.
            let (u, v) = if affine {
                let dx = bx - bw / 2;
                let dy = ly - bh / 2;
                let u = ((pa * dx + pb * dy) >> 8) + w / 2;
                let v = ((pc * dx + pd * dy) >> 8) + h / 2;
                if u < 0 || v < 0 || u >= w || v >= h {
                    continue;
                }
                (u, v)
            } else {
                let mut u = bx;
                if a1 & 0x1000 != 0 {
                    u = w - 1 - u;
                }
                (u, 0)
            };

            let (tu, pu) = (u / 8, u % 8);
            let (tv, pv) = if let Some((ntv, npv)) = non_affine_v {
                (ntv, npv)
            } else {
                (v / 8, v % 8)
            };
            let tile_step = if eight_bpp { 2 } else { 1 };
            let tile = tile_base + tv as usize * row_tiles as usize + tu as usize * tile_step;
            // In bitmap modes the first 512 tiles overlap the bitmap.
            if bitmap_mode && tile < 512 {
                continue;
            }
            let addr = 0x1_0000
                + if eight_bpp {
                    (tile / 2) * 64 + pv as usize * 8 + pu as usize
                } else {
                    tile * 32 + pv as usize * 4 + pu as usize / 2
                };
            if addr >= 0x1_8000 {
                continue;
            }
            let color_idx = if eight_bpp {
                mem.vram[addr] as usize
            } else {
                let b = mem.vram[addr];
                let nib = if pu & 1 == 0 { b & 0xF } else { b >> 4 } as usize;
                if nib == 0 {
                    0
                } else {
                    palette * 16 + nib
                }
            };
            if color_idx == 0 {
                continue;
            }
            let color = pal16(mem, 256 + color_idx);

            if mode == 2 {
                window[sx_usize] = true;
            } else if out[sx_usize].priority == 0xFF || priority < out[sx_usize].priority {
                if mode == 1 {
                    semi = true;
                }
                out[sx_usize] = ObjPixel {
                    color,
                    priority,
                    semi: mode == 1,
                };
            }
        }
    }
    semi
}

// ---- composition ----

/// Efecto de color que una línea puede aplicar sin mirar la segunda capa: con los modos 2 y 3
/// el resultado depende solo del color de la capa de arriba, así que la composición recta puede
/// aplicarlo al color ganador en lugar de resolver el par de capas píxel a píxel.
///
/// El modo 1 (alpha) no entra aquí a propósito: necesita las dos capas de verdad.
#[derive(Clone, Copy)]
struct EfectoTramo {
    /// `true` = oscurecer (modo 3), `false` = aclarar (modo 2).
    oscurecer: bool,
    /// `BLDY`: 1..16. Con 0 el efecto es la identidad y no se aplica nada.
    evy: u32,
    /// Bits 0-5 de `BLDCNT`: capas objetivo del efecto. Sin ninguno no cambia ningún píxel.
    objetivos: u16,
}

impl EfectoTramo {
    #[inline(always)]
    fn aplica(self, c: u16) -> u16 {
        if self.oscurecer {
            darken(c, self.evy)
        } else {
            brighten(c, self.evy)
        }
    }
}

// Cuenta de llamadas a `top_two_layers`, solo para tests: es la prueba sin cronómetro de que los
// caminos especializados no buscan la segunda capa en cada píxel. La cuenta es **por hilo**, no
// global, porque los tests corren en paralelo y una cuenta compartida mezclaría las líneas de un
// test con las de otro (la cuenta tiene que describir la línea que el propio test acaba de dibujar).
#[cfg(test)]
std::thread_local! {
    static BUSQUEDAS_DOS_CAPAS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Interruptor de los tests: apaga la vía rápida para comparar los dos caminos píxel a píxel.
/// En un binario normal esta función no existe y la vía rápida nunca se apaga.
#[cfg(test)]
static SIN_VIA_RAPIDA: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
#[inline(always)]
fn via_rapida_apagada() -> bool {
    SIN_VIA_RAPIDA.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(not(test))]
#[inline(always)]
fn via_rapida_apagada() -> bool {
    false
}

/// Interruptor de diagnóstico (feature `perf-video`): fuerza que el alpha del modo 1 pase por la
/// vía por tramos incluso en líneas con sprite translúcido. Sirve para comparar los dos caminos
/// sobre la misma línea; en un binario sin `perf-video` compila a `false`.
#[cfg(feature = "perf-video")]
static FORZAR_ALPHA_TRAMOS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(feature = "perf-video")]
pub fn forzar_alpha_tramos(activo: bool) {
    FORZAR_ALPHA_TRAMOS.store(activo, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(feature = "perf-video")]
#[inline(always)]
fn alpha_tramos_forzado() -> bool {
    FORZAR_ALPHA_TRAMOS.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(not(feature = "perf-video"))]
#[inline(always)]
fn alpha_tramos_forzado() -> bool {
    false
}

/// Interruptor de diagnóstico (`perf-video`): manda todas las líneas al camino general por píxel,
/// que es la referencia de fidelidad. Su gemelo de los tests es `SIN_VIA_RAPIDA`.
#[cfg(feature = "perf-video")]
static FORZAR_CAMINO_GENERAL: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(feature = "perf-video")]
pub fn forzar_camino_general(activo: bool) {
    FORZAR_CAMINO_GENERAL.store(activo, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(feature = "perf-video")]
#[inline(always)]
fn camino_general_forzado() -> bool {
    FORZAR_CAMINO_GENERAL.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(not(feature = "perf-video"))]
#[inline(always)]
fn camino_general_forzado() -> bool {
    false
}

/// The two topmost visible layers at one pixel: `(color, layer, second color,
/// second layer)`, with layer ids 0-3 = BGs, 4 = OBJ, 5 = backdrop. OBJ ranks
/// above BGs of equal priority; BGs tie-break by index via the pre-sorted list.
///
/// Callers that know the second layer cannot matter (see `compose`) avoid it.
///
/// Cada llamada suma en `BUSQUEDAS_DOS_CAPAS` (solo existe en los tests): es la prueba sin
/// cronómetro de cuántas veces se busca la segunda capa. La vía rápida de los modos 2 y 3 no
/// ejecuta ninguna; el camino por tramos del alpha, una por tramo de la línea.
#[inline(always)]
fn top_two_layers(
    bg_lines: &[Line; 4],
    bg_prio: &[(u8, usize)],
    control: u16,
    x: usize,
    obj: &ObjPixel,
    backdrop: u16,
) -> (u16, usize, u16, usize) {
    #[cfg(test)]
    BUSQUEDAS_DOS_CAPAS.with(|busquedas| busquedas.set(busquedas.get() + 1));
    let obj_visible = obj.priority != 0xFF && control & 0x10 != 0;
    let mut fc = backdrop;
    let mut fl = 5usize;
    let mut sc = backdrop;
    let mut sl = 5usize;

    let mut found_first = false;
    let mut obj_handled = !obj_visible;

    for &(bp, bg) in bg_prio {
        if !obj_handled && obj.priority <= bp {
            if !found_first {
                fc = obj.color;
                fl = 4;
                found_first = true;
            } else {
                sc = obj.color;
                sl = 4;
                obj_handled = true;
                break;
            }
            obj_handled = true;
        }

        if control & (1 << bg) != 0 {
            let c = bg_lines[bg][x];
            if c != TRANSPARENT {
                if !found_first {
                    fc = c;
                    fl = bg;
                    found_first = true;
                } else {
                    sc = c;
                    sl = bg;
                    break;
                }
            }
        }
    }

    if !obj_handled {
        if !found_first {
            fc = obj.color;
            fl = 4;
        } else {
            sc = obj.color;
            sl = 4;
        }
    }

    (fc, fl, sc, sl)
}

// ---- straight composition path ----

/// Top visible layer of a span with no sprite pixel on it: the first opaque BG in
/// priority order whose control bit allows it, or the backdrop colour.
#[inline(always)]
fn straight_bgs<const N: usize, const CON_EFECTO: bool>(
    lines: &[&[u16]; N],
    bg_prio: &[(u8, usize); N],
    control: u16,
    backdrop: u16,
    efecto: EfectoTramo,
    dst: &mut [u16],
) {
    // Con `CON_EFECTO` la línea trae un aclarado u oscurecido real: el color ganador pasa por el
    // efecto si su capa es objetivo de `BLDCNT`, y el fondo, si lo es, se transforma una sola vez
    // por tramo. Sin efecto (`CON_EFECTO = false`) el bucle compila igual que antes.
    let efectos = control & 0x20 != 0;
    let fondo = if CON_EFECTO && efectos && efecto.objetivos & 0x20 != 0 {
        efecto.aplica(backdrop)
    } else {
        backdrop
    };
    for (x, d) in dst.iter_mut().enumerate() {
        let mut color = fondo;
        for k in 0..N {
            let (_, bg) = bg_prio[k];
            if control & (1 << bg) != 0 {
                let c = lines[k][x];
                if c != TRANSPARENT {
                    color = if CON_EFECTO && efectos && efecto.objetivos & (1 << bg) != 0 {
                        efecto.aplica(c)
                    } else {
                        c
                    };
                    break;
                }
            }
        }
        *d = color;
    }
}

/// Same, with the sprite layer present: a sprite pixel wins when its priority is
/// not worse than the BG's, which is why it cuts the walk short.
#[inline(always)]
fn straight_obj<const N: usize, const CON_EFECTO: bool>(
    lines: &[&[u16]; N],
    bg_prio: &[(u8, usize); N],
    control: u16,
    obj_span: &[ObjPixel],
    backdrop: u16,
    efecto: EfectoTramo,
    dst: &mut [u16],
) {
    let efectos = control & 0x20 != 0;
    let fondo = if CON_EFECTO && efectos && efecto.objetivos & 0x20 != 0 {
        efecto.aplica(backdrop)
    } else {
        backdrop
    };
    // La capa 4 es el sprite para el reparto de objetivos de `BLDCNT`.
    let sprite_efecto = CON_EFECTO && efectos && efecto.objetivos & 0x10 != 0;
    for (x, d) in dst.iter_mut().enumerate() {
        let obj = &obj_span[x];
        let mut color = fondo;
        if obj.priority != 0xFF {
            let op = obj.priority;
            let mut chosen = false;
            for k in 0..N {
                let (bp, bg) = bg_prio[k];
                if op <= bp {
                    color = if sprite_efecto { efecto.aplica(obj.color) } else { obj.color };
                    chosen = true;
                    break;
                }
                if control & (1 << bg) != 0 {
                    let c = lines[k][x];
                    if c != TRANSPARENT {
                        color = if CON_EFECTO && efectos && efecto.objetivos & (1 << bg) != 0 {
                            efecto.aplica(c)
                        } else {
                            c
                        };
                        chosen = true;
                        break;
                    }
                }
            }
            if !chosen {
                // No opaque BG under it: the sprite pixel is all there is.
                color = if sprite_efecto { efecto.aplica(obj.color) } else { obj.color };
            }
        } else {
            for k in 0..N {
                let (_, bg) = bg_prio[k];
                if control & (1 << bg) != 0 {
                    let c = lines[k][x];
                    if c != TRANSPARENT {
                        color = if CON_EFECTO && efectos && efecto.objetivos & (1 << bg) != 0 {
                            efecto.aplica(c)
                        } else {
                            c
                        };
                        break;
                    }
                }
            }
        }
        *d = color;
    }
}

/// Composes one span of constant layer control, with the visible-BG list
/// monomorphised by its length (`N`): no window closure, no dynamic indexing and
/// no bounds check per pixel, because `dst` and the BG slices are already
/// clipped to the span.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn straight_span<const N: usize, const CON_EFECTO: bool>(
    bg_lines: &[Line; 4],
    bg_prio: &[(u8, usize)],
    obj_line: &[ObjPixel; VISIBLE_WIDTH],
    control: u16,
    backdrop: u16,
    start: usize,
    efecto: EfectoTramo,
    dst: &mut [u16],
) {
    debug_assert_eq!(bg_prio.len(), N);
    let mut pares: [(u8, usize); N] = [(0, 0); N];
    pares.copy_from_slice(bg_prio);
    let end = start + dst.len();
    let lines: [&[u16]; N] = core::array::from_fn(|k| &bg_lines[pares[k].1][start..end]);
    let obj_span = &obj_line[start..end];
    if control & 0x10 != 0 {
        straight_obj::<N, CON_EFECTO>(&lines, &pares, control, obj_span, backdrop, efecto, dst);
    } else {
        straight_bgs::<N, CON_EFECTO>(&lines, &pares, control, backdrop, efecto, dst);
    }
}

/// Straight composition of a line: each span is a half-open x range with a
/// constant layer control, so the top-layer rule is the same for every pixel
/// inside it.
#[allow(clippy::too_many_arguments)]
fn compose_straight<const CON_EFECTO: bool>(
    mem: &mut MemMap,
    bg_lines: &[Line; 4],
    bg_prio: &[(u8, usize)],
    obj_line: &[ObjPixel; VISIBLE_WIDTH],
    spans: &[(usize, usize, u16)],
    backdrop: u16,
    row: usize,
    efecto: EfectoTramo,
) {
    macro_rules! launch {
        ($n:literal) => {{
            for &(start, end, control) in spans {
                let dst = &mut mem.framebuffer[row + start..row + end];
                straight_span::<$n, CON_EFECTO>(
                    bg_lines, bg_prio, obj_line, control, backdrop, start, efecto, dst,
                );
            }
        }};
    }
    match bg_prio.len() {
        0 => {
            for &(start, end, control) in spans {
                let dst = &mut mem.framebuffer[row + start..row + end];
                if control & 0x10 != 0 {
                    let efectos = CON_EFECTO && control & 0x20 != 0;
                    for (x, d) in dst.iter_mut().enumerate() {
                        let o = &obj_line[start + x];
                        let (color, capa) = if o.priority != 0xFF {
                            (o.color, 4usize)
                        } else {
                            (backdrop, 5usize)
                        };
                        *d = if efectos && efecto.objetivos & (1 << capa) != 0 {
                            efecto.aplica(color)
                        } else {
                            color
                        };
                    }
                } else if CON_EFECTO && control & 0x20 != 0 && efecto.objetivos & 0x20 != 0 {
                    // Sin capas de fondo todo es el fondo: el efecto se aplica una vez.
                    dst.fill(efecto.aplica(backdrop));
                } else {
                    dst.fill(backdrop);
                }
            }
        }
        1 => launch!(1),
        2 => launch!(2),
        3 => launch!(3),
        _ => launch!(4),
    }
}

/// Pesos de la clave de tramo: el bit de `BLDCNT` que le corresponde a cada capa de fondo
/// (0-3) si el control de la ventana la habilita, o 0 si no. Se calculan una vez por tramo.
#[inline(always)]
fn pesos_de_capa(control: u16) -> [u16; 4] {
    let mut pesos = [0u16; 4];
    for (i, peso) in pesos.iter_mut().enumerate() {
        if control & (1u16 << i) != 0 {
            *peso = 1u16 << i;
        }
    }
    pesos
}

/// Clave que resume, para un píxel, todo lo que `top_two_layers` necesita de `x` además del
/// color: qué capas habilitadas son opacas (bits 0-3) y la prioridad del sprite que tapa
/// (bits 8-15, o 0 si no hay sprite visible). Dos píxeles con la misma clave resuelven la
/// misma pareja (capa de arriba, segunda capa), así que un tramo entero se resuelve una vez.
#[inline(always)]
fn clave_de_tramo(
    bg_lines: &[Line; 4],
    obj_line: &[ObjPixel; VISIBLE_WIDTH],
    pesos: [u16; 4],
    x: usize,
) -> u16 {
    let mut mascara = 0u16;
    for i in 0..4 {
        mascara |= if bg_lines[i][x] != TRANSPARENT {
            pesos[i]
        } else {
            0
        };
    }
    let prioridad = obj_line[x].priority;
    mascara
        | if prioridad != 0xFF {
            0x100 | ((prioridad as u16) << 9)
        } else {
            0
        }
}

/// Compone un tramo cuyos píxeles comparten la misma pareja (capa de arriba, segunda capa)
/// y el mismo control de ventana. `arriba` y `segunda` son identificadores de capa como en
/// el resto del módulo: 0-3 fondos, 4 sprite, 5 fondo de pantalla.
#[allow(clippy::too_many_arguments)]
fn componer_tramo(
    mem: &mut MemMap,
    bg_lines: &[Line; 4],
    obj_line: &[ObjPixel; VISIBLE_WIDTH],
    backdrop: u16,
    row: usize,
    inicio: usize,
    fin: usize,
    control: u16,
    arriba: usize,
    segunda: usize,
    bldcnt: u16,
    eva: u32,
    evb: u32,
) {
    // La misma regla del camino general, decidida una vez por tramo en lugar de una vez por
    // píxel: sin efectos dentro de la ventana, o sin la capa de arriba entre los objetivos de
    // `BLDCNT`, el píxel es la capa de arriba sin más.
    let mezcla = control & 0x20 != 0
        && bldcnt & (1u16 << arriba) != 0
        && bldcnt & (0x100u16 << segunda) != 0;
    let dst = &mut mem.framebuffer[row + inicio..row + fin];
    if !mezcla {
        match arriba {
            4 => {
                for (d, o) in dst.iter_mut().zip(&obj_line[inicio..fin]) {
                    *d = o.color;
                }
            }
            5 => dst.fill(backdrop),
            capa => dst.copy_from_slice(&bg_lines[capa][inicio..fin]),
        }
        return;
    }
    // Dos capas de fondo: el bucle caliente no necesita índices ni comprobaciones. Las demás
    // parejas (sprite o fondo de pantalla en juego) se resuelven con el mismo `alpha_blend`
    // del camino general, que es la referencia de fidelidad.
    if arriba < 4 && segunda < 4 {
        let a = &bg_lines[arriba][inicio..fin];
        let b = &bg_lines[segunda][inicio..fin];
        for (d, (ca, cb)) in dst.iter_mut().zip(a.iter().zip(b)) {
            *d = alpha_blend(*ca, *cb, eva, evb);
        }
        return;
    }
    for (k, d) in dst.iter_mut().enumerate() {
        let x = inicio + k;
        let color_de = |capa: usize| -> u16 {
            match capa {
                4 => obj_line[x].color,
                5 => backdrop,
                c => bg_lines[c][x],
            }
        };
        *d = alpha_blend(color_de(arriba), color_de(segunda), eva, evb);
    }
}

/// Composición por tramos del alpha real (`BLDCNT` modo 1): la pareja (capa de arriba, segunda
/// capa) se resuelve una vez por tramo en vez de una vez por píxel.
///
/// Un tramo es un rango de `x` con el mismo control de ventana y la misma clave de tramo, esto
/// es, con las mismas capas opacas habilitadas y la misma prioridad de sprite. Un modo 1 sin
/// segundos objetivos es inerte y lo resuelve la vía rápida, y las líneas con algún sprite
/// translúcido se quedan en el camino general, que es el único que mira `obj.semi`.
#[allow(clippy::too_many_arguments)]
fn compose_alpha_tramos(
    mem: &mut MemMap,
    bg_lines: &[Line; 4],
    bg_prio: &[(u8, usize)],
    obj_line: &[ObjPixel; VISIBLE_WIDTH],
    spans: &[(usize, usize, u16)],
    backdrop: u16,
    row: usize,
    bldcnt: u16,
    eva: u32,
    evb: u32,
) {
    for &(inicio, fin, control) in spans {
        let pesos = pesos_de_capa(control);
        let mut x = inicio;
        while x < fin {
            let (_, arriba, _, segunda) =
                top_two_layers(bg_lines, bg_prio, control, x, &obj_line[x], backdrop);
            let clave = clave_de_tramo(bg_lines, obj_line, pesos, x);
            let mut y = x + 1;
            while y < fin && clave_de_tramo(bg_lines, obj_line, pesos, y) == clave {
                y += 1;
            }
            componer_tramo(
                mem, bg_lines, obj_line, backdrop, row, x, y, control, arriba, segunda, bldcnt, eva,
                evb,
            );
            x = y;
        }
    }
}

/// Layer ids for blend targeting: 0-3 BGs, 4 OBJ, 5 backdrop.
#[allow(clippy::too_many_arguments)]
fn compose(
    mem: &mut MemMap,
    dispcnt: u16,
    line: usize,
    bg_lines: &[Line; 4],
    bg_used: &[bool; 4],
    obj_line: &[ObjPixel; VISIBLE_WIDTH],
    obj_window: &[bool; VISIBLE_WIDTH],
    obj_semi: bool,
) {
    let backdrop = pal16(mem, 0);

    // Background priorities (compose ties: lower BG number wins; sort
    // keys are unique, so unstable sort matches the old stable sort).
    let mut bg_prio_buf = [(0u8, 0usize); 4];
    let mut nbg = 0;
    for bg in 0..4 {
        if bg_used[bg] {
            bg_prio_buf[nbg] = ((io16(mem, 0x08 + bg * 2) & 3) as u8, bg);
            nbg += 1;
        }
    }
    bg_prio_buf[..nbg].sort_unstable();
    let bg_prio = &bg_prio_buf[..nbg];

    let win0_on = dispcnt & 0x2000 != 0;
    let win1_on = dispcnt & 0x4000 != 0;
    let objwin_on = dispcnt & 0x8000 != 0;
    let any_window = win0_on || win1_on || objwin_on;

    let winin = io16(mem, 0x48);
    let winout = io16(mem, 0x4A);

    let win0h = io16(mem, 0x40);
    let win1h = io16(mem, 0x42);
    let win0v = io16(mem, 0x44);
    let win1v = io16(mem, 0x46);

    let in_win0_y = win0_on && in_window_range((win0v >> 8) as u8, win0v as u8, line as u8);
    let in_win1_y = win1_on && in_window_range((win1v >> 8) as u8, win1v as u8, line as u8);

    let bldcnt = io16(mem, 0x50);
    let blend_mode = (bldcnt >> 6) & 3;
    let bldalpha = io16(mem, 0x52);
    let eva = (bldalpha & 0x1F).min(16) as u32;
    let evb = ((bldalpha >> 8) & 0x1F).min(16) as u32;
    let evy = (io16(mem, 0x54) & 0x1F).min(16) as u32;

    let row = line * VISIBLE_WIDTH;

    let line_has_window = in_win0_y || in_win1_y || objwin_on;
    let default_control = if !any_window { 0x3F } else { winout & 0x3F };

    // Straight path. With BLDCNT in mode 0 nothing on this line can be blended,
    // so a pixel's colour is only its top visible layer; and with the OBJ window
    // off, the layer control is constant over x ranges, so those ranges (at most
    // five) are composed one pass each instead of asking the window per pixel.
    // The general path below stays the reference for everything else: windows
    // driven by sprites, and translucent sprite pixels, which do blend even in
    // mode 0.
    // ¿Por qué camino va esta línea? Se decide una vez, y la guarda y las cuatro
    // cuentas salen de esa misma decisión, así que el instrumento no puede
    // divergir de lo que hace el código. La primera razón por la que la vía rápida
    // no aplica es la que se anota, de modo que las cuatro cuentas suman las 160
    // líneas del fotograma: ninguna queda sin explicar.
    // Un efecto de color que no puede cambiar ningún píxel tampoco impide la vía rápida:
    // - aclarar u oscurecer (modos 2 y 3) con `BLDY = 0` son la identidad, y la guarda ya exige
    //   que no haya ningún sprite translúcido, que es el único píxel que mezclaría en esos modos;
    // - el alpha (modo 1) sin bits de segundos objetivos no puede mezclar dos capas.
    let efecto_inerte = match blend_mode {
        0 => true,
        1 => bldcnt & 0x3F00 == 0,
        _ => evy == 0 || bldcnt & 0x3F == 0,
    };

    // Efecto que la vía rápida puede aplicar sin mirar la segunda capa: en los modos 2 y 3 el
    // resultado depende solo del color de arriba. Con `BLDY = 0` o sin capas objetivo no cambia
    // ningún píxel, y la línea se compone como si no hubiera efecto.
    let efecto_tramo = match blend_mode {
        2 if !efecto_inerte => EfectoTramo {
            oscurecer: false,
            evy,
            objetivos: bldcnt & 0x3F,
        },
        3 if !efecto_inerte => EfectoTramo {
            oscurecer: true,
            evy,
            objetivos: bldcnt & 0x3F,
        },
        _ => EfectoTramo {
            oscurecer: false,
            evy: 0,
            objetivos: 0,
        },
    };
    let con_efecto = efecto_tramo.evy != 0;

    // El único efecto real que la vía rápida no puede aplicar es el alpha del modo 1, que
    // necesita las dos capas de verdad. Pero la ventana de sprites se mira antes: el atajo corta
    // la línea solo por los bordes de WIN0 y WIN1, así que no puede saber qué control lleva cada
    // píxel dentro de la ventana de sprites, que depende de dónde estén los sprites. Por eso
    // ninguna línea con esa ventana activa puede usar el atajo, tampoco la que compondría el
    // alpha por tramos.
    let razon_camino = if objwin_on {
        2 // ventana de sprites
    } else if !efecto_inerte && !con_efecto {
        1u8 // efecto de color real que exige la segunda capa
    } else if obj_semi {
        3 // sprite translúcido
    } else {
        0 // vía rápida
    };

    // Reparto informativo: las líneas con efecto real se cuentan por modo, y las de los modos 2
    // y 3 que la vía rápida absorbe quedan también en `lineas_efecto`. Una línea con la ventana de
    // sprites se anota por la ventana aunque además tenga un alpha real: la ventana es la primera
    // razón que le impide el atajo, y así las cuatro cuentas siguen siendo excluyentes. Esas
    // líneas no entran en `lineas_efecto_alpha`, que cuenta las que el atajo deja solo por el alpha.
    #[cfg(feature = "perf-counters")]
    match razon_camino {
        0 => {
            mem.perf_counters.lineas_rectas += 1;
            if blend_mode != 0 {
                if con_efecto {
                    mem.perf_counters.lineas_efecto += 1;
                    if blend_mode == 2 {
                        mem.perf_counters.lineas_efecto_brillo += 1;
                    } else {
                        mem.perf_counters.lineas_efecto_oscuridad += 1;
                    }
                } else {
                    mem.perf_counters.lineas_inertes += 1;
                }
            }
        }
        1 => {
            mem.perf_counters.descartes_mezcla += 1;
            mem.perf_counters.lineas_efecto_alpha += 1;
        }
        2 => mem.perf_counters.descartes_ventana += 1,
        _ => mem.perf_counters.descartes_semi += 1,
    }

    // El alpha real (modo 1) también se compone por tramos: la pareja (capa de arriba, segunda
    // capa) se resuelve una vez por tramo en vez de una vez por píxel. Las líneas con algún
    // sprite translúcido se quedan en el camino general, que es el único que mira `obj.semi`.
    let alpha_por_tramos = razon_camino == 1 && (!obj_semi || alpha_tramos_forzado());
    if (razon_camino == 0 || alpha_por_tramos)
        && !via_rapida_apagada()
        && !camino_general_forzado()
    {
        let mut spans = [(0usize, VISIBLE_WIDTH, default_control); 5];
        let mut nspans = 1;
        if line_has_window {
            // Window control changes only at the horizontal window edges, so the
            // line splits there and each piece takes the control of its first
            // pixel (the same expression `control_at` uses). A window edge is a
            // half-open pair: `in_window_range` compares `x1 <= x < x2`, and
            // `x1 > x2` wraps around, which the sorted cuts handle too.
            let mut cuts = [0usize, VISIBLE_WIDTH, 0, 0, 0, 0];
            let mut ncuts = 2;
            if in_win0_y {
                cuts[ncuts] = (win0h >> 8) as usize;
                cuts[ncuts + 1] = (win0h as u8) as usize;
                ncuts += 2;
            }
            if in_win1_y {
                cuts[ncuts] = (win1h >> 8) as usize;
                cuts[ncuts + 1] = (win1h as u8) as usize;
                ncuts += 2;
            }
            cuts[..ncuts].sort_unstable();
            nspans = 0;
            for i in 0..ncuts - 1 {
                let a = cuts[i].min(VISIBLE_WIDTH);
                let b = cuts[i + 1].min(VISIBLE_WIDTH);
                if a >= b {
                    continue;
                }
                let x = a as u8;
                let control = if in_win0_y && in_window_range((win0h >> 8) as u8, win0h as u8, x) {
                    winin & 0x3F
                } else if in_win1_y && in_window_range((win1h >> 8) as u8, win1h as u8, x) {
                    (winin >> 8) & 0x3F
                } else {
                    winout & 0x3F
                };
                spans[nspans] = (a, b, control);
                nspans += 1;
            }
        }
        if alpha_por_tramos {
            #[cfg(feature = "perf-counters")]
            {
                mem.perf_counters.lineas_alpha_tramos += 1;
            }
            compose_alpha_tramos(
                mem,
                bg_lines,
                bg_prio,
                obj_line,
                &spans[..nspans],
                backdrop,
                row,
                bldcnt,
                eva,
                evb,
            );
        } else if con_efecto {
            compose_straight::<true>(
                mem,
                bg_lines,
                bg_prio,
                obj_line,
                &spans[..nspans],
                backdrop,
                row,
                efecto_tramo,
            );
        } else {
            compose_straight::<false>(
                mem,
                bg_lines,
                bg_prio,
                obj_line,
                &spans[..nspans],
                backdrop,
                row,
                efecto_tramo,
            );
        }
        return;
    }

    #[cfg(feature = "perf-counters")]
    {
        mem.perf_counters.lineas_camino_general += 1;
    }

    // Window control for one pixel: bits 0-3 BGs, 4 OBJ, 5 effects.
    let control_at = |x: usize| -> u16 {
        if !line_has_window {
            default_control
        } else if in_win0_y && in_window_range((win0h >> 8) as u8, win0h as u8, x as u8) {
            winin & 0x3F
        } else if in_win1_y && in_window_range((win1h >> 8) as u8, win1h as u8, x as u8) {
            (winin >> 8) & 0x3F
        } else if objwin_on && obj_window[x] {
            (winout >> 8) & 0x3F
        } else {
            winout & 0x3F
        }
    };

    // No colour effects this frame: every pixel's colour is simply the top
    // visible layer, so the second-layer search only matters where a
    // semi-transparent OBJ pixel sits on top and the blend list targets what is
    // underneath. Deferring that search to those pixels is what makes a line
    // with a single translucent sprite pixel cost the same as one without,
    // instead of sending all 240 pixels down the two-layer path.
    if blend_mode == 0 {
        for x in 0..VISIBLE_WIDTH {
            let control = control_at(x);
            let obj = &obj_line[x];
            let obj_visible = obj.priority != 0xFF && control & 0x10 != 0;

            // Top layer: the first opaque BG in priority order whose window
            // control bit allows it, beaten by the sprite when its priority is
            // <= that BG's (sprites rank above BGs of equal priority).
            let mut color = backdrop;
            let mut top_prio = 0xFFu8;
            let mut top_is_obj = false;
            for &(bp, bg) in bg_prio {
                if obj_visible && obj.priority <= bp {
                    color = obj.color;
                    top_prio = obj.priority;
                    top_is_obj = true;
                    break;
                }
                if control & (1 << bg) != 0 {
                    let c = bg_lines[bg][x];
                    if c != TRANSPARENT {
                        color = c;
                        top_prio = bp;
                        break;
                    }
                }
            }
            if !top_is_obj && obj_visible && top_prio == 0xFF {
                color = obj.color;
                top_is_obj = true;
            }

            if top_is_obj && obj.semi {
                // A translucent sprite pixel is on top, so now (and only now)
                // the layer below it decides the result.
                let (fc, _fl, sc, sl) =
                    top_two_layers(bg_lines, bg_prio, control, x, obj, backdrop);
                if bldcnt & (0x100 << sl) != 0 {
                    // Semi-transparent sprite forces alpha against 2nd targets.
                    color = alpha_blend(fc, sc, eva, evb);
                }
            }

            mem.framebuffer[row + x] = color;
        }
        return;
    }

    for x in 0..VISIBLE_WIDTH {
        let control = control_at(x);

        // Find the top two visible layers (OBJ ranks above BGs of equal
        // priority; BGs tie-break by index via the pre-sorted list).
        let obj = &obj_line[x];
        let obj_visible = obj.priority != 0xFF && control & 0x10 != 0;
        let (fc, fl, sc, sl) = top_two_layers(bg_lines, bg_prio, control, x, obj, backdrop);

        let effects_allowed = control & 0x20 != 0;
        let semi = obj_visible && fl == 4 && obj.semi;

        let color = if semi && bldcnt & (0x100 << sl) != 0 {
            // Semi-transparent sprite forces alpha against 2nd targets.
            alpha_blend(fc, sc, eva, evb)
        } else if effects_allowed && bldcnt & (1 << fl) != 0 {
            match blend_mode {
                1 if bldcnt & (0x100 << sl) != 0 => alpha_blend(fc, sc, eva, evb),
                2 => brighten(fc, evy),
                3 => darken(fc, evy),
                _ => fc,
            }
        } else {
            fc
        };

        mem.framebuffer[row + x] = color;
    }
}

/// Window coordinate test: x1 = left/top (inclusive), x2 = right/bottom
/// (exclusive); x1 > x2 wraps.
fn in_window_range(x1: u8, x2: u8, v: u8) -> bool {
    if x1 <= x2 {
        (x1..x2).contains(&v)
    } else {
        v >= x1 || v < x2
    }
}

// Color-effect arithmetic models the hardware blend unit, not GBATEK's
// fractional notation: per-channel multiply-accumulate rounds half-up
// (+8 >> 4; the darken subtrahend rounds half-down, +7, so the result
// still rounds half-up), and green passes through a 6-bit lane (the
// real hardware LSB of that lane is PRAM bit 15, which we mask at
// palette fetch — see pal16).

#[inline(always)]
fn alpha_blend(a: u16, b: u16, eva: u32, evb: u32) -> u16 {
    let (ar, ag, ab) = split6(a);
    let (br, bg, bb) = split6(b);
    let r = ((ar * eva + br * evb + 8) >> 4).min(31);
    let g = ((ag * eva + bg * evb + 8) >> 4).min(63);
    let b = ((ab * eva + bb * evb + 8) >> 4).min(31);
    join6(r, g, b)
}

#[inline(always)]
fn brighten(c: u16, evy: u32) -> u16 {
    let (r, g, b) = split6(c);
    join6(
        r + (((31 - r) * evy + 8) >> 4),
        g + (((63 - g) * evy + 8) >> 4),
        b + (((31 - b) * evy + 8) >> 4),
    )
}

#[inline(always)]
fn darken(c: u16, evy: u32) -> u16 {
    let (r, g, b) = split6(c);
    join6(
        r - ((r * evy + 7) >> 4),
        g - ((g * evy + 7) >> 4),
        b - ((b * evy + 7) >> 4),
    )
}

/// Channels as the blend unit sees them: 5-bit red/blue, 6-bit green
/// lane (5-bit value doubled; hardware ORs in PRAM bit 15 as the LSB,
/// which we strip at fetch).
#[inline(always)]
fn split6(c: u16) -> (u32, u32, u32) {
    (
        (c & 31) as u32,
        (((c >> 5) & 31) as u32) << 1,
        ((c >> 10) & 31) as u32,
    )
}

#[inline(always)]
fn join6(r: u32, g: u32, b: u32) -> u16 {
    (r | ((g >> 1) << 5) | (b << 10)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::Bus;

    #[test]
    fn mode3_pixel() {
        let mut mem = MemMap::new(vec![]);
        mem.write16(0x0400_0000, 0x0403); // mode 3, BG2 on
        mem.write16(0x0600_0000 + 2 * (5 + 7 * 240), 0x7C1F); // magenta at (5,7)
        render_scanline(&mut mem, 7);
        assert_eq!(mem.framebuffer[7 * 240 + 5], 0x7C1F);
        assert_eq!(mem.framebuffer[7 * 240 + 6], 0x0000);
    }

    #[test]
    fn mode0_text_tile() {
        let mut mem = MemMap::new(vec![]);
        mem.write16(0x0400_0000, 0x0100); // mode 0, BG0 on
                                          // BG0: char base 0, screen base block 31 (0xF800), 4bpp, 32x32.
        mem.write16(0x0400_0008, 31 << 8);
        // Palette 1, color 3 = red-ish.
        mem.write16(0x0500_0000 + (16 + 3) * 2, 0x001F);
        // Tile 1: all pixels = color 3.
        for row in 0..8 {
            mem.write32(0x0600_0000 + 32 + row * 4, 0x3333_3333);
        }
        // Map entry (0,0): tile 1, palette 1.
        mem.write16(0x0600_F800, (1 << 12) | 1);
        render_scanline(&mut mem, 0);
        assert_eq!(mem.framebuffer[0], 0x001F);
        // (8,0) is tile (1,0): map entry 0 -> tile 0 -> transparent -> backdrop.
        assert_eq!(mem.framebuffer[8], pal16(&mem, 0));
    }

    #[test]
    fn sprite_over_backdrop() {
        let mut mem = MemMap::new(vec![]);
        mem.write16(0x0400_0000, 0x1000); // mode 0, OBJ on
                                          // OBJ palette color 1 = green.
        mem.write16(0x0500_0200 + 2, 0x03E0);
        // Tile 1 in OBJ charblock: all color 1 (4bpp).
        for row in 0..8 {
            mem.write32(0x0601_0000 + 32 + row * 4, 0x1111_1111);
        }
        // Sprite 0: 8x8 at (10, 20), tile 1.
        mem.write16(0x0700_0000, 20); // attr0: y=20
        mem.write16(0x0700_0002, 10); // attr1: x=10
        mem.write16(0x0700_0004, 1); // attr2: tile 1
        render_scanline(&mut mem, 22);
        assert_eq!(mem.framebuffer[22 * 240 + 10], 0x03E0);
        assert_eq!(mem.framebuffer[22 * 240 + 18], 0x0000); // outside sprite
    }

    #[test]
    fn sprite_flips_use_obj_attr1_bits() {
        // Tile with one green pixel at texel (0, 0); the rest transparent.
        // Flips relocate the lit screen pixel: hflip = attr1 bit 12,
        // vflip = bit 13 (NOT the BG-entry bits 10/11).
        let mut mem = MemMap::new(vec![]);
        mem.write16(0x0400_0000, 0x1000); // mode 0, OBJ on
        mem.write16(0x0500_0200 + 2, 0x03E0);
        mem.write32(0x0601_0000 + 32, 0x0000_0001); // tile 1 row 0: texel 0 = color 1
        mem.write16(0x0700_0000, 20); // attr0: y=20
        mem.write16(0x0700_0002, 10); // attr1: x=10
        mem.write16(0x0700_0004, 1); // attr2: tile 1
        let lit = |mem: &mut MemMap, line: usize, x: usize| {
            render_scanline(mem, line);
            mem.framebuffer[line * 240 + x] == 0x03E0
        };
        assert!(lit(&mut mem, 20, 10)); // unflipped: top-left
        mem.write16(0x0700_0002, 10 | 0x1000); // hflip
        assert!(lit(&mut mem, 20, 17));
        mem.write16(0x0700_0002, 10 | 0x2000); // vflip
        assert!(lit(&mut mem, 27, 10));
        mem.write16(0x0700_0002, 10 | 0x3000); // both
        assert!(lit(&mut mem, 27, 17));
        // BG-entry flip bits must do nothing on sprites.
        mem.write16(0x0700_0002, 10 | 0x0C00);
        assert!(lit(&mut mem, 20, 10));
    }

    #[test]
    fn brightness_math() {
        assert_eq!(brighten(0x0000, 16), 0x7FFF);
        assert_eq!(darken(0x7FFF, 16), 0x0000);
        assert_eq!(brighten(0x0000, 0), 0x0000);
    }

    // Hardware blend-unit identities:
    // round-half-up MAC, 6-bit green lane.

    #[test]
    fn blend_identity_and_extremes() {
        // eva=16/evb=0 passes every color through exactly, including
        // the green lane round-trip.
        for c in [0x0000u16, 0x7FFF, 0x7C1F, 0x03E0, 0x1234, 0x5A5A] {
            assert_eq!(alpha_blend(c, 0x7FFF ^ c, 16, 0), c);
            assert_eq!(alpha_blend(0x7FFF ^ c, c, 0, 16), c);
        }
        // Saturation: white + white at full coefficients clamps.
        assert_eq!(alpha_blend(0x7FFF, 0x7FFF, 16, 16), 0x7FFF);
        // evy=0 is a no-op for both brightness directions.
        for c in [0x7FFF, 0x1234, 0x5A5A] {
            assert_eq!(brighten(c, 0), c);
            assert_eq!(darken(c, 0), c);
        }
    }

    #[test]
    fn blend_rounds_half_up() {
        // 50/50 of red 15 and 16: true value 15.5, hardware rounds up.
        // (15*8 + 16*8 + 8) >> 4 = 16.
        assert_eq!(alpha_blend(15, 16, 8, 8) & 31, 16);
        // Red 0 with 1 at evb=8: 0.5 rounds up to 1 (truncation gave 0).
        assert_eq!(alpha_blend(0, 1, 8, 8) & 31, 1);
        // Green's 6-bit lane rounds up only at fraction >= 12/16:
        // greens 15/16 at 8/8 -> S=124 in g6 (30*8+32*8=496; +8 >>4 =
        // 31; >>1 = 15): the half-step stays DOWN on green.
        assert_eq!((alpha_blend(15 << 5, 16 << 5, 8, 8) >> 5) & 31, 15);
        // ...but green 15/eva=4, 16/evb=12 (true 15.75) rounds up.
        assert_eq!((alpha_blend(15 << 5, 16 << 5, 4, 12) >> 5) & 31, 16);
    }

    #[test]
    fn brightness_rounds() {
        // Brighten red 0 by 1/16: (31*1 + 8) >> 4 = 2 (truncation: 1).
        assert_eq!(brighten(0, 1) & 31, 2);
        // Darken red 31 by 1/16: 31 - ((31 + 7) >> 4) = 29.
        assert_eq!(darken(31, 1) & 31, 29);
        // Green lane: brighten green 0 by 1/16 -> g6 = (63+8)>>4 = 4,
        // back to 5-bit = 2.
        assert_eq!((brighten(0, 1) >> 5) & 31, 2);
    }

    #[test]
    fn text_tile_4bpp_nibble_order_and_hflip() {
        let mut mem = MemMap::new(vec![]);
        mem.write16(0x0400_0000, 0x0100); // mode 0, BG0 on
        mem.write16(0x0400_0008, 31 << 8); // screen base 31 (0xF800)

        // Palette 0: colors 1..8
        for c in 1..=8u16 {
            mem.write16(0x0500_0000 + (c as u32) * 2, c);
        }

        // Tile 1 row 0: nibbles 1, 2, 3, 4, 5, 6, 7, 8
        // Byte 0: 0x21 (px0=1, px1=2)
        // Byte 1: 0x43 (px2=3, px3=4)
        // Byte 2: 0x65 (px4=5, px5=6)
        // Byte 3: 0x87 (px6=7, px7=8)
        // Little-endian u32 = 0x87654321
        mem.write32(0x0600_0000 + 32, 0x8765_4321);

        // Map entry 0: tile 1, no flip
        mem.write16(0x0600_F800, 1);
        render_scanline(&mut mem, 0);

        for px in 0..8 {
            assert_eq!(mem.framebuffer[px], (px as u16) + 1, "unflipped px {}", px);
        }

        // Map entry 0: tile 1 with hflip (bit 10)
        mem.write16(0x0600_F800, 1 | 0x0400);
        render_scanline(&mut mem, 0);

        for px in 0..8 {
            assert_eq!(mem.framebuffer[px], 8 - (px as u16), "hflip px {}", px);
        }
    }

    #[test]
    #[cfg(feature = "perf-counters")]
    fn straight_path_accounts_for_every_line() {
        // All layers off, no colour effect and no sprites: every line of the frame
        // must be counted as straight, and the four reasons must add up to whole
        // frames (the counters are read across one full frame).
        let mut mem = MemMap::new(vec![]);
        render_scanline(&mut mem, 0);
        let c = mem.perf_counters;
        assert_eq!(c.lineas_rectas, 1);
        assert_eq!(c.descartes_mezcla + c.descartes_ventana + c.descartes_semi, 0);
        let total = c.lineas_rectas
            + c.descartes_mezcla
            + c.descartes_ventana
            + c.descartes_semi;
        assert_eq!(total, 1, "una línea, una sola cuenta");
    }

    #[test]
    #[cfg(feature = "perf-counters")]
    fn colour_effect_sends_the_line_to_the_general_path() {
        // Un alpha real (con el fondo como primer y segundo objetivo) no se puede aplicar sin
        // la segunda capa, así que la línea va por el camino general y la cuenta lo dice con esa
        // razón exacta. Con la máscara de segundos objetivos vacía el alpha sería inerte y la
        // línea sí usaría la vía rápida (lo cubre el test de efectos inertes).
        let mut mem = MemMap::new(vec![]);
        mem.write16(0x0400_0050, 0x2060); // BLDCNT: alpha, fondo como primer y segundo objetivo
        render_scanline(&mut mem, 0);
        let c = mem.perf_counters;
        assert_eq!(c.descartes_mezcla, 1);
        assert_eq!(c.lineas_rectas, 0);
        assert_eq!(c.descartes_ventana + c.descartes_semi, 0);
    }

    /// Los dos tests que apagan la vía rápida comparten un interruptor global, así que se
    /// serializan con este candado para que sus mediciones sean deterministas.
    static LOCK_CAMINO: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Escena mínima para los tests de camino: BG0 y BG1 cubren la línea con colores opacos y
    /// distintos (para que el orden de capas importe), y los registros de efecto de color quedan
    /// como pida cada test.
    fn escena_de_efectos(bldcnt: u16, bldy: u16) -> MemMap {
        let mut mem = MemMap::new(vec![]);
        mem.write16(0x0400_0000, 0x1300); // modo 0, BG0, BG1 y OBJ activos
        mem.write16(0x0400_0008, 31 << 8); // BG0: prioridad 0, bloque de pantalla 31
        mem.write16(0x0400_000A, 1 | (30 << 8)); // BG1: prioridad 1, bloque 30
        for c in 1..16u32 {
            mem.write16(0x0500_0000 + (16 + c) * 2, 0x001F | ((c as u16) << 5));
            mem.write16(0x0500_0000 + (32 + c) * 2, 0x03E0 | (c as u16));
        }
        for row in 0..8u32 {
            mem.write32(0x0600_0000 + 32 + row * 4, 0x8765_4321); // tile 1: colores 1..8
        }
        for i in 0..30u32 {
            mem.write16(0x0600_F800 + i * 2, (1u16 << 12) | 1); // BG0: tile 1, paleta 1
            mem.write16(0x0600_F000 + i * 2, (1u16 << 12) | 1); // BG1: tile 1, paleta 1
        }
        // Un sprite opaco sobre la línea 0, con la misma prioridad que BG0 para que gane él:
        // así el color ganador puede ser el de la capa 4 (OBJ) y los objetivos de `BLDCNT`
        // tienen que distinguirlo de las capas de fondo.
        mem.write16(0x0500_0200 + 2 * 2, 0x03E0); // paleta OBJ 0, color 2
        for row in 0..8u32 {
            mem.write32(0x0601_0000 + 8 * 32 + row * 4, 0x2222_2222); // tile OBJ 8: todo color 2
        }
        mem.write16(0x0700_0000, 0); // attr0: y = 0, sin rotación ni modo translúcido
        mem.write16(0x0700_0002, 20); // attr1: x = 20
        mem.write16(0x0700_0004, 8); // attr2: tile 8, prioridad 0
        mem.write16(0x0400_0050, bldcnt);
        mem.write16(0x0400_0054, bldy);
        mem
    }

    /// Montaje de los escenarios 1 y 2 de la propuesta `2026-10-13_alpha-real-por-tramos.md`: dos
    /// capas de fondo visibles y opacas sobre toda la línea, con colores distintos —BG1 con otro banco
    /// de paleta, porque con el mismo color el alpha de 8/8 sería la identidad y la comparación de
    /// píxeles no probaría nada—, sin ventana, `BLDCNT = 0x241` (modo alpha, BG0 como primer objetivo,
    /// BG1 en la máscara de segundos objetivos) y `BLDALPHA = 0x0808`. Con `translucido` añade un
    /// sprite en modo translúcido en x = 100.
    fn escena_alpha_real(translucido: bool) -> MemMap {
        let mut mem = escena_de_efectos(0x0241, 0);
        // BG0 y BG1 activos; con el sprite hace falta además el bit 12 (OBJ), que es el único que
        // `render_scanline` mira para poblar la línea de sprites.
        mem.write16(0x0400_0000, if translucido { 0x1300 } else { 0x0300 });
        mem.write16(0x0400_0052, 0x0808); // EVA = EVB = 8
        for i in 0..30u32 {
            mem.write16(0x0600_F000 + i * 2, (2u16 << 12) | 1); // BG1: tile 1 con el banco 2
        }
        if translucido {
            mem.write16(0x0700_0000, 1 << 10); // attr0: modo translúcido del sprite
            mem.write16(0x0700_0002, 100); // attr1: x = 100
        }
        mem
    }

    /// Escenario 1 de la propuesta `2026-10-13_alpha-real-por-tramos.md`: una línea con dos capas de
    /// fondo visibles, sin ventana ni sprite, con alpha real (`BLDCNT = 0x241`, `BLDALPHA = 0x0808`).
    /// El camino por tramos tiene que dibujar los mismos 240 píxeles que el camino general y resolver
    /// la pareja de capas **una sola vez**, no una por píxel.
    #[test]
    fn alpha_por_tramos_da_los_mismos_pixeles() {
        let _guard = LOCK_CAMINO.lock().unwrap_or_else(|e| e.into_inner());
        let escena = || escena_alpha_real(false);

        let mut tramos = escena();
        BUSQUEDAS_DOS_CAPAS.with(|busquedas| busquedas.set(0));
        render_scanline(&mut tramos, 0);
        assert_eq!(
            BUSQUEDAS_DOS_CAPAS.with(|busquedas| busquedas.get()),
            1,
            "las dos capas son las mismas en toda la línea: una búsqueda, no 240"
        );

        SIN_VIA_RAPIDA.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut general = escena();
        BUSQUEDAS_DOS_CAPAS.with(|busquedas| busquedas.set(0));
        render_scanline(&mut general, 0);
        let busquedas_general = BUSQUEDAS_DOS_CAPAS.with(|busquedas| busquedas.get());
        SIN_VIA_RAPIDA.store(false, std::sync::atomic::Ordering::Relaxed);

        assert_eq!(
            busquedas_general, VISIBLE_WIDTH as u64,
            "el camino general busca la segunda capa en cada píxel"
        );
        assert_eq!(
            &tramos.framebuffer[..240],
            &general.framebuffer[..240],
            "el camino por tramos debe dar los mismos 240 píxeles"
        );
    }

    /// Escenario de la propuesta `2026-10-22_clave-de-tramo-aliase-la-prioridad.md`:
    /// Colisión de bits en la clave de tramo entre la opacidad de los fondos (bits 0-3)
    /// y la prioridad del sprite. En una línea con alpha real (BG0 sobre BG1) donde BG1 es opaca
    /// solo en x = 8..15, un sprite de prioridad 2 en x = 0..7 y un sprite de prioridad 0 en x = 8..15,
    /// la clave no debe confundir la prioridad 2 con el bit de BG1.
    #[test]
    fn alpha_por_tramos_no_aliase_la_prioridad_del_sprite() {
        let _guard = LOCK_CAMINO.lock().unwrap_or_else(|e| e.into_inner());
        let escena = || {
            let mut mem = MemMap::new(vec![]);
            // modo 0, BG0, BG1 y OBJ activos (0x1300)
            mem.write16(0x0400_0000, 0x1300);
            mem.write16(0x0400_0008, 31 << 8); // BG0: prioridad 0, bloque 31
            mem.write16(0x0400_000A, 1 | (30 << 8)); // BG1: prioridad 1, bloque 30
            mem.write16(0x0400_0050, 0x0241); // modo 1 (alpha), 1.er obj BG0, 2.º obj BG1
            mem.write16(0x0400_0052, 0x0808); // EVA = EVB = 8

            // Paletas
            mem.write16(0x0500_0000 + 16 * 2 + 2, 0x003F); // BG paleta 1 color 1: azul
            mem.write16(0x0500_0000 + 32 * 2 + 2, 0x7C00); // BG paleta 2 color 1: rojo
            mem.write16(0x0500_0200 + 2 * 2, 0x03E0);      // OBJ paleta 0 color 2: verde

            // Tiles
            for row in 0..8u32 {
                mem.write32(0x0600_0000 + 32 + row * 4, 0x1111_1111); // tile BG 1: color 1
                mem.write32(0x0601_0000 + 8 * 32 + row * 4, 0x2222_2222); // tile OBJ 8: color 2
            }

            // BG0 cubre toda la línea con tile 1, paleta 1
            for i in 0..30u32 {
                mem.write16(0x0600_F800 + i * 2, (1u16 << 12) | 1);
            }
            // BG1 es opaca solo en el bloque 1 (x = 8..15)
            mem.write16(0x0600_F000 + 1 * 2, (2u16 << 12) | 1);

            // Sprite 0: x = 0..7, prioridad 2 (attr2 bits 10-11 = 2)
            mem.write16(0x0700_0000, 0); // y = 0
            mem.write16(0x0700_0002, 0); // x = 0
            mem.write16(0x0700_0004, (2 << 10) | 8); // prioridad 2, tile 8

            // Sprite 1: x = 8..15, prioridad 0 (attr2 bits 10-11 = 0)
            mem.write16(0x0700_0008, 0); // y = 0
            mem.write16(0x0700_000A, 8); // x = 8
            mem.write16(0x0700_000C, (0 << 10) | 8); // prioridad 0, tile 8
            mem
        };

        let mut rapido = escena();
        render_scanline(&mut rapido, 0);

        SIN_VIA_RAPIDA.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut general = escena();
        render_scanline(&mut general, 0);
        SIN_VIA_RAPIDA.store(false, std::sync::atomic::Ordering::Relaxed);

        let distintos: Vec<usize> = (0..240)
            .filter(|&x| rapido.framebuffer[x] != general.framebuffer[x])
            .collect();
        assert_eq!(
            distintos,
            Vec::<usize>::new(),
            "el camino por tramos difiere del general en {} píxeles: {:?}",
            distintos.len(),
            distintos
        );
    }

    /// Escenario 2 de la propuesta: con un píxel de sprite translúcido en x = 100, el resultado de
    /// ese píxel depende de `obj.semi`, que solo mira el camino general, así que la línea se queda
    /// donde estaba: mismos 240 píxeles y las mismas 240 búsquedas de la segunda capa.
    #[test]
    fn alpha_con_sprite_translucido_se_queda_en_el_camino_general() {
        let _guard = LOCK_CAMINO.lock().unwrap_or_else(|e| e.into_inner());
        let escena = || escena_alpha_real(true);

        let mut tramos = escena();
        BUSQUEDAS_DOS_CAPAS.with(|busquedas| busquedas.set(0));
        render_scanline(&mut tramos, 0);
        assert_eq!(
            BUSQUEDAS_DOS_CAPAS.with(|busquedas| busquedas.get()),
            VISIBLE_WIDTH as u64,
            "con un sprite translúcido la línea no adopta el camino por tramos"
        );

        SIN_VIA_RAPIDA.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut general = escena();
        render_scanline(&mut general, 0);
        SIN_VIA_RAPIDA.store(false, std::sync::atomic::Ordering::Relaxed);

        assert_eq!(
            &tramos.framebuffer[..240],
            &general.framebuffer[..240],
            "la línea se compone con el camino general, píxel a píxel"
        );
    }

    /// Escena para el test de la ventana de sprites: la línea con alpha real de
    /// `escena_alpha_real`, pero con la ventana de sprites activada (`DISPCNT`, bit 15) y un sprite
    /// en modo ventana (atributo 0, bits 10-11 = 2) que cubre ocho píxeles en x = 100. El control de
    /// dentro de la ventana (`WINOUT`, bits 8-13) apaga BG0, así que dentro la capa de arriba es BG1
    /// y no hay alpha; fuera, la de arriba es BG0 y el alpha sí se aplica. Esa diferencia entre
    /// dos píxeles de la misma línea es la que delata al atajo, que no puede saber qué control
    /// lleva cada píxel dentro de la ventana.
    fn escena_alpha_con_ventana_obj() -> MemMap {
        let mut mem = escena_alpha_real(false);
        mem.write16(0x0400_0000, 0x9300); // modo 0, BG0, BG1 y OBJ activos, y ventana de sprites
        mem.write16(0x0400_004A, 0x3E3F); // WINOUT: fuera todo activo; dentro, sin BG0
        mem.write16(0x0700_0008, 2 << 10); // attr0 del sprite 1: modo ventana
        mem.write16(0x0700_000A, 100); // attr1: x = 100
        mem.write16(0x0700_000C, 8); // attr2: tile 8, prioridad 0
        mem
    }

    /// La ventana de sprites con alpha real a la vez: el atajo por tramos corta la línea solo por
    /// los bordes de WIN0 y WIN1, así que no puede aplicar la máscara de la ventana de sprites. Con
    /// la ventana activa la línea se va al camino general por píxel, y tiene que dar los mismos
    /// 240 píxeles que da ese camino.
    #[test]
    fn alpha_por_tramos_con_ventana_obj_dibuja_lo_mismo_que_el_camino_general() {
        let _guard = LOCK_CAMINO.lock().unwrap_or_else(|e| e.into_inner());
        let escena = || escena_alpha_con_ventana_obj();

        let mut rapido = escena();
        BUSQUEDAS_DOS_CAPAS.with(|busquedas| busquedas.set(0));
        render_scanline(&mut rapido, 0);
        assert_eq!(
            BUSQUEDAS_DOS_CAPAS.with(|busquedas| busquedas.get()),
            VISIBLE_WIDTH as u64,
            "con la ventana de sprites la línea no adopta el atajo por tramos"
        );
        #[cfg(feature = "perf-counters")]
        {
            let c = rapido.perf_counters;
            assert_eq!(
                c.descartes_ventana, 1,
                "la línea se descarta por la ventana de sprites"
            );
            assert_eq!(c.descartes_mezcla, 0);
            assert_eq!(
                c.lineas_efecto_alpha, 0,
                "la ventana es la primera razón que impide el atajo, no el alpha"
            );
        }

        SIN_VIA_RAPIDA.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut general = escena();
        render_scanline(&mut general, 0);
        SIN_VIA_RAPIDA.store(false, std::sync::atomic::Ordering::Relaxed);

        assert_ne!(
            general.framebuffer[100], general.framebuffer[40],
            "la escena tiene que ejercitar la ventana: dentro el control apaga BG0 y fuera no"
        );
        assert_eq!(
            &rapido.framebuffer[..240],
            &general.framebuffer[..240],
            "la línea con ventana de sprites tiene que dar los mismos 240 píxeles"
        );
    }

    /// El alpha real se compone por tramos: la pareja (capa de arriba, segunda capa) se resuelve
    /// una vez por tramo en vez de una vez por píxel. La prueba de que el atajo no cambia un solo
    /// píxel es comparar contra el camino general forzado, con la escena que sí mezcla de verdad
    /// (BG0 y BG1 opacos, y un sprite opaco de la misma prioridad que BG0 en medio de la línea)
    /// y con varias combinaciones de objetivos de `BLDCNT` y de `EVA`/`EVB`.
    #[test]
    fn alpha_real_por_tramos_dibuja_lo_mismo_que_el_camino_general() {
        let _guard = LOCK_CAMINO.lock().unwrap_or_else(|e| e.into_inner());
        // modo 1 (bit 6) con BG0 como primer objetivo y varias capas como segundas; el par
        // (BLDCNT, BLDALPHA) se escribe tal cual en los registros, así que el test no copia la
        // lógica del camino general para construir la escena.
        let combinaciones = [
            ("BG0 sobre BG1", 0x0241u16, 0x0F05u16),
            ("BG0 sobre OBJ", 0x1041u16, 0x050Fu16),
            ("OBJ sobre BG1", 0x0250u16, 0x0808u16),
            ("BG0 y fondo sobre el fondo", 0x2061u16, 0x0F01u16),
        ];
        for (etiqueta, bldcnt, bldalpha) in combinaciones {
            let mut rapido = escena_de_efectos(bldcnt, 0);
            rapido.write16(0x0400_0052, bldalpha);
            BUSQUEDAS_DOS_CAPAS.with(|busquedas| busquedas.set(0));
            render_scanline(&mut rapido, 0);
            let busquedas_tramos = BUSQUEDAS_DOS_CAPAS.with(|busquedas| busquedas.get());
            // El mecanismo del atajo es este: una búsqueda por tramo, no una por píxel. La
            // aserción es holgada a propósito (menos de una búsqueda cada 10 píxeles) para no
            // atarla al número exacto de tramos de la escena, pero falla si el camino vuelve a
            // preguntar por píxel.
            assert!(
                busquedas_tramos < VISIBLE_WIDTH as u64 / 10,
                "{etiqueta}: el camino por tramos buscó {busquedas_tramos} veces en {VISIBLE_WIDTH} píxeles"
            );
            #[cfg(feature = "perf-counters")]
            assert_eq!(
                rapido.perf_counters.descartes_mezcla, 1,
                "{etiqueta}: un alpha real con segundos objetivos no es inerte"
            );

            SIN_VIA_RAPIDA.store(true, std::sync::atomic::Ordering::Relaxed);
            let mut general = escena_de_efectos(bldcnt, 0);
            general.write16(0x0400_0052, bldalpha);
            BUSQUEDAS_DOS_CAPAS.with(|busquedas| busquedas.set(0));
            render_scanline(&mut general, 0);
            SIN_VIA_RAPIDA.store(false, std::sync::atomic::Ordering::Relaxed);
            let busquedas_general = BUSQUEDAS_DOS_CAPAS.with(|busquedas| busquedas.get());

            assert_eq!(
                busquedas_general, VISIBLE_WIDTH as u64,
                "{etiqueta}: el camino general busca la segunda capa una vez por píxel"
            );
            assert_eq!(
                &rapido.framebuffer[..240],
                &general.framebuffer[..240],
                "{etiqueta}: el camino por tramos y el camino general deben dibujar lo mismo"
            );
        }
    }

    /// Un efecto de color que no puede cambiar ningún píxel no debe impedir la vía rápida:
    /// oscurecer con `BLDY = 0` es la identidad, y el alpha sin bits de segundos objetivos no
    /// puede mezclar dos capas. La comparación con el camino general (forzado) es la prueba de
    /// que el atajo dibuja exactamente lo mismo.
    #[test]
    fn efecto_de_color_inerte_usa_la_via_rapida_sin_cambiar_pixeles() {
        let _guard = LOCK_CAMINO.lock().unwrap_or_else(|e| e.into_inner());
        let inerte_oscurecer = 0x3FFF; // modo 3 (oscurecer) con todos los objetivos, BLDY = 0
        let inerte_alpha = 0x007F; // modo 1 (alpha) sin ningún segundo objetivo
        for (etiqueta, bldcnt, bldy) in [
            ("oscurecer con BLDY = 0", inerte_oscurecer, 0u16),
            ("alpha sin segundos objetivos", inerte_alpha, 0u16),
            // Real pero sin línea que cambiar: el único objetivo es BG1 y en esta escena gana
            // BG0 (o el sprite), así que el efecto no puede tocar ningún píxel.
            ("oscurecer sobre una capa que no gana", 0x00C2, 8u16),
            // Real y con el sprite como único objetivo (capa 4): los píxeles del sprite cambian
            // y los de fondo no, en los dos caminos.
            ("oscurecer solo sobre el sprite", 0x00D0, 8u16),
        ] {
            let mut rapido = escena_de_efectos(bldcnt, bldy);
            render_scanline(&mut rapido, 0);
            #[cfg(feature = "perf-counters")]
            {
                assert_eq!(
                    rapido.perf_counters.lineas_rectas, 1,
                    "{etiqueta}: la línea debe ir por la vía rápida"
                );
                assert_eq!(
                    rapido.perf_counters.descartes_mezcla, 0,
                    "{etiqueta}: un efecto inerte no descarta la línea"
                );
            }

            SIN_VIA_RAPIDA.store(true, std::sync::atomic::Ordering::Relaxed);
            let mut general = escena_de_efectos(bldcnt, bldy);
            render_scanline(&mut general, 0);
            SIN_VIA_RAPIDA.store(false, std::sync::atomic::Ordering::Relaxed);

            assert_eq!(
                &rapido.framebuffer[..240],
                &general.framebuffer[..240],
                "{etiqueta}: la vía rápida y el camino general deben dibujar lo mismo"
            );
        }
    }

    /// Con un efecto real (oscurecer con `BLDY` distinto de cero) el resultado depende solo de la
    /// capa de arriba, así que esa línea tampoco necesita buscar la segunda capa. Se cuenta la
    /// búsqueda en lugar de medir tiempo, y se exige que el framebuffer sea el mismo.
    #[test]
    fn oscurecido_real_no_busca_la_segunda_capa() {
        let _guard = LOCK_CAMINO.lock().unwrap_or_else(|e| e.into_inner());
        let mut rapido = escena_de_efectos(0x3FFF, 8);
        BUSQUEDAS_DOS_CAPAS.with(|busquedas| busquedas.set(0));
        render_scanline(&mut rapido, 0);
        assert_eq!(
            BUSQUEDAS_DOS_CAPAS.with(|busquedas| busquedas.get()),
            0,
            "el camino especializado no debe buscar la segunda capa"
        );

        SIN_VIA_RAPIDA.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut general = escena_de_efectos(0x3FFF, 8);
        BUSQUEDAS_DOS_CAPAS.with(|busquedas| busquedas.set(0));
        render_scanline(&mut general, 0);
        let busquedas_del_general = BUSQUEDAS_DOS_CAPAS.with(|busquedas| busquedas.get());
        SIN_VIA_RAPIDA.store(false, std::sync::atomic::Ordering::Relaxed);

        assert!(
            busquedas_del_general > 0,
            "el camino general sí busca la segunda capa: sin eso el contraste no prueba nada"
        );
        assert_eq!(
            &rapido.framebuffer[..240],
            &general.framebuffer[..240],
            "el atajo debe dibujar lo mismo que el camino general"
        );
    }

    /// La puerta de los efectos de ventana (`WININ` bit 5) decide si el efecto se aplica píxel a
    /// píxel. Con una ventana que lo apaga, la vía rápida tiene que dejar sin transformar justo esa
    /// parte de la línea, y el camino general coincidir con ella.
    #[test]
    fn efecto_dentro_de_una_ventana_sin_efectos_da_los_mismos_pixeles() {
        let _guard = LOCK_CAMINO.lock().unwrap_or_else(|e| e.into_inner());
        fn escena() -> MemMap {
            let mut mem = MemMap::new(vec![]);
            mem.write16(0x0400_0000, 0x2100); // modo 0, BG0 activo, WIN0 activa
            mem.write16(0x0400_0008, 31 << 8); // BG0: prioridad 0, bloque de pantalla 31
            mem.write16(0x0500_0000 + 16 * 2 + 2, 0x003F); // paleta 1, color 1
            for row in 0..8u32 {
                mem.write32(0x0600_0000 + 32 + row * 4, 0x1111_1111); // tile 1: todo color 1
            }
            for i in 0..30u32 {
                mem.write16(0x0600_F800 + i * 2, (1u16 << 12) | 1);
            }
            mem.write16(0x0400_0040, (8 << 8) | 16); // WIN0H: x = 8..16
            mem.write16(0x0400_0044, 0x00FF); // WIN0V: cubre la línea 0
            mem.write16(0x0400_0048, 0x0001); // WININ: BG0 y efectos apagados
            mem.write16(0x0400_004A, 0x0021); // WINOUT: BG0 y efectos encendidos
            mem.write16(0x0400_0050, 0x00C1); // oscurecer, objetivo BG0
            mem.write16(0x0400_0054, 8);
            mem
        }
        let mut rapido = escena();
        render_scanline(&mut rapido, 0);

        SIN_VIA_RAPIDA.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut general = escena();
        render_scanline(&mut general, 0);
        SIN_VIA_RAPIDA.store(false, std::sync::atomic::Ordering::Relaxed);

        assert_eq!(
            &rapido.framebuffer[..240],
            &general.framebuffer[..240],
            "la ventana con efectos apagados debe tratarse igual en ambos caminos"
        );
        // Fuera de la ventana el efecto se aplica; dentro, no: sin esta pareja el test podría
        // pasar sin que el efecto se estuviera haciendo en ninguna parte.
        assert_eq!(rapido.framebuffer[16], darken(0x003F, 8), "fuera de la ventana");
        assert_eq!(rapido.framebuffer[8], 0x003F, "dentro de la ventana, sin efecto");
    }

    /// La capa que gana puede no ser objetivo del efecto aunque otra sí lo sea: entonces no se
    /// toca ningún píxel. La escena usa una ventana que cubre la línea entera para que el control
    /// venga de `WININ` y la composición use la variante sin sprite, que es la otra mitad del
    /// camino especializado.
    #[test]
    fn efecto_sobre_una_capa_que_no_gana_no_cambia_pixeles() {
        let _guard = LOCK_CAMINO.lock().unwrap_or_else(|e| e.into_inner());
        fn escena() -> MemMap {
            let mut mem = MemMap::new(vec![]);
            mem.write16(0x0400_0000, 0x2300); // modo 0, BG0, BG1 y WIN0 activos
            mem.write16(0x0400_0008, 31 << 8); // BG0: prioridad 0, bloque 31
            mem.write16(0x0400_000A, 1 | (30 << 8)); // BG1: prioridad 1, bloque 30
            mem.write16(0x0500_0000 + 16 * 2 + 2, 0x003F); // paleta 1, color 1
            mem.write16(0x0500_0000 + 32 * 2 + 2, 0x03FF); // paleta 2, color 1
            for row in 0..8u32 {
                mem.write32(0x0600_0000 + 32 + row * 4, 0x1111_1111); // tile 1: todo color 1
            }
            for i in 0..30u32 {
                mem.write16(0x0600_F800 + i * 2, (1u16 << 12) | 1); // BG0: tile 1, paleta 1
                mem.write16(0x0600_F000 + i * 2, (2u16 << 12) | 1); // BG1: tile 1, paleta 2
            }
            mem.write16(0x0400_0040, 240); // WIN0H: x = 0..240, la línea entera
            mem.write16(0x0400_0044, 0x00FF); // WIN0V: cubre la línea 0
            mem.write16(0x0400_0048, 0x002F); // WININ: BG0-3 y efectos, sin OBJ
            mem.write16(0x0400_0050, 0x00C2); // oscurecer, objetivo BG1 (BG0 no lo es)
            mem.write16(0x0400_0054, 8);
            mem
        }
        let mut rapido = escena();
        render_scanline(&mut rapido, 0);

        SIN_VIA_RAPIDA.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut general = escena();
        render_scanline(&mut general, 0);
        SIN_VIA_RAPIDA.store(false, std::sync::atomic::Ordering::Relaxed);

        assert_eq!(
            &rapido.framebuffer[..240],
            &general.framebuffer[..240],
            "una capa no objetivo no debe verse afectada en ninguno de los caminos"
        );
        // Gana BG0 (paleta 1), que no es objetivo: su color debe quedar intacto, no el de BG1.
        assert_eq!(
            rapido.framebuffer[120], 0x003F,
            "el efecto apunta a BG1, así que la capa ganadora no se transforma"
        );
    }

    /// Un sprite presente puede perder contra un fondo de mejor prioridad: la capa que gana es la
    /// de fondo y el efecto tiene que aplicarse a *su* color, no al del sprite (que en esa columna
    /// ni se ve). La escena de efectos no cubre esto —su sprite empata con BG0 y gana—, así que sin
    /// este caso la rama de la vía rápida con sprite presente y fondo ganador queda sin cubrir.
    #[test]
    fn fondo_que_gana_al_sprite_recibe_el_efecto() {
        let _guard = LOCK_CAMINO.lock().unwrap_or_else(|e| e.into_inner());
        /// BG0 (prioridad 0) y BG1 (prioridad 1) cubren la línea entera con colores distintos, y
        /// un sprite de prioridad 3 (la peor de todas) se solapa en x = 20..28 pero pierde contra
        /// BG0, que está por encima.
        fn escena(bldcnt: u16) -> MemMap {
            let mut mem = MemMap::new(vec![]);
            mem.write16(0x0400_0000, 0x1300); // modo 0, BG0, BG1 y OBJ activos
            mem.write16(0x0400_0008, 31 << 8); // BG0: prioridad 0, bloque 31
            mem.write16(0x0400_000A, 1 | (30 << 8)); // BG1: prioridad 1, bloque 30
            mem.write16(0x0500_0000 + (16 + 1) * 2, 0x003F); // paleta 1, color 1
            mem.write16(0x0500_0000 + (32 + 1) * 2, 0x03FF); // paleta 2, color 1
            for row in 0..8u32 {
                mem.write32(0x0600_0000 + 32 + row * 4, 0x1111_1111); // tile 1: todo color 1
            }
            for i in 0..30u32 {
                mem.write16(0x0600_F800 + i * 2, (1u16 << 12) | 1); // BG0: tile 1, paleta 1
                mem.write16(0x0600_F000 + i * 2, (2u16 << 12) | 1); // BG1: tile 1, paleta 2
            }
            mem.write16(0x0500_0200 + 2 * 2, 0x03E0); // paleta OBJ 0, color 2
            for row in 0..8u32 {
                mem.write32(0x0601_0000 + 8 * 32 + row * 4, 0x2222_2222); // tile OBJ 8: color 2
            }
            mem.write16(0x0700_0000, 0); // attr0: y = 0, sin rotación ni modo translúcido
            mem.write16(0x0700_0002, 20); // attr1: x = 20
            mem.write16(0x0700_0004, 8 | (3 << 10)); // attr2: tile 8, prioridad 3
            mem.write16(0x0400_0050, bldcnt);
            mem.write16(0x0400_0054, 8); // BLDY distinto de cero: oscurecido real
            mem
        }

        // `BLDCNT` oscurece apuntando a BG0, la capa que gana al sprite.
        let mut rapido = escena(0x00C1);
        render_scanline(&mut rapido, 0);
        #[cfg(feature = "perf-counters")]
        {
            assert_eq!(
                rapido.perf_counters.lineas_rectas, 1,
                "un oscurecido real usa la vía rápida también con sprite en la línea"
            );
            assert_eq!(rapido.perf_counters.descartes_mezcla, 0);
        }

        // El mismo reparto de capas, pero el objetivo es BG1, la capa que pierde: nada cambia.
        let mut rapido_sin_objetivo = escena(0x00C2);
        render_scanline(&mut rapido_sin_objetivo, 0);

        SIN_VIA_RAPIDA.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut general = escena(0x00C1);
        render_scanline(&mut general, 0);
        let mut general_sin_objetivo = escena(0x00C2);
        render_scanline(&mut general_sin_objetivo, 0);
        SIN_VIA_RAPIDA.store(false, std::sync::atomic::Ordering::Relaxed);

        assert_eq!(
            &rapido.framebuffer[..240],
            &general.framebuffer[..240],
            "con sprite presente y fondo ganador los dos caminos deben dibujar lo mismo"
        );
        assert_eq!(
            &rapido_sin_objetivo.framebuffer[..240],
            &general_sin_objetivo.framebuffer[..240],
            "una capa no objetivo no debe verse afectada en ninguno de los caminos"
        );

        // El valor esperado sale de la propia función de efecto: lo que se comprueba es que la
        // capa ganadora (BG0) lo reciba en toda la línea, incluida la columna del sprite.
        let oscurecido = darken(0x003F, 8);
        assert_ne!(
            oscurecido, 0x003F,
            "el escenario necesita un efecto que cambie el píxel, o no probaría nada"
        );
        assert!(
            rapido.framebuffer[..240].iter().all(|&p| p == oscurecido),
            "BG0 gana en toda la línea y es objetivo: cada píxel debe salir oscurecido"
        );
        assert!(
            rapido_sin_objetivo.framebuffer[..240]
                .iter()
                .all(|&p| p == 0x003F),
            "BG0 gana pero no es objetivo: ningún píxel debe cambiar"
        );
        // BG0 (prioridad 0) está por encima del sprite (prioridad 3) en toda la línea, así que el
        // color del sprite no puede aparecer en ninguna de las dos escenas.
        assert!(
            !rapido.framebuffer[..240].contains(&0x03E0)
                && !rapido_sin_objetivo.framebuffer[..240].contains(&0x03E0),
            "el sprite pierde en toda la línea: su color no debe verse"
        );
    }

    #[test]
    fn window_ranges_keep_the_control_of_each_range() {
        // A window splits the line into ranges with different layer control. The
        // straight path composes each range as one pass, so its boundaries must
        // land exactly where the per-pixel window test would. WIN0H packs the
        // window as X1 (left) in the high byte and X2 (right) in the low byte,
        // and the range is `x1 <= x < x2`; outside it the layers come from
        // WINOUT.
        let mut mem = MemMap::new(vec![]);
        mem.write16(0x0400_0000, 0x2300); // mode 0, BG0 + BG1 on, WIN0 on
        mem.write16(0x0400_0008, 31 << 8); // BG0: screen base 31 (0xF800)
        mem.write16(0x0400_000A, 30 << 8); // BG1: screen base 30 (0xF000)
        mem.write16(0x0500_0000 + (16 + 3) * 2, 0x001F); // palette 1 color 3
        mem.write16(0x0500_0000 + (32 + 3) * 2, 0x03E0); // palette 2 color 3
        for row in 0..8 {
            mem.write32(0x0600_0000 + 32 + row * 4, 0x3333_3333); // tile 1
            mem.write32(0x0600_0000 + 64 + row * 4, 0x3333_3333); // tile 2
        }
        // Both layers must cover the whole line, or the transparent half would
        // let the backdrop show through and hide the control being tested.
        for i in 0..30 {
            mem.write16(0x0600_F800 + i * 2, (1 << 12) | 1); // BG0: tile 1, palette 1
            mem.write16(0x0600_F000 + i * 2, (2 << 12) | 2); // BG1: tile 2, palette 2
        }
        mem.write16(0x0400_0040, (8 << 8) | 16); // WIN0H: x = 8..16
        mem.write16(0x0400_0044, 0x00FF); // WIN0V covers line 0
        mem.write16(0x0400_0048, 0x0001); // WININ: BG0 only
        mem.write16(0x0400_004A, 0x0002); // WINOUT: BG1 only
        render_scanline(&mut mem, 0);
        assert_eq!(mem.framebuffer[7], 0x03E0, "before the window: BG1");
        assert_eq!(mem.framebuffer[8], 0x001F, "window start is inclusive");
        assert_eq!(mem.framebuffer[15], 0x001F, "window end is exclusive");
        assert_eq!(mem.framebuffer[16], 0x03E0, "after the window: BG1 again");
        assert_eq!(mem.framebuffer[239], 0x03E0, "last pixel of the line");
    }

    #[test]
    fn translucent_sprite_still_blends_in_mode_zero() {
        // OBJ mode 1 (translucent) blends even when BLDCNT's mode is 0, so a line
        // with such a pixel must not take the straight path: it has to search the
        // layer under the sprite and run the alpha unit.
        let mut mem = MemMap::new(vec![]);
        mem.write16(0x0400_0000, 0x1100); // mode 0, BG0 on, OBJ on
        mem.write16(0x0400_0008, 31 << 8); // BG0: screen base 31
        mem.write16(0x0500_0000 + (16 + 3) * 2, 0x03E0); // BG palette 1 color 3
        mem.write16(0x0500_0200 + 2, 0x001F); // OBJ palette color 1
        for row in 0..8 {
            mem.write32(0x0600_0000 + 32 + row * 4, 0x3333_3333); // BG tile 1
            mem.write32(0x0601_0000 + 32 + row * 4, 0x1111_1111); // OBJ tile 1
        }
        // The layer under the sprite has to be opaque where the sprite sits,
        // otherwise the second layer is the backdrop and nothing blends. Line 22
        // is tile row 2, whose map entries start at index 2 * 32.
        for tx in 0..30 {
            mem.write16(0x0600_F800 + (2 * 32 + tx) * 2, (1 << 12) | 1); // BG0: tile 1
        }
        mem.write16(0x0700_0000, 20 | (1 << 10)); // attr0: y=20, OBJ mode 1
        mem.write16(0x0700_0002, 10); // attr1: x=10
        mem.write16(0x0700_0004, 1); // attr2: tile 1
        // Mode 0, first target OBJ, second target BG0, EVA = EVB = 8.
        mem.write16(0x0400_0050, 0x0110);
        mem.write16(0x0400_0052, 0x0808);
        render_scanline(&mut mem, 22);
        let blended = alpha_blend(0x001F, 0x03E0, 8, 8);
        assert_ne!(blended, 0x001F, "the two layers must differ after blending");
        assert_eq!(mem.framebuffer[22 * 240 + 10], blended);
        assert_eq!(mem.framebuffer[22 * 240 + 18], 0x03E0, "outside the sprite: BG0");
    }
}
