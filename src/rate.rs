/*!
 * Pages indexed per minute (#264): a count per minute for the last hour, in a fixed array, and
 * the average over the last few full minutes.
 */
use std::sync::Mutex;
use std::time::Instant;

const MINUTES: usize = 60;
// the average covers this many full minutes
const WINDOW: u64 = 10;

pub struct Rate {
    start: Instant,
    // per minute since start, at index minute % MINUTES: (that minute, its count)
    minutes: Mutex<[(u64, u64); MINUTES]>,
}

impl Rate {
    pub fn new() -> Rate {
        Rate::starting(Instant::now())
    }

    fn starting(start: Instant) -> Rate {
        Rate {
            start,
            minutes: Mutex::new([(0, 0); MINUTES]),
        }
    }

    /// Counts one page now
    pub fn add(&self) {
        self.add_at(Instant::now())
    }

    /// The average per minute over the last WINDOW full minutes (fewer just after start; 0 in
    /// the first minute)
    pub fn per_minute(&self) -> u64 {
        self.per_minute_at(Instant::now())
    }

    // every page counted in the last hour
    #[cfg(test)]
    pub(crate) fn counted(&self) -> u64 {
        self.minutes.lock().unwrap().iter().map(|m| m.1).sum()
    }

    fn minute(&self, now: Instant) -> u64 {
        now.duration_since(self.start).as_secs() / 60
    }

    fn add_at(&self, now: Instant) {
        let minute = self.minute(now);
        let mut minutes = self.minutes.lock().unwrap_or_else(|e| e.into_inner());
        let slot = &mut minutes[minute as usize % MINUTES];
        // a slot last used an hour or more ago starts again
        if slot.0 != minute {
            *slot = (minute, 0);
        }
        slot.1 += 1;
    }

    fn per_minute_at(&self, now: Instant) -> u64 {
        let current = self.minute(now);
        let full = current.min(WINDOW);
        if full == 0 {
            return 0;
        }
        let minutes = self.minutes.lock().unwrap_or_else(|e| e.into_inner());
        let total: u64 = (current - full..current)
            .map(|minute| match minutes[minute as usize % MINUTES] {
                (at, count) if at == minute => count,
                _ => 0,
            })
            .sum();
        total / full
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(start: Instant, seconds: u64) -> Instant {
        start + Duration::from_secs(seconds)
    }

    #[test]
    fn nothing_is_reported_in_the_first_minute() {
        let start = Instant::now();
        let rate = Rate::starting(start);
        for _ in 0..50 {
            rate.add_at(at(start, 10));
        }
        assert_eq!(rate.per_minute_at(at(start, 59)), 0);
    }

    #[test]
    fn the_average_covers_only_the_full_minutes_since_start() {
        let start = Instant::now();
        let rate = Rate::starting(start);
        for _ in 0..30 {
            rate.add_at(at(start, 5));
        }
        for _ in 0..90 {
            rate.add_at(at(start, 70));
        }
        // the minute in progress doesn't count
        rate.add_at(at(start, 130));
        assert_eq!(rate.per_minute_at(at(start, 125)), 60, "(30 + 90) / 2");
    }

    #[test]
    fn the_average_is_over_the_last_ten_full_minutes() {
        let start = Instant::now();
        let rate = Rate::starting(start);
        for minute in 0..20u64 {
            for _ in 0..minute {
                rate.add_at(at(start, minute * 60 + 1));
            }
        }
        // minutes 10..=19: (10 + 19) * 10 / 2 = 145 pages
        assert_eq!(rate.per_minute_at(at(start, 20 * 60)), 14);
    }

    #[test]
    fn a_slot_reused_an_hour_later_starts_again_and_old_ones_dont_count() {
        let start = Instant::now();
        let rate = Rate::starting(start);
        for _ in 0..100 {
            rate.add_at(at(start, 5 * 60));
        }
        // an hour later the same slot holds minute 65, counted from zero
        for _ in 0..20 {
            rate.add_at(at(start, 65 * 60));
        }
        assert_eq!(
            rate.per_minute_at(at(start, 66 * 60)),
            2,
            "20 over minutes 56..=65"
        );
        // long idle: nothing in the window
        assert_eq!(rate.per_minute_at(at(start, 300 * 60)), 0);
    }
}
