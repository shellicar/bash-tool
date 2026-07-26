//! The `time` keyword's time sources, behind a seam so tests control time
//! instead of trusting the real clock.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub user: Duration,
    pub sys: Duration,
}

pub trait Clock {
    /// Monotonic elapsed time since an arbitrary fixed origin.
    fn now_monotonic(&self) -> Duration;
    /// Accumulated CPU time of this process plus its waited-for children —
    /// the set bash's `time` reports on.
    fn cpu_times(&self) -> CpuTimes;
}

pub struct RealClock {
    origin: Instant,
}

impl Default for RealClock {
    fn default() -> Self {
        Self { origin: Instant::now() }
    }
}

impl Clock for RealClock {
    fn now_monotonic(&self) -> Duration {
        self.origin.elapsed()
    }

    fn cpu_times(&self) -> CpuTimes {
        let mut total = CpuTimes::default();
        for who in [libc::RUSAGE_SELF, libc::RUSAGE_CHILDREN] {
            let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
            if unsafe { libc::getrusage(who, &mut ru) } == 0 {
                total.user += timeval_duration(ru.ru_utime);
                total.sys += timeval_duration(ru.ru_stime);
            }
        }
        total
    }
}

fn timeval_duration(tv: libc::timeval) -> Duration {
    Duration::new(tv.tv_sec.max(0) as u64, (tv.tv_usec.max(0) as u32) * 1000)
}
