use std::cell::Cell;
use std::time::{Duration, Instant};

const SUSPENDS: [&str; 2] = ["/sys/power/suspend_stats/success", "/sys/power/suspend_stats/fail"];
const STALL: Duration = Duration::from_secs(6);

fn clock(id: libc::clockid_t) -> Option<Duration> {
    let mut at: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(id, &mut at) } != 0 {
        return None;
    }
    Some(Duration::new(u64::try_from(at.tv_sec).ok()?, u32::try_from(at.tv_nsec).ok()?))
}

fn asleep() -> Option<(Duration, Duration)> {
    let (before, since_boot, after) = (clock(libc::CLOCK_MONOTONIC)?, clock(libc::CLOCK_BOOTTIME)?, clock(libc::CLOCK_MONOTONIC)?);
    Some((since_boot.saturating_sub(after), since_boot.saturating_sub(before)))
}

fn counted(paths: &[&str]) -> Option<u64> {
    paths.iter().map(|path| std::fs::read_to_string(path).ok()?.trim().parse::<u64>().ok()).sum()
}

pub fn suspends() -> Option<u64> {
    counted(&SUSPENDS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reading {
    asleep: Option<Duration>,
    suspends: Option<u64>,
}

fn slept_between(before: Reading, now: Reading) -> bool {
    let by_clock = match (before.asleep, now.asleep) {
        (Some(at_most), Some(at_least)) => at_least > at_most,
        (None, None) => false,
        _ => true,
    };
    by_clock || before.suspends != now.suspends
}

fn stalled(last_look: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last_look) > STALL
}

pub struct Vigil {
    began: Reading,
    last_look: Cell<Option<Instant>>,
    over: Cell<bool>,
}

impl Vigil {
    pub fn begin() -> Self {
        let began = Reading { asleep: asleep().map(|(_, at_most)| at_most), suspends: suspends() };
        Vigil { began, last_look: Cell::new(None), over: Cell::new(false) }
    }

    pub fn slept(&self) -> bool {
        let now = Instant::now();
        let frozen = self.last_look.replace(Some(now)).is_some_and(|before| stalled(before, now));
        self.over.set(self.over.get() || frozen);
        self.ended()
    }

    pub fn ended(&self) -> bool {
        let asleep = slept_between(self.began, Reading { asleep: asleep().map(|(at_least, _)| at_least), suspends: suspends() });
        self.over.set(self.over.get() || asleep);
        self.over.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(3600);

    fn reading(asleep: Option<Duration>, suspends: Option<u64>) -> Reading {
        Reading { asleep, suspends }
    }

    #[test]
    fn a_vigil_ends_with_any_sign_of_sleep_and_with_nothing_else() {
        let vigil = Vigil::begin();
        std::thread::sleep(Duration::from_millis(30));
        assert!(!vigil.slept());
        let (at_least, at_most) = asleep().unwrap();
        assert!(at_least <= at_most && at_most - at_least < Duration::from_millis(50));
        let before = reading(Some(HOUR), Some(7));
        assert!(!slept_between(before, before));
        assert!(!slept_between(before, reading(Some(HOUR - Duration::from_micros(3)), Some(7))));
        assert!(slept_between(before, reading(Some(HOUR + Duration::from_millis(1)), Some(7))));
        assert!(slept_between(before, reading(Some(HOUR), Some(8))));
        assert!(slept_between(before, reading(Some(HOUR), None)));
        assert!(slept_between(before, reading(None, Some(7))));
        let blind = reading(None, None);
        assert!(!slept_between(blind, blind));
        assert!(slept_between(blind, reading(Some(HOUR), None)));
        let uncounted = reading(Some(HOUR), None);
        assert!(!slept_between(uncounted, uncounted));
        assert!(slept_between(uncounted, reading(Some(HOUR + Duration::from_secs(2)), None)));
    }

    #[test]
    fn a_long_gap_between_two_looks_ends_the_vigil_for_good() {
        let start = Instant::now();
        assert!(!stalled(start, start));
        assert!(!stalled(start, start + STALL));
        assert!(stalled(start, start + STALL + Duration::from_millis(1)));
        assert!(!stalled(start + STALL, start));
        let long_ago = || Some(Instant::now() - STALL - Duration::from_millis(50));
        let opening = Vigil::begin();
        assert!(opening.last_look.get().is_none());
        assert!(!opening.slept());
        assert!(opening.last_look.get().is_some());
        let vigil = Vigil::begin();
        assert!(!vigil.slept());
        vigil.last_look.set(long_ago());
        assert!(!vigil.ended());
        assert!(vigil.slept());
        assert!(vigil.slept() && vigil.ended());
    }

    #[test]
    fn suspend_counters_add_up_or_are_absent_together() {
        let dir = std::env::temp_dir().join(format!("elanfp-vigil-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = |name: &str, text: &str| {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            path.to_string_lossy().into_owned()
        };
        let (done, failed, broken) = (file("success", "41\n"), file("fail", "1\n"), file("broken", "many\n"));
        let missing = dir.join("missing").to_string_lossy().into_owned();
        assert_eq!(counted(&[&done, &failed]), Some(42));
        assert_eq!(counted(&[&done]), Some(41));
        assert_eq!(counted(&[&done, &missing]), None);
        assert_eq!(counted(&[&broken, &failed]), None);
        assert_eq!(counted(&[]), Some(0));
        std::fs::remove_dir_all(&dir).unwrap();
        if let Some(total) = suspends() {
            assert_eq!(Some(total), suspends());
        }
    }
}
