//! Hybrid logical clock: wall-clock milliseconds plus a counter. Two writes made at the same millisecond, or on
//! devices whose clocks differ a little, still get a total order, and a device never stamps a write older than
//! one it has already seen.

use std::time::{SystemTime, UNIX_EPOCH};

/// A stamp from the future by more than this is refused (a phone with a wildly wrong clock would otherwise win
/// every conflict forever).
pub const MAX_DRIFT_MS: u64 = 24 * 60 * 60 * 1000;

/// Ordered by wall time, then counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct Hlc {
    pub wall: u64,
    pub counter: u32,
}

impl From<(u64, u32)> for Hlc {
    fn from((wall, counter): (u64, u32)) -> Self {
        Hlc { wall, counter }
    }
}

impl From<Hlc> for (u64, u32) {
    fn from(h: Hlc) -> Self {
        (h.wall, h.counter)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ClockSkew {
    pub ahead_ms: u64,
}

pub struct HlcClock {
    last: Hlc,
    now: Box<dyn Fn() -> u64 + Send>,
}

impl std::fmt::Debug for HlcClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HlcClock")
            .field("last", &self.last)
            .finish()
    }
}

fn system_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl HlcClock {
    pub fn system() -> Self {
        Self::with_source(system_ms)
    }

    /// A clock reading time from `now` (tests drive it by hand).
    pub fn with_source(now: impl Fn() -> u64 + Send + 'static) -> Self {
        HlcClock {
            last: Hlc::default(),
            now: Box::new(now),
        }
    }

    pub fn last(&self) -> Hlc {
        self.last
    }

    /// Moves the clock forward to at least `seen` without any drift check: used when loading stamps this device
    /// wrote itself, so a system clock that was set back never produces older stamps.
    pub fn advance_to(&mut self, seen: Hlc) {
        if seen > self.last {
            self.last = seen;
        }
    }

    /// A stamp for a local write, newer than everything this clock has produced or seen.
    pub fn tick(&mut self) -> Hlc {
        let now = (self.now)();
        if now > self.last.wall {
            self.last = Hlc {
                wall: now,
                counter: 0,
            };
        } else {
            self.last.counter = self.last.counter.saturating_add(1);
        }
        self.last
    }

    /// Takes a stamp received from the other device into account, so the next local write is newer than it.
    pub fn observe(&mut self, remote: Hlc) -> Result<(), ClockSkew> {
        let now = (self.now)();
        if remote.wall > now.saturating_add(MAX_DRIFT_MS) {
            return Err(ClockSkew {
                ahead_ms: remote.wall - now,
            });
        }
        let wall = self.last.wall.max(remote.wall).max(now);
        let counter = if wall == self.last.wall && wall == remote.wall {
            self.last.counter.max(remote.counter).saturating_add(1)
        } else if wall == self.last.wall {
            self.last.counter.saturating_add(1)
        } else if wall == remote.wall {
            remote.counter.saturating_add(1)
        } else {
            0
        };
        self.last = Hlc { wall, counter };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    fn manual(start: u64) -> (HlcClock, Arc<AtomicU64>) {
        let time = Arc::new(AtomicU64::new(start));
        let t = time.clone();
        (
            HlcClock::with_source(move || t.load(Ordering::SeqCst)),
            time,
        )
    }

    #[test]
    fn ticks_are_strictly_increasing_even_within_one_millisecond() {
        let (mut clock, time) = manual(1000);
        let a = clock.tick();
        let b = clock.tick();
        assert_eq!(
            (a, b),
            (
                Hlc {
                    wall: 1000,
                    counter: 0
                },
                Hlc {
                    wall: 1000,
                    counter: 1
                }
            )
        );
        time.store(1001, Ordering::SeqCst);
        assert_eq!(
            clock.tick(),
            Hlc {
                wall: 1001,
                counter: 0
            }
        );
    }

    #[test]
    fn a_clock_set_back_never_goes_backwards() {
        let (mut clock, time) = manual(5000);
        let before = clock.tick();
        time.store(100, Ordering::SeqCst);
        assert!(clock.tick() > before);
    }

    #[test]
    fn observing_a_newer_remote_stamp_makes_the_next_write_newer_than_it() {
        let (mut clock, _) = manual(1000);
        clock
            .observe(Hlc {
                wall: 2000,
                counter: 5,
            })
            .unwrap();
        assert!(
            clock.tick()
                > Hlc {
                    wall: 2000,
                    counter: 5
                }
        );
        let (mut clock, _) = manual(3000);
        clock
            .observe(Hlc {
                wall: 2000,
                counter: 5,
            })
            .unwrap();
        assert_eq!(
            clock.last(),
            Hlc {
                wall: 3000,
                counter: 0
            },
            "our own time is already ahead"
        );
        let (mut clock, _) = manual(2000);
        clock.tick();
        clock
            .observe(Hlc {
                wall: 2000,
                counter: 5,
            })
            .unwrap();
        assert_eq!(
            clock.last(),
            Hlc {
                wall: 2000,
                counter: 6
            }
        );
    }

    #[test]
    fn a_stamp_far_in_the_future_is_refused() {
        let (mut clock, _) = manual(1000);
        let err = clock
            .observe(Hlc {
                wall: 1000 + MAX_DRIFT_MS + 1,
                counter: 0,
            })
            .unwrap_err();
        assert_eq!(err.ahead_ms, MAX_DRIFT_MS + 1);
        assert!(clock
            .observe(Hlc {
                wall: 1000 + MAX_DRIFT_MS,
                counter: 0
            })
            .is_ok());
    }

    #[test]
    fn advance_to_restores_the_saved_clock() {
        let (mut clock, _) = manual(10);
        clock.advance_to(Hlc {
            wall: 9999,
            counter: 3,
        });
        assert!(
            clock.tick()
                > Hlc {
                    wall: 9999,
                    counter: 3
                }
        );
    }
}
