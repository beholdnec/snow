//! Native clock of the devices of a bus, which the CPU may run faster than

use serde::{Deserialize, Serialize};

use crate::tickable::Ticks;

/// What a step of the native clock did
pub struct Step {
    /// Native ticks that elapsed
    pub native: Ticks,
    /// The VIA clock ticked at the end of them
    pub via_tick: bool,
    /// CPU ticks they consumed
    pub cpu_used: Ticks,
}

/// Native clock of a machine's device bus, which the CPU may run faster
/// than when overclocking.
///
/// The CPU runs on CPU ticks, the devices on native ticks. A native tick is
/// never shorter than a CPU tick. The clock also produces the VIA clock,
/// known as the E clock on the 68000 Macs, which ticks every `via_divider`
/// native ticks.
#[derive(Serialize, Deserialize)]
pub struct NativeClock {
    /// Native frequency
    native_freq: Ticks,
    /// Frequency of the CPU, which includes any overclocking
    cpu_freq: Ticks,
    /// Native ticks per VIA tick
    via_divider: Ticks,
    /// Progress towards the next native tick: a CPU tick adds `native_freq`
    /// to it, and a native tick takes `cpu_freq`
    progress: Ticks,
    /// Native ticks since the last VIA tick
    via_phase: Ticks,
}

impl NativeClock {
    /// Native clock at `native_freq`, with a VIA tick every `via_divider`
    /// native ticks
    pub fn new(native_freq: Ticks, via_divider: Ticks) -> Self {
        Self {
            native_freq,
            cpu_freq: native_freq,
            via_divider,
            progress: 0,
            via_phase: 0,
        }
    }

    /// Sets the frequency of the CPU, which includes any overclocking
    pub fn set_cpu_frequency(&mut self, cpu_freq: Ticks) {
        self.cpu_freq = cpu_freq;
        self.progress = 0;
    }

    /// Runs the clock for up to `cpu_ticks` CPU ticks, stopping at the next
    /// VIA tick or after `max` native ticks, whichever comes first.
    ///
    /// `max` is typically used to stop on the bus's own events, such as the
    /// hblank and vblank boundaries of the compact Macs' video: `None` for
    /// no limit, otherwise at least 1.
    ///
    /// Returns how many native ticks were advanced, how many CPU ticks they
    /// consumed, and whether the clock stopped on a VIA tick. When
    /// the CPU ticks don't complete a native tick, they are all consumed and
    /// 0 native ticks are advanced.
    #[inline]
    pub fn advance(&mut self, cpu_ticks: Ticks, max: Option<Ticks>) -> Step {
        debug_assert!(max != Some(0));
        let to_via = self.via_divider - self.via_phase;
        let limit = max.map_or(to_via, |max| max.min(to_via));

        let (native, cpu_used) = if self.cpu_freq == self.native_freq {
            let native = limit.min(cpu_ticks);
            (native, native)
        } else {
            // CPU ticks for `limit` native ticks to complete
            let needed = (limit * self.cpu_freq - self.progress).div_ceil(self.native_freq);
            if needed <= cpu_ticks {
                self.progress = self.progress + needed * self.native_freq - limit * self.cpu_freq;
                (limit, needed)
            } else {
                self.progress += cpu_ticks * self.native_freq;
                let native = self.progress / self.cpu_freq;
                self.progress -= native * self.cpu_freq;
                (native, cpu_ticks)
            }
        };

        self.via_phase += native;
        let via_tick = self.via_phase == self.via_divider;
        if via_tick {
            self.via_phase = 0;
        }
        Step {
            native,
            via_tick,
            cpu_used,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `cpu` CPU ticks through the clock, returns the native ticks and
    /// the VIA ticks
    fn run(clock: &mut NativeClock, mut cpu: Ticks) -> (Ticks, Ticks) {
        let (mut native, mut via) = (0, 0);
        while cpu > 0 {
            let step = clock.advance(cpu, None);
            if step.native == 0 {
                break;
            }
            cpu -= step.cpu_used;
            native += step.native;
            via += Ticks::from(step.via_tick);
        }
        (native, via)
    }

    #[test]
    fn native_speed() {
        let mut clock = NativeClock::new(1000, 10);
        assert_eq!(run(&mut clock, 25), (25, 2));
        let step = clock.advance(100, None);
        assert_eq!((step.native, step.via_tick, step.cpu_used), (5, true, 5));
    }

    #[test]
    fn twice_as_fast() {
        let mut clock = NativeClock::new(1000, 10);
        clock.set_cpu_frequency(2000);
        assert_eq!(run(&mut clock, 41), (20, 2));
        // The odd CPU tick is half a native tick: one more completes it
        assert_eq!(run(&mut clock, 1), (1, 0));
    }

    #[test]
    fn uneven_ratio() {
        let mut clock = NativeClock::new(3, 10);
        clock.set_cpu_frequency(7);
        // 70 CPU ticks are 30 native ticks, whatever the batches
        let mut native = 0;
        for cpu in [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 15] {
            native += run(&mut clock, cpu).0;
        }
        assert_eq!(native, 30);
    }

    #[test]
    fn stops_at_max() {
        let mut clock = NativeClock::new(1000, 10);
        clock.set_cpu_frequency(4000);
        let step = clock.advance(100, Some(3));
        assert_eq!((step.native, step.via_tick, step.cpu_used), (3, false, 12));
        let step = clock.advance(100, Some(20));
        assert_eq!((step.native, step.via_tick, step.cpu_used), (7, true, 28));
    }

    #[test]
    fn short_batch() {
        let mut clock = NativeClock::new(1000, 10);
        clock.set_cpu_frequency(4000);
        let step = clock.advance(3, None);
        assert_eq!((step.native, step.via_tick, step.cpu_used), (0, false, 3));
        let step = clock.advance(1, None);
        assert_eq!((step.native, step.via_tick, step.cpu_used), (1, false, 1));
    }
}
