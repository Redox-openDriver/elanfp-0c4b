use std::time::{Duration, Instant};

use elanfp_core::Transport;
use papilla::{EnrollError, Enrollment, Frame, Gallery, Template, Verdict};

use crate::dynamics::Dynamics;
use crate::session::{Session, Touch, with_quality};
use crate::store::FileStore;
use crate::{BoxError, Explain, Fail, Msg, Policy, Reporter, check_name, library_fail, region_key};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Matched { finger: String, score: usize },
    NotMatched,
    NoTouch,
    Locked,
}

pub trait Guard {
    fn allowed(&mut self) -> bool;

    fn charge(&mut self) -> Result<bool, BoxError>;

    fn matched(&mut self, finger: &str, score: usize);

    fn mismatched(&mut self, best: usize);

    fn skipped(&mut self, finger: &str, error: &BoxError);

    fn unsaved(&mut self, finger: &str, error: &BoxError);

    fn observed(&mut self, _dynamics: &Dynamics) {}
}

pub struct Unguarded;

impl Guard for Unguarded {
    fn allowed(&mut self) -> bool {
        true
    }

    fn charge(&mut self) -> Result<bool, BoxError> {
        Ok(true)
    }

    fn matched(&mut self, _finger: &str, _score: usize) {}

    fn mismatched(&mut self, _best: usize) {}

    fn skipped(&mut self, _finger: &str, _error: &BoxError) {}

    fn unsaved(&mut self, _finger: &str, _error: &BoxError) {}
}

fn template<T: Transport>(policy: &Policy, session: &Session<T>, frame: &[u8]) -> Result<Template, Fail>
where
    T::Error: Explain,
{
    let image = session.image(frame);
    let (width, height) = (session.dim().width(), session.dim().height());
    let frame = Frame::gray8(width, height, &image).map_err(|e| library_fail(e.key(), e.args()))?;
    Template::extract(&policy.profile, &frame).map_err(|e| library_fail(e.key(), e.args()))
}

pub fn enrollable(policy: &Policy, store: &FileStore, uid: u32, name: &str) -> Result<(), BoxError> {
    check_name(name)?;
    if store.exists(uid, name)? {
        return Err(Fail::new("finger-exists").arg("finger", name).into());
    }
    if store.names(uid)?.len() >= policy.max_fingers {
        return Err(Fail::new("too-many-fingers").num("limit", policy.max_fingers).into());
    }
    Ok(())
}

pub fn enroll<T: Transport>(
    policy: &Policy,
    session: &mut Session<T>,
    store: &FileStore,
    uid: u32,
    name: &str,
    report: &mut dyn Reporter,
) -> Result<Msg, BoxError>
where
    T::Error: Explain,
{
    enrollable(policy, store, uid, name)?;
    let rules = policy.profile.enrolling;
    let mut enrollment = Enrollment::new(policy.profile).map_err(|e| Fail::new(e.key()))?;
    let (mut idle, mut streak, mut kept, mut unusable) = (0, 0, 0, 0);
    let begun = Instant::now();
    report.say(Msg::new("enroll-intro").arg("finger", name).num("streak", rules.streak).num("min", rules.min_touches).num("max", rules.max_touches))?;
    while !enrollment.is_complete() {
        if begun.elapsed() >= policy.enroll_budget {
            return Err(Fail::new("enroll-too-long").num("minutes", policy.enroll_budget.as_secs() / 60).into());
        }
        let touches = enrollment.touches();
        report.say(Msg::new("enroll-prompt").num("n", touches + 1).sub("region", &region_key(touches)))?;
        match session.touch(report, policy.enroll_wait)? {
            Touch::Timeout => {
                idle += 1;
                if idle >= policy.enroll_idle_limit {
                    return Err(Fail::new("enroll-abandoned").into());
                }
                report.say(Msg::new("touch-timeout-wait"))?;
                continue;
            }
            Touch::Missed(status) => {
                unusable += 1;
                report.say(Msg::new("touch-missed-retry").arg("status", format!("{status:02x}")))?;
            }
            Touch::Rejected { reason, verdict } => {
                unusable += 1;
                report.say(with_quality(Msg::new("touch-rejected-retry").sub("reason", reason), &verdict))?;
            }
            Touch::Accepted { frame, verdict, .. } => {
                if !report.alive() {
                    return Err(Fail::new("client-gone").into());
                }
                let template = template(policy, session, &frame)?;
                let points = template.len();
                let progress = match enrollment.add(template) {
                    Ok(progress) => progress,
                    Err(e @ EnrollError::Featureless { .. }) => {
                        unusable += 1;
                        report.say(Msg::new(e.key()).args(e.args()))?;
                        if unusable > rules.max_touches {
                            return Err(Fail::new("enroll-unusable").num("count", unusable).into());
                        }
                        session.pause(report);
                        continue;
                    }
                    Err(e) => return Err(library_fail(e.key(), e.args()).into()),
                };
                (streak, kept) = (progress.streak, progress.kept);
                report.say(with_quality(
                    Msg::new("enroll-accepted")
                        .num("points", points)
                        .arg("novelty", if progress.known { "known" } else { "new" })
                        .num("score", progress.best_score)
                        .num("streak", progress.streak.min(rules.streak))
                        .num("need", rules.streak)
                        .num("kept", progress.kept)
                        .num("min", rules.min_touches),
                    &verdict,
                ))?;
                if progress.suspect {
                    report.say(Msg::new("enroll-same-place"))?;
                }
            }
        }
        if unusable > rules.max_touches {
            return Err(Fail::new("enroll-unusable").num("count", unusable).into());
        }
        idle = 0;
        session.pause(report);
    }
    let covered = if streak >= rules.streak && kept >= rules.min_touches {
        Msg::new("enroll-covered").num("streak", streak)
    } else {
        Msg::new("enroll-limit").num("max", rules.max_touches)
    };
    report.say(covered)?;
    let binding = session.identity().tag();
    let started = Instant::now();
    let (gallery, summary) = enrollment.finish().map_err(|e| library_fail(e.key(), e.args()))?;
    report.say(Msg::new("enroll-consistency").arg("seconds", format!("{:.1}", started.elapsed().as_secs_f32())).num("dropped", summary.dropped))?;
    store.save(uid, name, &gallery, &binding)?;
    Ok(Msg::new("enroll-done").arg("finger", name).num("count", summary.kept).arg("algorithm", crate::store::ALGORITHM))
}

#[allow(clippy::too_many_arguments)]
pub fn verify<T: Transport>(
    policy: &Policy,
    session: &mut Session<T>,
    store: &FileStore,
    uid: u32,
    fingers: &[String],
    wait: Duration,
    guard: &mut dyn Guard,
    report: &mut dyn Reporter,
) -> Result<Outcome, BoxError>
where
    T::Error: Explain,
{
    let binding = session.identity().tag();
    let mut galleries: Vec<(&String, Gallery)> = Vec::with_capacity(fingers.len());
    let mut unreadable = None;
    for finger in fingers {
        match store.load(uid, finger, &policy.profile, &binding) {
            Ok(gallery) => galleries.push((finger, gallery)),
            Err(e) => {
                guard.skipped(finger, &e);
                unreadable.get_or_insert(e);
            }
        }
    }
    if galleries.is_empty() {
        return Err(unreadable.unwrap_or_else(|| Fail::new("no-fingers").into()));
    }
    let deadline = Instant::now() + policy.verify_budget;
    let (mut attempts, mut unusable, mut rejected) = (0, 0, false);
    let mut patience = None;
    while attempts < policy.verify_attempts && unusable < policy.unusable_limit {
        if !guard.allowed() {
            return Ok(Outcome::Locked);
        }
        let now = Instant::now();
        let left = deadline.saturating_duration_since(now);
        if left.is_zero() {
            break;
        }
        let until = match patience {
            Some(until) => until,
            None => {
                report.say(Msg::new("verify-prompt").num("attempt", attempts + 1).num("attempts", policy.verify_attempts))?;
                *patience.insert(now + wait.min(left))
            }
        };
        let touch = match until.saturating_duration_since(now) {
            waiting if waiting.is_zero() => Touch::Timeout,
            waiting => session.touch(report, waiting)?,
        };
        if !matches!(touch, Touch::Missed(_)) {
            patience = None;
        }
        match touch {
            Touch::Timeout => {
                report.say(Msg::new("verify-no-touch"))?;
                break;
            }
            Touch::Missed(_) => session.pause(report),
            Touch::Rejected { reason, .. } => {
                unusable += 1;
                report.say(Msg::new("verify-bad-frame").sub("reason", reason))?;
                session.pause(report);
            }
            Touch::Accepted { frame, trace, .. } => {
                attempts += 1;
                if !report.alive() {
                    return Err(Fail::new("client-gone").into());
                }
                if let Some(dynamics) = Dynamics::of(&trace) {
                    guard.observed(&dynamics);
                }
                let probe = template(policy, session, &frame)?;
                galleries.retain(|(finger, _)| store.exists(uid, finger).unwrap_or(false));
                if galleries.is_empty() {
                    return Err(Fail::new("no-fingers").into());
                }
                if !guard.charge()? {
                    return Ok(Outcome::Locked);
                }
                let verdicts: Vec<Verdict> = galleries.iter().map(|(_, g)| g.verify(&probe)).collect();
                let best = verdicts.iter().enumerate().max_by_key(|(i, v)| (v.score(), std::cmp::Reverse(*i))).map(|(i, v)| (i, *v));
                match best {
                    Some((i, verdict)) if verdict.is_match() => {
                        let (finger, gallery) = &mut galleries[i];
                        let (verdict, learned) = gallery.verify_adapting(probe);
                        let score = verdict.score();
                        guard.matched(finger, score);
                        if learned && let Err(e) = store.update(uid, finger, gallery, &binding) {
                            guard.unsaved(finger, &e);
                        }
                        let _ = report.say(Msg::new("verify-match").arg("finger", finger.as_str()));
                        return Ok(Outcome::Matched { finger: finger.to_string(), score });
                    }
                    Some((_, verdict)) => {
                        rejected = true;
                        guard.mismatched(verdict.score());
                        report.say(Msg::new("verify-no-match"))?;
                        session.pause(report);
                    }
                    None => break,
                }
            }
        }
    }
    Ok(if rejected { Outcome::NotMatched } else { Outcome::NoTouch })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FAST, FakeSensor, FakeTouch, PID, Recorder, TempDir, VID, fail_key, finger_frame, policy};
    use crate::store::Layout;
    use elanfp_core::Sensor;

    const UID: u32 = 1000;

    fn store(dir: &TempDir) -> FileStore {
        FileStore::new(dir.0.clone(), Layout::PerUser, papilla::vault::new_key().unwrap()).unwrap()
    }

    fn session(seeds: &[u32]) -> Session<FakeSensor> {
        Session::open_with(Sensor::new(FakeSensor::with_touches(seeds)), VID, PID, FAST, &mut Recorder::default()).unwrap()
    }

    fn enrolled(dir: &TempDir) -> FileStore {
        let store = store(dir);
        enroll(&policy(), &mut session(&[7, 7, 7]), &store, UID, "right-index", &mut Recorder::default()).expect("enroll");
        store
    }

    #[test]
    fn enrollment_stops_by_itself_and_is_saved_encrypted() {
        let dir = TempDir::new("enroll");
        let store = store(&dir);
        let mut report = Recorder::default();
        let done = enroll(&policy(), &mut session(&[7, 7, 7, 7, 7, 7]), &store, UID, "right-index", &mut report).expect("enroll");
        assert_eq!(done.key, "enroll-done");
        assert_eq!(report.count("enroll-prompt"), 3);
        assert_eq!(report.count("enroll-accepted"), 3);
        assert_eq!(store.names(UID).unwrap(), vec!["right-index".to_string()]);
        let file = std::fs::read(dir.0.join("1000/right-index.papv")).unwrap();
        assert!(papilla::vault::is_sealed(&file));
        assert!(!file.windows(4).any(|w| w == b"PAPL"));
    }

    #[test]
    fn progress_shows_both_conditions_and_never_more_than_the_streak_asked_for() {
        let dir = TempDir::new("enroll-progress");
        let store = store(&dir);
        let mut thorough = policy();
        thorough.profile.enrolling = papilla::Enrolling { min_touches: 5, max_touches: 6, streak: 2, ..thorough.profile.enrolling };
        let mut report = Recorder::default();
        enroll(&thorough, &mut session(&[7, 7, 7, 7, 7, 7]), &store, UID, "right-index", &mut report).expect("enroll");
        let shown: Vec<String> = report.messages.iter().filter(|message| message.key == "enroll-accepted").map(|message| message.to_string()).collect();
        let between = |line: &'_ str| line.split_once(" streak=").and_then(|(_, tail)| tail.split_once(" quality=")).map(|(counts, _)| counts.to_string());
        let tails: Vec<String> = shown.iter().filter_map(|line| between(line)).collect();
        assert_eq!(
            tails[..5],
            ["0 need=2 kept=0 min=5", "1 need=2 kept=2 min=5", "2 need=2 kept=3 min=5", "2 need=2 kept=4 min=5", "2 need=2 kept=5 min=5"],
            "{shown:?}"
        );
        assert_eq!(shown.len(), 5);
    }

    #[test]
    fn frame_polling_carries_a_whole_enrollment_and_verification() {
        use crate::fake::FAST_POLLING;
        let polled = |seeds: &[u32]| {
            let mut sensor = FakeSensor::with_touches(seeds);
            sensor.frames_mode = true;
            Session::open_with(Sensor::new(sensor), VID, PID, FAST_POLLING, &mut Recorder::default()).unwrap()
        };
        let dir = TempDir::new("polled");
        let store = store(&dir);
        enroll(&policy(), &mut polled(&[7, 7, 7]), &store, UID, "right-index", &mut Recorder::default()).expect("enroll");
        let fingers = vec!["right-index".to_string()];
        let matched = verify(&policy(), &mut polled(&[7]), &store, UID, &fingers, policy().verify_wait, &mut Unguarded, &mut Recorder::default()).unwrap();
        assert!(matches!(matched, Outcome::Matched { .. }));
        let other = verify(&policy(), &mut polled(&[8, 9]), &store, UID, &fingers, policy().verify_wait, &mut Unguarded, &mut Recorder::default()).unwrap();
        assert_eq!(other, Outcome::NotMatched);
    }

    #[derive(Default)]
    struct Ledger {
        limit: Option<usize>,
        charges: usize,
        unrecordable: bool,
        mismatches: Vec<usize>,
        matches: Vec<(String, usize)>,
        skipped: Vec<String>,
        unsaved: Vec<String>,
        sabotage: Option<std::path::PathBuf>,
    }

    impl Guard for Ledger {
        fn allowed(&mut self) -> bool {
            self.limit.is_none_or(|limit| self.mismatches.len() < limit)
        }

        fn charge(&mut self) -> Result<bool, BoxError> {
            if self.unrecordable {
                return Err(Fail::new("attempt-unrecorded").into());
            }
            assert_eq!(self.charges, self.mismatches.len() + self.matches.len(), "every earlier charge was followed by a result");
            self.charges += 1;
            Ok(true)
        }

        fn matched(&mut self, finger: &str, score: usize) {
            assert_eq!(self.charges, self.mismatches.len() + self.matches.len() + 1, "a match that was not charged");
            self.matches.push((finger.to_string(), score));
            if let Some(path) = &self.sabotage {
                std::fs::remove_file(path).unwrap();
                std::fs::create_dir(path).unwrap();
                std::fs::write(path.join("occupied"), b"x").unwrap();
            }
        }

        fn mismatched(&mut self, best: usize) {
            assert_eq!(self.charges, self.mismatches.len() + self.matches.len() + 1, "a mismatch that was not charged");
            self.mismatches.push(best);
        }

        fn skipped(&mut self, finger: &str, _error: &BoxError) {
            self.skipped.push(finger.to_string());
        }

        fn unsaved(&mut self, finger: &str, _error: &BoxError) {
            self.unsaved.push(finger.to_string());
        }
    }

    struct Quitter {
        leave_on: &'static str,
        seen: Vec<String>,
    }

    impl Reporter for Quitter {
        fn say(&mut self, message: Msg) -> Result<(), BoxError> {
            self.seen.push(message.key.clone());
            if message.key == self.leave_on { Err(Fail::new("client-gone").into()) } else { Ok(()) }
        }
    }

    #[test]
    fn a_failed_comparison_is_recorded_before_the_client_hears_about_it() {
        let dir = TempDir::new("guard-first");
        let store = enrolled(&dir);
        let fingers = vec!["right-index".to_string()];
        let mut ledger = Ledger::default();
        let mut quitter = Quitter { leave_on: "verify-no-match", seen: Vec::new() };
        let gone = verify(&policy(), &mut session(&[8, 9]), &store, UID, &fingers, policy().verify_wait, &mut ledger, &mut quitter).expect_err("client left");
        assert_eq!(fail_key(&gone), "client-gone");
        assert_eq!(ledger.mismatches.len(), 1);
        assert!(ledger.mismatches[0] < policy().profile.accept_score);
    }

    #[test]
    fn a_touch_is_not_compared_once_nobody_waits_for_the_answer() {
        struct Lapsed(bool);

        impl Reporter for Lapsed {
            fn say(&mut self, _message: Msg) -> Result<(), BoxError> {
                Ok(())
            }

            fn alive(&mut self) -> bool {
                !self.0
            }

            fn presence(&mut self, state: crate::Presence) {
                self.0 |= state == crate::Presence::Away;
            }
        }

        let dir = TempDir::new("verify-unwanted");
        let store = enrolled(&dir);
        let fingers = vec!["right-index".to_string()];
        let mut ledger = Ledger::default();
        let gone =
            verify(&policy(), &mut session(&[7]), &store, UID, &fingers, policy().verify_wait, &mut ledger, &mut Lapsed(false)).expect_err("nobody waits");
        assert_eq!(fail_key(&gone), "client-gone");
        assert_eq!((ledger.charges, ledger.matches.len(), ledger.mismatches.len()), (0, 0, 0));
    }

    #[test]
    fn nothing_is_compared_unless_the_attempt_could_be_recorded() {
        let dir = TempDir::new("guard-charge");
        let store = enrolled(&dir);
        let fingers = vec!["right-index".to_string()];
        let mut ledger = Ledger { unrecordable: true, ..Ledger::default() };
        let mut report = Recorder::default();
        let refused = verify(&policy(), &mut session(&[7]), &store, UID, &fingers, policy().verify_wait, &mut ledger, &mut report).expect_err("refused");
        assert_eq!(fail_key(&refused), "attempt-unrecorded");
        assert_eq!((ledger.matches.len(), ledger.mismatches.len(), report.count("verify-match")), (0, 0, 0));
        let mut ledger = Ledger::default();
        verify(&policy(), &mut session(&[8, 7]), &store, UID, &fingers, policy().verify_wait, &mut ledger, &mut Recorder::default()).unwrap();
        assert_eq!((ledger.charges, ledger.mismatches.len(), ledger.matches.len()), (2, 1, 1));
    }

    struct Deleter<'a> {
        store: &'a FileStore,
        finger: &'static str,
    }

    impl Reporter for Deleter<'_> {
        fn say(&mut self, message: Msg) -> Result<(), BoxError> {
            if message.key == "verify-prompt" && self.store.exists(UID, self.finger)? {
                self.store.delete(UID, self.finger)?;
            }
            Ok(())
        }
    }

    #[test]
    fn a_finger_deleted_while_the_request_waits_no_longer_opens_anything() {
        let dir = TempDir::new("deleted-meanwhile");
        let store = enrolled(&dir);
        enroll(&policy(), &mut session(&[11, 11, 11]), &store, UID, "left-index", &mut Recorder::default()).expect("second finger");
        let fingers = store.names(UID).unwrap();
        let mut ledger = Ledger::default();
        let mut deleter = Deleter { store: &store, finger: "right-index" };
        let outcome = verify(&policy(), &mut session(&[7, 11]), &store, UID, &fingers, policy().verify_wait, &mut ledger, &mut deleter).unwrap();
        assert!(matches!(outcome, Outcome::Matched { ref finger, .. } if finger == "left-index"), "{outcome:?}");
        assert_eq!((ledger.mismatches.len(), ledger.matches.len()), (1, 1));
        let mut ledger = Ledger::default();
        let mut deleter = Deleter { store: &store, finger: "left-index" };
        let nothing_left = verify(&policy(), &mut session(&[11]), &store, UID, &["left-index".to_string()], policy().verify_wait, &mut ledger, &mut deleter);
        assert_eq!(fail_key(&nothing_left.expect_err("no fingers left")), "no-fingers");
        assert_eq!(ledger.charges, 0);
    }

    #[test]
    fn a_sensor_that_keeps_answering_without_a_finger_does_not_cut_the_wait_short() {
        let dir = TempDir::new("missed");
        let store = enrolled(&dir);
        let fingers = vec!["right-index".to_string()];
        let mut fake = FakeSensor::new();
        for _ in 0..policy().unusable_limit * 3 {
            fake.push(FakeTouch { delay_polls: 0, status: 0xca, frame: Vec::new() });
        }
        fake.press(finger_frame(7));
        let mut report = Recorder::default();
        let mut session = Session::open_with(Sensor::new(fake), VID, PID, FAST, &mut Recorder::default()).unwrap();
        let wait = std::time::Duration::from_secs(5);
        let outcome = verify(&policy(), &mut session, &store, UID, &fingers, wait, &mut Unguarded, &mut report).unwrap();
        assert!(matches!(outcome, Outcome::Matched { .. }), "{outcome:?}");
        assert_eq!(report.count("verify-prompt"), 1);
        let mut only_misses = FakeSensor::new();
        for _ in 0..200 {
            only_misses.push(FakeTouch { delay_polls: 0, status: 0xca, frame: Vec::new() });
        }
        let mut report = Recorder::default();
        let mut session = Session::open_with(Sensor::new(only_misses), VID, PID, FAST, &mut Recorder::default()).unwrap();
        let started = Instant::now();
        let outcome = verify(&policy(), &mut session, &store, UID, &fingers, policy().verify_wait, &mut Unguarded, &mut report).unwrap();
        assert_eq!((outcome, report.count("verify-prompt"), report.count("verify-no-touch")), (Outcome::NoTouch, 1, 1));
        assert!(started.elapsed() >= policy().verify_wait, "{:?}", started.elapsed());
    }

    #[test]
    fn enrollment_cannot_hold_the_reader_forever() {
        let dir = TempDir::new("enroll-budget");
        let store = store(&dir);
        let mut hasty = policy();
        hasty.enroll_budget = std::time::Duration::ZERO;
        let stopped = enroll(&hasty, &mut session(&[7, 7, 7]), &store, UID, "right-index", &mut Recorder::default()).expect_err("out of time");
        assert_eq!(fail_key(&stopped), "enroll-too-long");
        assert!(store.names(UID).unwrap().is_empty());
    }

    #[test]
    fn the_guard_can_stop_further_comparisons_in_the_same_request() {
        let dir = TempDir::new("guard-stop");
        let store = enrolled(&dir);
        let fingers = vec!["right-index".to_string()];
        let mut ledger = Ledger { limit: Some(1), ..Ledger::default() };
        let outcome = verify(&policy(), &mut session(&[8, 7]), &store, UID, &fingers, policy().verify_wait, &mut ledger, &mut Recorder::default()).unwrap();
        assert_eq!(outcome, Outcome::Locked);
        assert!(ledger.matches.is_empty());
        let mut ledger = Ledger { limit: Some(0), ..Ledger::default() };
        let mut report = Recorder::default();
        let outcome = verify(&policy(), &mut session(&[7]), &store, UID, &fingers, policy().verify_wait, &mut ledger, &mut report).unwrap();
        assert_eq!((outcome, report.count("verify-prompt")), (Outcome::Locked, 0));
    }

    #[test]
    fn clients_never_see_scores_or_thresholds() {
        let dir = TempDir::new("no-scores");
        let store = enrolled(&dir);
        let fingers = vec!["right-index".to_string()];
        let mut report = Recorder::default();
        verify(&policy(), &mut session(&[8, 7]), &store, UID, &fingers, policy().verify_wait, &mut Unguarded, &mut report).unwrap();
        for message in report.messages.iter().filter(|m| m.key.starts_with("verify-")) {
            assert!(message.args.iter().all(|(name, _)| !matches!(name.as_str(), "score" | "threshold" | "ms")), "{message:?}");
        }
        assert_eq!((report.count("verify-no-match"), report.count("verify-match")), (1, 1));
    }

    #[test]
    fn an_unreadable_finger_does_not_disable_the_others() {
        let dir = TempDir::new("skip-bad");
        let store = enrolled(&dir);
        enroll(&policy(), &mut session(&[11, 11, 11]), &store, UID, "left-index", &mut Recorder::default()).expect("second finger");
        std::fs::write(dir.0.join("1000/left-index.papv"), b"damaged").unwrap();
        let fingers = store.names(UID).unwrap();
        let mut ledger = Ledger::default();
        let outcome = verify(&policy(), &mut session(&[7]), &store, UID, &fingers, policy().verify_wait, &mut ledger, &mut Recorder::default()).unwrap();
        assert!(matches!(outcome, Outcome::Matched { ref finger, .. } if finger == "right-index"));
        assert_eq!(ledger.skipped, vec!["left-index".to_string()]);
        std::fs::write(dir.0.join("1000/right-index.papv"), b"damaged").unwrap();
        let none = verify(&policy(), &mut session(&[7]), &store, UID, &fingers, policy().verify_wait, &mut Unguarded, &mut Recorder::default());
        assert_eq!(fail_key(&none.expect_err("nothing readable")), "template-unreadable");
    }

    #[test]
    fn a_match_survives_a_failed_adaptation_save() {
        let mut learning = policy();
        learning.profile.enrolling.redundant_score = usize::MAX;
        let fingers = vec!["right-index".to_string()];
        let dir = TempDir::new("save-works");
        let store = enrolled(&dir);
        let path = dir.0.join("1000/right-index.papv");
        let before = std::fs::read(&path).unwrap();
        let kept = verify(&learning, &mut session(&[7]), &store, UID, &fingers, learning.verify_wait, &mut Unguarded, &mut Recorder::default());
        assert!(matches!(kept, Ok(Outcome::Matched { .. })) && std::fs::read(&path).unwrap() != before, "this touch must be one that adaptation keeps");
        let dir = TempDir::new("save-fails");
        let store = enrolled(&dir);
        let path = dir.0.join("1000/right-index.papv");
        let mut ledger = Ledger { sabotage: Some(path.clone()), ..Ledger::default() };
        let outcome = verify(&learning, &mut session(&[7]), &store, UID, &fingers, learning.verify_wait, &mut ledger, &mut Recorder::default()).unwrap();
        assert!(matches!(outcome, Outcome::Matched { .. }));
        assert_eq!(ledger.unsaved, vec!["right-index".to_string()]);
        std::fs::remove_dir_all(&path).unwrap();
    }

    #[test]
    fn an_update_never_resurrects_a_deleted_finger() {
        let dir = TempDir::new("no-resurrection");
        let store = enrolled(&dir);
        let gallery = store.load(UID, "right-index", &policy().profile, &session(&[]).identity().tag()).unwrap();
        store.delete(UID, "right-index").unwrap();
        assert!(!store.update(UID, "right-index", &gallery, &session(&[]).identity().tag()).unwrap());
        assert!(store.names(UID).unwrap().is_empty());
    }

    #[test]
    fn a_request_cannot_outlive_its_budget() {
        let dir = TempDir::new("budget");
        let store = enrolled(&dir);
        let fingers = vec!["right-index".to_string()];
        let mut tight = policy();
        tight.verify_budget = Duration::ZERO;
        let mut report = Recorder::default();
        let outcome = verify(&tight, &mut session(&[7]), &store, UID, &fingers, tight.verify_wait, &mut Unguarded, &mut report).unwrap();
        assert_eq!((outcome, report.count("verify-prompt")), (Outcome::NoTouch, 0));
    }

    #[test]
    fn enrollment_gives_up_after_too_many_unusable_touches() {
        use crate::fake::partial_frame;
        let dir = TempDir::new("unusable-enroll");
        let store = store(&dir);
        let mut sensor = FakeSensor::new();
        for seed in 0..(policy().profile.enrolling.max_touches as u32 + 2) {
            sensor.press(partial_frame(seed));
        }
        let mut session = Session::open_with(Sensor::new(sensor), VID, PID, FAST, &mut Recorder::default()).unwrap();
        let failed = enroll(&policy(), &mut session, &store, UID, "right-index", &mut Recorder::default()).expect_err("must give up");
        assert_eq!(fail_key(&failed), "enroll-unusable");
        assert!(store.names(UID).unwrap().is_empty());
    }

    #[test]
    fn enrolled_finger_verifies_and_another_finger_does_not() {
        let dir = TempDir::new("verify");
        let store = enrolled(&dir);
        let fingers = vec!["right-index".to_string()];
        let placements: Vec<Vec<u8>> = (0..=8).map(|step| crate::fake::placed_frame(7, step)).collect();
        assert!(placements.iter().enumerate().all(|(at, frame)| !placements[..at].contains(frame)));
        let mut report = Recorder::default();
        let outcome = verify(&policy(), &mut session(&[7]), &store, UID, &fingers, policy().verify_wait, &mut Unguarded, &mut report).unwrap();
        assert!(matches!(outcome, Outcome::Matched { ref finger, score } if finger == "right-index" && score >= policy().profile.accept_score));
        assert_eq!(report.count("verify-match"), 1);
        let mut report = Recorder::default();
        let outcome = verify(&policy(), &mut session(&[8, 9, 10]), &store, UID, &fingers, policy().verify_wait, &mut Unguarded, &mut report).unwrap();
        assert_eq!(outcome, Outcome::NotMatched);
        assert_eq!(report.count("verify-no-match"), policy().verify_attempts);
    }

    #[test]
    fn second_attempt_can_succeed_after_a_wrong_touch() {
        let dir = TempDir::new("second");
        let store = enrolled(&dir);
        let fingers = vec!["right-index".to_string()];
        let outcome = verify(&policy(), &mut session(&[8, 7]), &store, UID, &fingers, policy().verify_wait, &mut Unguarded, &mut Recorder::default()).unwrap();
        assert!(matches!(outcome, Outcome::Matched { .. }));
    }

    #[test]
    fn no_touch_is_not_counted_as_a_failed_match() {
        let dir = TempDir::new("notouch");
        let store = enrolled(&dir);
        let fingers = vec!["right-index".to_string()];
        let outcome = verify(&policy(), &mut session(&[]), &store, UID, &fingers, policy().verify_wait, &mut Unguarded, &mut Recorder::default()).unwrap();
        assert_eq!(outcome, Outcome::NoTouch);
    }

    #[test]
    fn unusable_touches_are_bounded_and_never_compared() {
        let dir = TempDir::new("unusable");
        let store = enrolled(&dir);
        let fingers = vec!["right-index".to_string()];
        let mut fake = FakeSensor::new();
        for _ in 0..10 {
            fake.press(crate::fake::partial_frame(7));
        }
        let mut s = Session::open_with(Sensor::new(fake), VID, PID, FAST, &mut Recorder::default()).unwrap();
        let mut report = Recorder::default();
        let outcome = verify(&policy(), &mut s, &store, UID, &fingers, policy().verify_wait, &mut Unguarded, &mut report).unwrap();
        assert_eq!(outcome, Outcome::NoTouch);
        assert_eq!(report.count("verify-bad-frame"), policy().unusable_limit);
        assert_eq!(report.count("verify-no-match"), 0);
    }

    #[test]
    fn any_enrolled_finger_is_accepted_and_named() {
        let dir = TempDir::new("multi");
        let store = enrolled(&dir);
        enroll(&policy(), &mut session(&[21, 21, 21]), &store, UID, "left-thumb", &mut Recorder::default()).unwrap();
        let fingers = store.names(UID).unwrap();
        let outcome = verify(&policy(), &mut session(&[21]), &store, UID, &fingers, policy().verify_wait, &mut Unguarded, &mut Recorder::default()).unwrap();
        assert!(matches!(outcome, Outcome::Matched { ref finger, .. } if finger == "left-thumb"));
    }

    #[test]
    fn different_fingers_cannot_be_enrolled_as_one() {
        let dir = TempDir::new("mixed");
        let store = store(&dir);
        let error = enroll(&policy(), &mut session(&[1, 2, 3, 4, 5, 6]), &store, UID, "right-index", &mut Recorder::default()).err().unwrap();
        assert_eq!(fail_key(&error), "papilla-enroll-inconsistent");
        assert!(store.names(UID).unwrap().is_empty());
    }

    #[test]
    fn a_session_ended_by_the_touch_limit_does_not_say_the_finger_is_covered() {
        let dir = TempDir::new("limit");
        let store = store(&dir);
        let mut strict = policy();
        strict.profile.enrolling = papilla::Enrolling { min_touches: 5, max_touches: 6, streak: 2, ..strict.profile.enrolling };
        let mut report = Recorder::default();
        let mut mixed = FakeSensor::new();
        for (seed, step) in [(1, 0), (2, 0), (3, 0), (7, 1), (7, 2), (7, 3)] {
            mixed.press(crate::fake::placed_frame(seed, step));
        }
        let mut session = Session::open_with(Sensor::new(mixed), VID, PID, FAST, &mut Recorder::default()).unwrap();
        let error = enroll(&strict, &mut session, &store, UID, "right-index", &mut report).err().unwrap();
        assert_eq!(fail_key(&error), "papilla-enroll-inconsistent");
        assert_eq!((report.count("enroll-accepted"), report.count("enroll-limit"), report.count("enroll-covered")), (6, 1, 0));
        assert!(!store.has_templates());
    }

    #[test]
    fn a_picture_that_never_moves_is_not_enrolled_as_a_finger() {
        let dir = TempDir::new("fixed");
        let store = store(&dir);
        let mut long = policy();
        long.profile.enrolling = papilla::Enrolling { min_touches: 16, max_touches: 16, streak: 16, ..long.profile.enrolling };
        let mut report = Recorder::default();
        let mut stuck = FakeSensor::new();
        for _ in 0..16 {
            stuck.press(finger_frame(7));
        }
        let mut session = Session::open_with(Sensor::new(stuck), VID, PID, FAST, &mut Recorder::default()).unwrap();
        let error = enroll(&long, &mut session, &store, UID, "right-index", &mut report).err().unwrap();
        assert_eq!(fail_key(&error), "papilla-enroll-fixed");
        assert_eq!((report.count("enroll-accepted"), report.count("enroll-same-place")), (13, 2));
        assert!(!store.has_templates());
    }

    #[test]
    fn enrollment_guards_names_duplicates_limits_and_idle_users() {
        let dir = TempDir::new("guards");
        let store = enrolled(&dir);
        let p = policy();
        let key = |r: Result<Msg, BoxError>| fail_key(&r.expect_err("must fail"));
        assert_eq!(key(enroll(&p, &mut session(&[7]), &store, UID, "../evil", &mut Recorder::default())), "finger-name-invalid");
        assert_eq!(key(enroll(&p, &mut session(&[7]), &store, UID, "right-index", &mut Recorder::default())), "finger-exists");
        assert_eq!(key(enroll(&p, &mut session(&[]), &store, UID, "idle", &mut Recorder::default())), "enroll-abandoned");
        enroll(&p, &mut session(&[5, 5, 5]), &store, UID, "second", &mut Recorder::default()).unwrap();
        assert_eq!(key(enroll(&p, &mut session(&[6, 6, 6]), &store, UID, "third", &mut Recorder::default())), "too-many-fingers");
    }

    #[test]
    fn interrupted_enrollment_saves_nothing() {
        let dir = TempDir::new("interrupted");
        let store = store(&dir);
        let mut report = Recorder { hang_up_after: Some(3), ..Recorder::default() };
        let mut fake = FakeSensor::with_touches(&[7]);
        fake.push(crate::fake::FakeTouch { delay_polls: 1000, status: elanfp_core::FINGER_PRESENT, frame: finger_frame(7) });
        let mut s = Session::open_with(Sensor::new(fake), VID, PID, FAST, &mut Recorder::default()).unwrap();
        let mut slow = policy();
        slow.enroll_wait = Duration::from_secs(30);
        let error = enroll(&slow, &mut s, &store, UID, "right-index", &mut report).err().unwrap();
        assert_eq!(fail_key(&error), "client-gone");
        assert!(!store.has_templates());
    }

    #[test]
    fn confident_touches_extend_the_gallery_on_disk() {
        let dir = TempDir::new("adapt");
        let store = enrolled(&dir);
        let mut learning = policy();
        learning.profile.enrolling.redundant_score = usize::MAX;
        let fingers = vec!["right-index".to_string()];
        let binding = elanfp_core::identity::VERIFIED[0].tag();
        let before = store.load(UID, "right-index", &learning.profile, &binding).unwrap();
        verify(&learning, &mut session(&[7]), &store, UID, &fingers, learning.verify_wait, &mut Unguarded, &mut Recorder::default()).unwrap();
        let after = store.load(UID, "right-index", &learning.profile, &binding).unwrap();
        assert_eq!((before.adapted(), after.adapted(), after.enrolled()), (0, 1, before.enrolled()));
    }
}
