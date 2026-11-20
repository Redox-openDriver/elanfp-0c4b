use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::BoxError;
use crate::store::FileStore;

pub const MAX_FAILURES: u32 = 5;
pub const LOCKOUT: Duration = Duration::from_secs(600);
pub const LOCKOUT_LIMIT: Duration = Duration::from_secs(24 * 3600);
const NOTE: &str = "lockout";
const MAX_DOUBLINGS: u32 = 16;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Tally {
    failures: u32,
    strikes: u32,
    until: Option<Instant>,
}

#[derive(Debug)]
pub enum Charge {
    Recorded,
    Locked(Duration),
    Unrecorded(BoxError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failed {
    pub count: u32,
    pub locked: Option<Duration>,
}

pub struct Lockouts {
    started: Instant,
    tallies: Mutex<HashMap<u32, Tally>>,
}

impl Default for Lockouts {
    fn default() -> Self {
        Lockouts::since(Instant::now())
    }
}

pub fn span(strikes: u32) -> Duration {
    let doublings = strikes.saturating_sub(1).min(MAX_DOUBLINGS);
    LOCKOUT.saturating_mul(1 << doublings).min(LOCKOUT_LIMIT)
}

fn parse(data: &[u8]) -> Option<(u32, u32)> {
    let mut fields = std::str::from_utf8(data).ok()?.split_ascii_whitespace();
    let failures = fields.next()?.parse().ok()?;
    let strikes = fields.next()?.parse().ok()?;
    fields.next().is_none().then_some((failures, strikes))
}

fn encode(tally: &Tally) -> String {
    format!("{} {}\n", tally.failures, tally.strikes)
}

fn locked(strikes: u32, from: Instant) -> Tally {
    let strikes = strikes.max(1);
    Tally { failures: MAX_FAILURES, strikes, until: Some(from + span(strikes)) }
}

fn save(store: &FileStore, uid: u32, tally: &Tally) -> Result<(), BoxError> {
    if tally.failures == 0 && tally.strikes == 0 { store.clear_note(uid, NOTE) } else { store.write_note(uid, NOTE, encode(tally).as_bytes()) }
}

fn recall(known: &mut HashMap<u32, Tally>, store: &FileStore, uid: u32, started: Instant) -> Tally {
    if let Some(tally) = known.get(&uid) {
        return *tally;
    }
    let tally = match store.read_note(uid, NOTE).map(|found| found.map(|data| parse(&data))) {
        Ok(None) => return Tally::default(),
        Ok(Some(Some((failures, strikes)))) if failures < MAX_FAILURES => Tally { failures, strikes, until: None },
        Ok(Some(Some((_, strikes)))) => locked(strikes, started),
        Ok(Some(None)) | Err(_) => locked(1, started),
    };
    if tally.until.is_some() {
        let _ = save(store, uid, &tally);
    }
    known.insert(uid, tally);
    tally
}

impl Lockouts {
    pub fn since(started: Instant) -> Self {
        Lockouts { started, tallies: Mutex::default() }
    }

    fn tallies(&self) -> MutexGuard<'_, HashMap<u32, Tally>> {
        self.tallies.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn left(&self, known: &mut HashMap<u32, Tally>, store: &FileStore, uid: u32, now: Instant) -> Option<Duration> {
        let mut tally = recall(known, store, uid, self.started);
        let until = tally.until?;
        if until > now {
            return Some(until - now);
        }
        tally.failures = 0;
        tally.until = None;
        known.insert(uid, tally);
        let _ = save(store, uid, &tally);
        None
    }

    pub fn remaining(&self, store: &FileStore, uid: u32, now: Instant) -> Option<Duration> {
        self.left(&mut self.tallies(), store, uid, now)
    }

    pub fn charge(&self, store: &FileStore, uid: u32, now: Instant) -> Charge {
        let mut known = self.tallies();
        if let Some(left) = self.left(&mut known, store, uid, now) {
            return Charge::Locked(left);
        }
        let mut tally = recall(&mut known, store, uid, self.started);
        if tally.failures >= MAX_FAILURES {
            let tally = locked(tally.strikes, now);
            known.insert(uid, tally);
            let _ = save(store, uid, &tally);
            return Charge::Locked(span(tally.strikes));
        }
        tally.failures += 1;
        if tally.failures == MAX_FAILURES {
            tally.strikes = tally.strikes.saturating_add(1);
        }
        match save(store, uid, &tally) {
            Ok(()) => {
                known.insert(uid, tally);
                Charge::Recorded
            }
            Err(e) => Charge::Unrecorded(e),
        }
    }

    pub fn failed(&self, store: &FileStore, uid: u32, now: Instant) -> Failed {
        let mut known = self.tallies();
        let mut tally = recall(&mut known, store, uid, self.started);
        if tally.failures < MAX_FAILURES || tally.until.is_some() {
            return Failed { count: tally.failures, locked: None };
        }
        let lasting = span(tally.strikes);
        tally.until = Some(now + lasting);
        known.insert(uid, tally);
        Failed { count: tally.failures, locked: Some(lasting) }
    }

    pub fn clear(&self, store: &FileStore, uid: u32) -> Result<(), BoxError> {
        let mut known = self.tallies();
        known.remove(&uid);
        store.clear_note(uid, NOTE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::TempDir;
    use crate::store::Layout;

    const UID: u32 = 1000;
    const MINUTE: Duration = Duration::from_secs(60);

    fn store(dir: &TempDir) -> FileStore {
        FileStore::new(dir.0.clone(), Layout::PerUser, papilla::vault::new_key().unwrap()).unwrap()
    }

    fn fail(lockouts: &Lockouts, store: &FileStore, now: Instant) -> Failed {
        assert!(matches!(lockouts.charge(store, UID, now), Charge::Recorded));
        lockouts.failed(store, UID, now)
    }

    fn fail_until_locked(lockouts: &Lockouts, store: &FileStore, now: Instant) -> Duration {
        for count in 1..MAX_FAILURES {
            assert_eq!(fail(lockouts, store, now), Failed { count, locked: None });
        }
        fail(lockouts, store, now).locked.expect("locked after the last allowed failure")
    }

    #[test]
    fn the_fifth_failure_locks_and_every_further_lockout_lasts_twice_as_long() {
        let dir = TempDir::new("lockout-doubling");
        let mut now = Instant::now();
        let (store, lockouts) = (store(&dir), Lockouts::since(now));
        let mut expected = LOCKOUT;
        for _ in 0..12 {
            assert_eq!(fail_until_locked(&lockouts, &store, now), expected);
            assert_eq!(lockouts.remaining(&store, UID, now), Some(expected));
            assert!(matches!(lockouts.charge(&store, UID, now + expected - MINUTE), Charge::Locked(left) if left == MINUTE));
            now += expected;
            assert_eq!(lockouts.remaining(&store, UID, now), None);
            expected = (expected * 2).min(LOCKOUT_LIMIT);
        }
        assert_eq!(expected, LOCKOUT_LIMIT);
        assert_eq!((span(0), span(1), span(2), span(u32::MAX)), (LOCKOUT, LOCKOUT, LOCKOUT * 2, LOCKOUT_LIMIT));
    }

    #[test]
    fn a_match_or_an_administrator_forgets_failures_and_strikes() {
        let dir = TempDir::new("lockout-clear");
        let now = Instant::now();
        let (store, lockouts) = (store(&dir), Lockouts::since(now));
        assert_eq!(fail(&lockouts, &store, now).count, 1);
        assert_eq!(fail(&lockouts, &store, now).count, 2);
        lockouts.clear(&store, UID).unwrap();
        assert_eq!(store.read_note(UID, NOTE).unwrap(), None);
        assert_eq!(fail_until_locked(&lockouts, &store, now), LOCKOUT);
        lockouts.clear(&store, UID).unwrap();
        assert_eq!(lockouts.remaining(&store, UID, now), None);
        let fresh = Lockouts::since(now);
        assert_eq!(fresh.remaining(&store, UID, now), None);
        assert_eq!(fail_until_locked(&fresh, &store, now), LOCKOUT);
    }

    #[test]
    fn an_attempt_is_on_disk_before_it_is_compared() {
        let dir = TempDir::new("lockout-charge");
        let now = Instant::now();
        let (store, lockouts) = (store(&dir), Lockouts::since(now));
        assert!(matches!(lockouts.charge(&store, UID, now), Charge::Recorded));
        assert_eq!(store.read_note(UID, NOTE).unwrap().as_deref(), Some(&b"1 0\n"[..]));
        let restarted = Lockouts::since(now);
        for count in 2..=MAX_FAILURES {
            assert!(matches!(restarted.charge(&store, UID, now), Charge::Recorded));
            let level = u32::from(count == MAX_FAILURES);
            assert_eq!(store.read_note(UID, NOTE).unwrap(), Some(format!("{count} {level}\n").into_bytes()));
        }
        assert!(matches!(restarted.charge(&store, UID, now), Charge::Locked(left) if left == LOCKOUT));
        let crashed_before_the_answer = Lockouts::since(now);
        assert_eq!(crashed_before_the_answer.remaining(&store, UID, now), Some(LOCKOUT));
    }

    #[test]
    fn a_crash_between_the_last_attempt_and_its_verdict_still_raises_the_level() {
        let dir = TempDir::new("lockout-crash");
        let (store, mut now) = (store(&dir), Instant::now());
        let mut expected = LOCKOUT;
        for _ in 0..4 {
            let service = Lockouts::since(now);
            for _ in 0..MAX_FAILURES {
                assert!(matches!(service.charge(&store, UID, now), Charge::Recorded));
            }
            let restarted = Lockouts::since(now);
            assert_eq!(restarted.remaining(&store, UID, now), Some(expected));
            now += expected;
            assert_eq!(restarted.remaining(&store, UID, now), None);
            expected *= 2;
        }
    }

    #[test]
    fn clearing_cannot_be_undone_by_a_request_that_looks_at_the_same_moment() {
        let dir = TempDir::new("lockout-race");
        let now = Instant::now();
        let (store, lockouts) = (store(&dir), Lockouts::since(now));
        struct Stop<'a>(&'a std::sync::atomic::AtomicBool);

        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }

        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    lockouts.remaining(&store, UID, now);
                    std::thread::yield_now();
                }
            });
            let _stop = Stop(&done);
            for _ in 0..200 {
                store.write_note(UID, NOTE, b"4 0\n").unwrap();
                lockouts.clear(&store, UID).unwrap();
                assert!(matches!(lockouts.charge(&store, UID, now), Charge::Recorded));
                assert_eq!(lockouts.failed(&store, UID, now), Failed { count: 1, locked: None });
            }
        });
    }

    #[test]
    fn an_attempt_that_cannot_be_recorded_is_refused() {
        let dir = TempDir::new("lockout-unwritable");
        let (store, lockouts, now) = (store(&dir), Lockouts::default(), Instant::now());
        let blocker = dir.0.join(UID.to_string()).join("lockout.tmp");
        std::fs::create_dir_all(&blocker).unwrap();
        assert!(matches!(lockouts.charge(&store, UID, now), Charge::Unrecorded(_)));
        assert!(matches!(lockouts.charge(&store, UID, now), Charge::Unrecorded(_)));
        assert_eq!(lockouts.failed(&store, UID, now), Failed { count: 0, locked: None });
        std::fs::remove_dir(&blocker).unwrap();
        assert!(matches!(lockouts.charge(&store, UID, now), Charge::Recorded));
        assert_eq!(lockouts.failed(&store, UID, now), Failed { count: 1, locked: None });
    }

    #[test]
    fn a_restart_starts_a_stored_lockout_again_and_keeps_its_level() {
        let dir = TempDir::new("lockout-restart");
        let (store, now) = (store(&dir), Instant::now());
        let first = Lockouts::since(now);
        fail_until_locked(&first, &store, now);
        let later = now + LOCKOUT;
        assert_eq!(first.remaining(&store, UID, later), None);
        assert_eq!(fail_until_locked(&first, &store, later), LOCKOUT * 2);
        let much_later = later + LOCKOUT * 2 - MINUTE;
        let restarted = Lockouts::since(much_later);
        assert_eq!(restarted.remaining(&store, UID, much_later), Some(LOCKOUT * 2));
        assert_eq!(restarted.remaining(&store, UID, much_later + LOCKOUT * 2), None);
        assert_eq!(fail_until_locked(&restarted, &store, much_later + LOCKOUT * 2), LOCKOUT * 4);
    }

    #[test]
    fn a_lockout_that_ran_out_while_nobody_asked_is_not_served_again() {
        let dir = TempDir::new("lockout-stale");
        let (store, evening) = (store(&dir), Instant::now());
        fail_until_locked(&Lockouts::since(evening), &store, evening);
        let morning = evening + Duration::from_secs(12 * 3600);
        let rebooted = Lockouts::since(morning);
        assert_eq!(rebooted.remaining(&store, UID, morning + MINUTE), Some(LOCKOUT - MINUTE));
        let next_day = Lockouts::since(morning);
        assert_eq!(next_day.remaining(&store, UID, morning + LOCKOUT), None);
        assert_eq!(fail_until_locked(&next_day, &store, morning + LOCKOUT), LOCKOUT * 2);
    }

    #[test]
    fn a_damaged_record_locks_once_and_then_expires() {
        for damage in [&b"garbage"[..], b"", b"5", b"1 2 3", b"-1 0", b"5 0 \xff"] {
            let dir = TempDir::new("lockout-damaged");
            let now = Instant::now();
            let (store, lockouts) = (store(&dir), Lockouts::since(now));
            store.write_note(UID, NOTE, damage).unwrap();
            assert_eq!(lockouts.remaining(&store, UID, now), Some(LOCKOUT), "{damage:?}");
            assert_eq!(store.read_note(UID, NOTE).unwrap().as_deref(), Some(&b"5 1\n"[..]));
            assert_eq!(lockouts.remaining(&store, UID, now + LOCKOUT - MINUTE), Some(MINUTE));
            assert_eq!(lockouts.remaining(&store, UID, now + LOCKOUT), None);
            assert_eq!(store.read_note(UID, NOTE).unwrap().as_deref(), Some(&b"0 1\n"[..]));
            assert!(matches!(lockouts.charge(&store, UID, now + LOCKOUT), Charge::Recorded));
        }
    }

    #[test]
    fn an_untouched_account_leaves_no_trace() {
        let dir = TempDir::new("lockout-untouched");
        let (store, lockouts, now) = (store(&dir), Lockouts::default(), Instant::now());
        for uid in 0..2000 {
            assert_eq!(lockouts.remaining(&store, uid, now), None);
        }
        assert!(lockouts.tallies().is_empty());
        assert!(!dir.0.join("1000").exists());
    }
}
