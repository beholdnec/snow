//! Native clock of the devices of a bus, which the CPU may run faster than

use serde::{Deserialize, Serialize};

use crate::bus::access::{BusAccess, CpuBus};
use crate::tickable::Ticks;

/// Wait states, in CPU ticks
pub type WaitTicks = Ticks;

/// What a device access cycle does
pub enum DeviceCycle {
    /// Stall the CPU for this many CPU ticks, then it presents the cycle
    /// again
    Stall(Ticks),
    /// Do the access now, with these wait states
    Access(WaitTicks),
}

/// What a step of the native clock did
pub struct Step {
    /// Native ticks that elapsed
    pub native: Ticks,
    /// The VIA clock ticked at the end of them
    pub via_tick: bool,
    /// CPU ticks they consumed
    pub cpu_used: Ticks,
}

/// Helper struct used to work with fractional tick values when scaling from
/// an input frequency to an output frequency.
///
/// Rational arithmetic is used to avoid errors.
#[derive(Copy, Clone, Serialize, Deserialize)]
struct ClockScaler {
    /// Represents the number of elapsed output ticks.
    ///
    /// The number of elapsed output ticks is `N / in_freq`. On each input tick,
    /// the value increases by `out_freq`.
    n: Ticks,
    in_freq: Ticks,
    out_freq: Ticks,
}

impl ClockScaler {
    fn new(in_freq: Ticks, out_freq: Ticks) -> Self {
        ClockScaler {
            n: 0,
            in_freq,
            out_freq,
        }
    }

    fn add_in_ticks(&mut self, in_ticks: Ticks) {
        self.n += in_ticks * self.out_freq;
    }

    fn subtract_out_ticks(&mut self, out_ticks: Ticks) {
        self.n -= out_ticks * self.in_freq;
    }

    fn out_ticks(&self) -> Ticks {
        self.n / self.in_freq
    }

    /// Return the number of input ticks needed until a given number of output ticks
    /// have elapsed
    fn in_ticks_until_out_ticks(&self, out_ticks: Ticks) -> Ticks {
        (out_ticks * self.in_freq)
            .saturating_sub(self.n)
            .div_ceil(self.out_freq)
    }
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
    /// The value `n` in the CPU-to-native clock downscaler
    native_clockgen: ClockScaler,
    /// Native ticks since the last VIA tick
    via_phase: Ticks,
    /// Native ticks elapsed
    native_ticks: Ticks,
}

impl NativeClock {
    /// Native clock at `native_freq`, with a VIA tick every `via_divider`
    /// native ticks
    pub fn new(native_freq: Ticks, via_divider: Ticks) -> Self {
        Self {
            native_freq,
            cpu_freq: native_freq,
            via_divider,
            native_clockgen: ClockScaler::new(native_freq, native_freq),
            via_phase: 0,
            native_ticks: 0,
        }
    }

    /// Sets the frequency of the CPU, which includes any overclocking
    pub fn set_cpu_frequency(&mut self, cpu_freq: Ticks) {
        self.cpu_freq = cpu_freq;
        self.native_clockgen = ClockScaler::new(cpu_freq, self.native_freq);
    }

    /// True when the CPU runs faster than the native clock
    pub fn is_overclocking(&self) -> bool {
        self.cpu_freq != self.native_freq
    }

    /// Native ticks elapsed
    pub fn native_ticks(&self) -> Ticks {
        self.native_ticks
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
            let needed = self.native_clockgen.in_ticks_until_out_ticks(limit);
            if needed <= cpu_ticks {
                self.native_clockgen.add_in_ticks(needed);
                self.native_clockgen.subtract_out_ticks(limit);
                (limit, needed)
            } else {
                self.native_clockgen.add_in_ticks(cpu_ticks);
                let native = self.native_clockgen.out_ticks();
                self.native_clockgen.subtract_out_ticks(native);
                (native, cpu_ticks)
            }
        };

        self.native_ticks += native;
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

    /// Start of a device access cycle.
    ///
    /// When overclocking, the CPU waits until the native clock has caught
    /// up with it and a native tick starts; the access then takes the CPU's
    /// base cycle, counted in native ticks. Without overclocking, the access
    /// goes ahead with no wait states.
    ///
    /// Returns a DeviceCycle, which indicates whether to stall, or the wait
    /// states of the base device cycle.
    pub fn device_sync<C: CpuBus>(&self, acc: &BusAccess<C>) -> DeviceCycle {
        if !self.is_overclocking() {
            return DeviceCycle::Access(0);
        }
        let (_, into_tick) = self.cpu_position(acc.pending);
        let on_tick = into_tick < self.native_freq;
        if on_tick && acc.pending == 0 {
            return DeviceCycle::Access(self.scale_dev_waits(0, C::CYCLE) - C::CYCLE);
        }
        if on_tick {
            // Only to let the clock catch up with the CPU
            return DeviceCycle::Stall(0);
        }
        DeviceCycle::Stall(self.cpu_ticks_until(1, into_tick))
    }

    /// Start of a VIA access cycle: the same, on a VIA tick instead of a
    /// native tick, with or without overclocking. The bus adds how long the
    /// VIA cycle lasts from the VIA tick.
    ///
    /// Returns a DeviceCycle, which indicates whether to stall, or the wait
    /// states of the base device cycle.
    pub fn via_sync<C: CpuBus>(&self, acc: &BusAccess<C>) -> DeviceCycle {
        let (ticks_done, into_tick) = self.cpu_position(acc.pending);
        let via_phase = (self.via_phase + ticks_done) % self.via_divider;
        let on_via_tick = via_phase == 0 && into_tick < self.native_freq;
        if on_via_tick && acc.pending == 0 {
            return DeviceCycle::Access(self.scale_dev_waits(0, C::CYCLE) - C::CYCLE);
        }
        if on_via_tick {
            // Only to let the clock catch up with the CPU
            return DeviceCycle::Stall(0);
        }
        DeviceCycle::Stall(self.cpu_ticks_until(self.via_divider - via_phase, into_tick))
    }

    /// Adds the device's own wait states to the wait states of a device
    /// cycle, and returns the resulting total wait states, scaled to the CPU
    /// clock.
    ///
    /// `w` is what device_sync() or via_sync() returned: the duration of the
    /// device access cycle itself, beyond the CPU's base cycle
    /// (`CpuBus::CYCLE`), already in CPU ticks. `extra` is the number of
    /// native wait states the device adds itself (wait states inserted by
    /// the GLUE, or a recovery time). Without overclocking, this is
    /// `w + extra`.
    pub fn scale_dev_waits(&self, w: WaitTicks, extra: Ticks) -> WaitTicks {
        w + (extra * self.cpu_freq).div_ceil(self.native_freq)
    }
}

/// Private helpers of device_sync() and via_sync()
impl NativeClock {
    /// Where the CPU is, `cpu_ahead` CPU ticks after the clock was last
    /// advanced: the native ticks it has completed since, and how far it is
    /// into the next one
    ///
    /// TODO (beholdnec): I'd like to get rid of this method
    fn cpu_position(&self, cpu_ahead: Ticks) -> (Ticks, Ticks) {
        let mut progress = self.native_clockgen;
        progress.add_in_ticks(cpu_ahead);
        (progress.out_ticks(), progress.n % self.cpu_freq)
    }

    /// CPU ticks until `native_ticks` more native ticks have completed, from
    /// `into_tick` into the current one
    ///
    /// TODO (beholdnec): I'd like to get rid of this method
    fn cpu_ticks_until(&self, native_ticks: Ticks, into_tick: Ticks) -> Ticks {
        (native_ticks * self.cpu_freq - into_tick).div_ceil(self.native_freq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::access::Bus68000;

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

    fn read(pending: Ticks) -> BusAccess<Bus68000> {
        BusAccess::read(0, 1, pending)
    }

    fn sync(clock: &mut NativeClock, cycle: DeviceCycle) -> Option<WaitTicks> {
        match cycle {
            DeviceCycle::Stall(n) => {
                run(clock, n);
                None
            }
            DeviceCycle::Access(w) => Some(w),
        }
    }

    #[test]
    fn device_sync_native_speed() {
        let clock = NativeClock::new(1000, 10);
        assert!(matches!(
            clock.device_sync(&read(7)),
            DeviceCycle::Access(0)
        ));
    }

    #[test]
    fn device_sync_overclocked() {
        let mut clock = NativeClock::new(1000, 10);
        clock.set_cpu_frequency(4000);
        // 6 CPU ticks in: half way through the second native tick
        let cycle = clock.device_sync(&read(6));
        assert!(matches!(cycle, DeviceCycle::Stall(2)));
        run(&mut clock, 8);
        // On a native tick: 4 native ticks of the 68000's cycle
        let cycle = clock.device_sync(&read(0));
        let w = sync(&mut clock, cycle);
        assert_eq!(w, Some(16 - 4));
    }

    #[test]
    fn via_sync_native_speed() {
        let mut clock = NativeClock::new(1000, 10);
        // On a VIA tick, up to date
        assert!(matches!(clock.via_sync(&read(0)), DeviceCycle::Access(0)));
        // On a VIA tick, but the clock has to be brought up to date first
        assert!(matches!(clock.via_sync(&read(10)), DeviceCycle::Stall(0)));
        run(&mut clock, 3);
        assert!(matches!(clock.via_sync(&read(0)), DeviceCycle::Stall(7)));
        assert!(matches!(clock.via_sync(&read(2)), DeviceCycle::Stall(5)));
    }

    #[test]
    fn via_sync_overclocked() {
        let mut clock = NativeClock::new(1000, 10);
        clock.set_cpu_frequency(3000);
        run(&mut clock, 4);
        // A third of a native tick past native tick 1
        let cycle = clock.via_sync(&read(0));
        assert!(matches!(cycle, DeviceCycle::Stall(26)));
        assert_eq!(sync(&mut clock, cycle), None);
        let cycle = clock.via_sync(&read(0));
        let w = sync(&mut clock, cycle);
        assert_eq!(w, Some(12 - 4));
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
