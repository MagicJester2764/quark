use crate::time::Duration;

const TICK_PERIOD_NS: u64 = 10_000_000; // 10 ms per tick (100 Hz PIT)

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Instant(u64); // ticks

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct SystemTime(u64); // ticks since boot (no RTC yet)

pub const UNIX_EPOCH: SystemTime = SystemTime(0);

impl Instant {
    pub fn now() -> Instant {
        Instant(quark_rt::rt::ticks())
    }

    pub fn checked_sub_instant(&self, other: &Instant) -> Option<Duration> {
        self.0.checked_sub(other.0).map(|ticks| Duration::from_nanos(ticks * TICK_PERIOD_NS))
    }

    pub fn checked_add_duration(&self, other: &Duration) -> Option<Instant> {
        let ticks = other.as_nanos() as u64 / TICK_PERIOD_NS;
        self.0.checked_add(ticks).map(Instant)
    }

    pub fn checked_sub_duration(&self, other: &Duration) -> Option<Instant> {
        let ticks = other.as_nanos() as u64 / TICK_PERIOD_NS;
        self.0.checked_sub(ticks).map(Instant)
    }
}

impl SystemTime {
    pub fn now() -> SystemTime {
        SystemTime(quark_rt::rt::ticks())
    }

    pub fn sub_time(&self, other: &SystemTime) -> Result<Duration, Duration> {
        if self.0 >= other.0 {
            let ticks = self.0 - other.0;
            Ok(Duration::from_nanos(ticks * TICK_PERIOD_NS))
        } else {
            let ticks = other.0 - self.0;
            Err(Duration::from_nanos(ticks * TICK_PERIOD_NS))
        }
    }

    pub fn checked_add_duration(&self, other: &Duration) -> Option<SystemTime> {
        let ticks = other.as_nanos() as u64 / TICK_PERIOD_NS;
        self.0.checked_add(ticks).map(SystemTime)
    }

    pub fn checked_sub_duration(&self, other: &Duration) -> Option<SystemTime> {
        let ticks = other.as_nanos() as u64 / TICK_PERIOD_NS;
        self.0.checked_sub(ticks).map(SystemTime)
    }
}
