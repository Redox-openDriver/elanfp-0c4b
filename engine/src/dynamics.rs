use std::time::Duration;

use elanfp_core::quality::Verdict;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub at: Duration,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dynamics {
    pub frames: usize,
    pub settle_ms: u64,
    pub held_ms: u64,
    pub coverage_rise: f32,
    pub contrast_drift: f32,
    pub restlessness: f32,
}

impl Dynamics {
    pub fn of(trace: &[Sample]) -> Option<Dynamics> {
        let (first, last) = (trace.first()?, trace.last()?);
        let settled = trace.iter().find(|sample| sample.verdict.accepted())?;
        let usable: Vec<&Sample> = trace.iter().filter(|sample| sample.verdict.accepted()).collect();
        let restlessness = usable.iter().map(|sample| sample.verdict.motion).sum::<f32>() / usable.len() as f32;
        Some(Dynamics {
            frames: trace.len(),
            settle_ms: settled.at.saturating_sub(first.at).as_millis() as u64,
            held_ms: last.at.saturating_sub(first.at).as_millis() as u64,
            coverage_rise: last.verdict.coverage - first.verdict.coverage,
            contrast_drift: (last.verdict.contrast - settled.verdict.contrast) as f32 / settled.verdict.contrast.max(1) as f32,
            restlessness,
        })
    }

    pub fn record(&self) -> String {
        format!("{}\t{}\t{}\t{:.4}\t{:.4}\t{:.4}", self.frames, self.settle_ms, self.held_ms, self.coverage_rise, self.contrast_drift, self.restlessness)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ms: u64, coverage: f32, contrast: i64, motion: f32) -> Sample {
        Sample { at: Duration::from_millis(ms), verdict: Verdict { coverage, contrast, motion } }
    }

    #[test]
    fn dynamics_describe_how_a_touch_settled() {
        let trace =
            [sample(0, 0.30, 100, 1.0), sample(40, 0.70, 180, 0.40), sample(80, 0.95, 400, 0.04), sample(120, 0.97, 440, 0.02), sample(160, 0.98, 480, 0.00)];
        let dynamics = Dynamics::of(&trace).expect("a settled touch");
        assert_eq!((dynamics.frames, dynamics.settle_ms, dynamics.held_ms), (5, 80, 160));
        assert!((dynamics.coverage_rise - 0.68).abs() < 1e-6);
        assert!((dynamics.contrast_drift - 0.2).abs() < 1e-6);
        assert!((dynamics.restlessness - 0.02).abs() < 1e-6);
        assert_eq!(dynamics.record().split('\t').count(), 6);
    }

    #[test]
    fn a_touch_that_never_settled_has_no_dynamics() {
        assert_eq!(Dynamics::of(&[]), None);
        assert_eq!(Dynamics::of(&[sample(0, 0.2, 10, 1.0), sample(40, 0.3, 20, 0.9)]), None);
    }
}
