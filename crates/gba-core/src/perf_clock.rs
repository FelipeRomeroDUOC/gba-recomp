//! Cheap monotonic timestamps for the per-phase performance counters.
//!
//! The counters are diagnostic, but they sit inside the scanline loop: the PPU
//! takes four timestamps per line, so a frame is 640 clock reads. On the Android
//! TV the port targets, `std::time::Instant::now()` costs ~1.9 µs (the port
//! measures it and reports it as `reloj=` in its telemetry header; thirteen
//! sessions on that device family ranged 1394-2168 ns), which spends ~1.2 ms
//! per frame measuring the frame. The virtual counter is one register read.
//!
//! Per-platform, by design (no third-party crate):
//!   - aarch64: `CNTVCT_EL0`, the architectural virtual counter. Android keeps
//!     it readable from EL0 and its rate comes from `CNTFRQ_EL0`.
//!   - Everything else (host tests, x86_64): nanoseconds from a process-local
//!     `Instant` origin, where a clock read costs ~30 ns and rate is irrelevant.

/// Reads the monotonic tick source.
#[inline(always)]
pub fn ticks() -> u64 {
    imp::ticks()
}

/// Nanoseconds elapsed between two readings, in either order.
///
/// Only deltas are converted, and a delta is scanline-sized, so `delta * 1e9`
/// cannot overflow u64: it would take ~5 hours of ticks at 19.2 MHz.
/// A zero frequency (firmware left `CNTFRQ_EL0` unprogrammed) reports 0 rather
/// than inventing a rate.
#[inline(always)]
pub fn ns_between(start: u64, end: u64) -> u64 {
    let delta = end.wrapping_sub(start);
    match imp::hz() {
        0 => 0,
        hz => delta.saturating_mul(1_000_000_000) / hz,
    }
}

/// Nanoseconds elapsed since `start`.
#[inline(always)]
pub fn ns_since(start: u64) -> u64 {
    ns_between(start, ticks())
}

#[cfg(target_arch = "aarch64")]
mod imp {
    use core::arch::asm;

    /// `CNTVCT_EL0`: the 64-bit virtual counter, readable from userspace.
    #[inline(always)]
    pub fn ticks() -> u64 {
        let v: u64;
        // SAFETY: reading the virtual counter has no side effects and is always
        // allowed from EL0 on AArch64 (Android does not trap it).
        unsafe {
            asm!("mrs {v}, cntvct_el0", v = out(reg) v, options(nomem, nostack, preserves_flags));
        }
        v
    }

    /// `CNTFRQ_EL0`: the counter rate in Hz.
    #[inline(always)]
    pub fn hz() -> u64 {
        let hz: u64;
        // SAFETY: as above — a plain system-register read.
        unsafe {
            asm!("mrs {hz}, cntfrq_el0", hz = out(reg) hz, options(nomem, nostack, preserves_flags));
        }
        hz
    }
}

#[cfg(not(target_arch = "aarch64"))]
mod imp {
    use std::sync::OnceLock;
    use std::time::Instant;

    fn origin() -> Instant {
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        *ORIGIN.get_or_init(Instant::now)
    }

    /// Nanoseconds since the first reading on this target.
    #[inline(always)]
    pub fn ticks() -> u64 {
        origin().elapsed().as_nanos() as u64
    }

    /// Ticks are already nanoseconds here.
    #[inline(always)]
    pub fn hz() -> u64 {
        1_000_000_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_length_span_is_zero_and_conversions_are_monotonic() {
        let a = ticks();
        let mut spins = 0u64;
        let b = loop {
            let b = ticks();
            if b != a {
                break b;
            }
            spins += 1;
            assert!(spins < 10_000_000, "el contador monotono no avanzo");
        };
        assert_eq!(ns_between(a, a), 0, "una medida de longitud cero debe dar 0");
        assert!(
            ns_between(a, b) <= ns_between(a, b.saturating_add(1_000_000_000)),
            "un tramo mas largo no puede medir menos"
        );
        assert!(ns_since(a) >= ns_between(a, b));
    }

    #[test]
    fn a_second_of_ticks_is_a_second_of_nanoseconds() {
        let hz = imp::hz();
        if hz == 0 {
            // Firmware left CNTFRQ_EL0 unprogrammed: the counters report 0 by
            // design, so there is no rate to convert with.
            return;
        }
        assert_eq!(ns_between(0, hz), 1_000_000_000);
    }
}
