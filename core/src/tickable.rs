use anyhow::Result;

/// Representation of X amount of ticks (T-cycles)
/// of the main system clock.
pub type Ticks = usize; // TODO: should be u64

pub trait Tickable {
    fn tick(&mut self, ticks: Ticks) -> Result<Ticks>;
}
