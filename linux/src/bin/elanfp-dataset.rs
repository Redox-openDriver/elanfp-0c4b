use std::path::{Path, PathBuf};

use elanfp_core::quality::{self, Reject};
use elanfp_engine::papilla::{Enrollment, Frame, Template};
use elanfp_engine::{Fail, Msg, PROFILE, as_msg, catalog};

type BoxError = Box<dyn std::error::Error>;

const ROOTS: [&str; 2] = ["captures/dataset", "captures/dataset-v2"];
const EXCLUDED_SETS: [&str; 1] = ["dataset-v2/me-right-middle-normal"];
const SIDE: usize = 80;
const ACCEPTED: &str = "captures/accepted.tsv";
const FOREIGN: &str = "refs/datasets/fvc-crops/list-merged.tsv";
const STAINED_EVERY: usize = 8;

fn mislabeled() -> Vec<String> {
    std::fs::read_to_string("captures/mislabeled.txt")
        .map(|s| s.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
        .unwrap_or_default()
}

fn sorted_entries(dir: &Path) -> Result<Vec<PathBuf>, BoxError> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)?.filter_map(Result::ok).map(|e| e.path()).collect();
    out.sort();
    Ok(out)
}

fn list() -> Result<(), BoxError> {
    let excluded = mislabeled();
    let (mut out, mut kept, mut rejected) = (String::new(), 0, 0);
    for root in ROOTS.iter().map(Path::new).filter(|p| p.is_dir()) {
        let root_name = root.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        for set in sorted_entries(root)?.into_iter().filter(|p| p.is_dir()) {
            let label = set.file_name().and_then(|n| n.to_str()).ok_or_else(|| Fail::new("dataset-set-name").arg("path", set.display()))?.to_string();
            let source = format!("{root_name}/{label}");
            if EXCLUDED_SETS.contains(&source.as_str()) {
                continue;
            }
            let (finger, _) = label.rsplit_once('-').ok_or_else(|| Fail::new("dataset-set-name").arg("path", set.display()))?;
            let files = sorted_entries(&set)?;
            let base_path = files
                .iter()
                .rfind(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("base-")))
                .ok_or_else(|| Fail::new("dataset-no-base").arg("path", set.display()))?;
            let base = std::fs::read(base_path)?;
            for raw in files.iter().filter(|p| p.extension().is_some_and(|x| x == "raw")) {
                let stem = raw.file_stem().unwrap_or_default().to_string_lossy().to_string();
                if excluded.contains(&format!("{source}/{stem}")) {
                    continue;
                }
                let frame = std::fs::read(raw)?;
                let gate = quality::Verdict::measure(&frame, Some(&frame), &base, SIDE, SIDE);
                if matches!(gate.reject(), Some(Reject::PartialTouch | Reject::NoRidges)) {
                    rejected += 1;
                    continue;
                }
                out.push_str(&format!("{}/{label}/{stem}.pgm\t{finger}\t{source}\n", root.display()));
                kept += 1;
            }
        }
    }
    std::fs::write(ACCEPTED, out)?;
    println!("{}", catalog().render(&Msg::new("dataset-listed").num("kept", kept).num("rejected", rejected).arg("file", ACCEPTED)));
    Ok(())
}

struct Sample {
    finger: String,
    template: Template,
}

fn read_pgm(path: &str) -> Result<Vec<u8>, BoxError> {
    let data = std::fs::read(path)?;
    let header = format!("P5\n{SIDE} {SIDE}\n255\n");
    match data.strip_prefix(header.as_bytes()) {
        Some(pixels) if pixels.len() == SIDE * SIDE => Ok(pixels.to_vec()),
        _ => Err(Fail::new("dataset-bad-frame").arg("path", path).into()),
    }
}

struct Stain {
    file: String,
    picture: Vec<u8>,
    weight: f32,
}

impl Stain {
    fn over(&self, pixels: &mut [u8]) {
        for (pixel, fixed) in pixels.iter_mut().zip(&self.picture) {
            *pixel = (f32::from(*pixel) * (1.0 - self.weight) + f32::from(*fixed) * self.weight).round() as u8;
        }
    }
}

fn samples(list: &str, stain: Option<&Stain>, every: usize) -> Result<Vec<Sample>, BoxError> {
    let text = std::fs::read_to_string(list)?;
    let rows: Vec<(&str, &str)> = text
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(path, rest)| (path, rest.split('\t').next().unwrap_or(rest)))
        .step_by(every.max(1))
        .collect();
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get());
    let extract = |part: &[(&str, &str)]| -> Result<Vec<Sample>, String> {
        part.iter()
            .map(|(path, finger)| {
                let mut pixels = read_pgm(path).map_err(|e| catalog().render(&as_msg(e.as_ref(), "error-internal")))?;
                if let Some(stain) = stain {
                    stain.over(&mut pixels);
                }
                let frame = Frame::gray8(SIDE, SIDE, &pixels).map_err(|e| e.key().to_string())?;
                let template = Template::extract(&PROFILE, &frame).map_err(|e| e.key().to_string())?;
                Ok(Sample { finger: finger.to_string(), template })
            })
            .collect()
    };
    let parts: Vec<Result<Vec<Sample>, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = rows.chunks(rows.len().div_ceil(workers).max(1)).map(|part| scope.spawn(move || extract(part))).collect();
        handles.into_iter().map(|handle| handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))).collect()
    });
    let mut all = Vec::with_capacity(rows.len());
    for part in parts {
        all.extend(part.map_err(|detail| Fail::new("dataset-bad-frame").arg("path", detail))?);
    }
    Ok(all)
}

fn stain(percent: &str) -> Result<Stain, BoxError> {
    let weight = percent.parse::<u8>().ok().filter(|percent| (1..100).contains(percent)).ok_or_else(|| Fail::new("dataset-usage"))?;
    let list = std::fs::read_to_string(FOREIGN)?;
    let file = list.lines().next().and_then(|line| line.split('\t').next()).ok_or_else(|| Fail::new("dataset-bad-frame").arg("path", FOREIGN))?.to_string();
    Ok(Stain { picture: read_pgm(&file)?, file, weight: f32::from(weight) / 100.0 })
}

fn replay(stain: Option<Stain>) -> Result<(), BoxError> {
    let texts = catalog();
    let every = if stain.is_some() { STAINED_EVERY } else { 1 };
    if let Some(stain) = &stain {
        let mixed = Msg::new("replay-stained").arg("file", &stain.file).num("percent", (stain.weight * 100.0).round() as u32).num("every", every);
        println!("{}", texts.render(&mixed));
    }
    let own = samples(ACCEPTED, stain.as_ref(), 1)?;
    let foreign = if Path::new(FOREIGN).exists() { samples(FOREIGN, stain.as_ref(), every)? } else { Vec::new() };
    let mut fingers: Vec<&str> = own.iter().map(|s| s.finger.as_str()).collect();
    fingers.dedup();
    fingers.sort_unstable();
    fingers.dedup();
    for finger in fingers {
        let mine: Vec<&Sample> = own.iter().filter(|s| s.finger == finger).collect();
        if mine.len() < PROFILE.enrolling.min_touches * 2 {
            continue;
        }
        let mut enrollment = Enrollment::new(PROFILE).map_err(|e| Fail::new(e.key()))?;
        let mut used = 0;
        while !enrollment.is_complete() && used < mine.len() {
            let _ = enrollment.add(mine[used].template.clone());
            used += 1;
        }
        let touches = enrollment.touches();
        let still = enrollment.still_touches();
        let (mut gallery, summary) = match enrollment.finish() {
            Ok(done) => done,
            Err(e) => {
                println!(
                    "{}",
                    texts.render(&Msg::new("replay-failed").arg("finger", finger).num("touches", touches).nest("reason", Msg::new(e.key()).args(e.args())))
                );
                continue;
            }
        };
        println!(
            "{}",
            texts.render(
                &Msg::new("replay-enrolled")
                    .arg("finger", finger)
                    .num("touches", touches)
                    .num("kept", summary.kept)
                    .num("dropped", summary.dropped)
                    .num("groups", summary.groups)
                    .num("largest", summary.largest)
                    .num("still", still)
            )
        );
        let (mut missed, mut learned) = (0, 0);
        let probes = &mine[used..];
        for probe in probes {
            let (verdict, adapted) = gallery.verify_adapting(probe.template.clone());
            missed += usize::from(!verdict.is_match());
            learned += usize::from(adapted);
        }
        let others = own.iter().filter(|s| s.finger != finger).step_by(every).chain(foreign.iter());
        let (mut impostors, mut accepted, mut top) = (0, 0, 0);
        for other in others {
            let verdict = gallery.verify(&other.template);
            impostors += 1;
            accepted += usize::from(verdict.is_match());
            top = top.max(verdict.score());
        }
        println!(
            "{}",
            texts.render(
                &Msg::new("replay-verified")
                    .arg("finger", finger)
                    .num("probes", probes.len())
                    .num("missed", missed)
                    .arg("percent", format!("{:.1}", 100.0 * missed as f32 / probes.len().max(1) as f32))
                    .num("learned", learned)
                    .num("templates", gallery.templates().len())
                    .num("impostors", impostors)
                    .num("accepted", accepted)
                    .num("top", top)
                    .num("threshold", PROFILE.accept_score)
            )
        );
    }
    Ok(())
}

fn median(values: &mut [f32]) -> f32 {
    values.sort_by(f32::total_cmp);
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

fn dynamics() -> Result<(), BoxError> {
    let texts = catalog();
    for root in ROOTS.iter().map(Path::new).filter(|p| p.is_dir()) {
        for set in sorted_entries(root)?.into_iter().filter(|p| p.is_dir()) {
            let mut columns: [Vec<f32>; 6] = Default::default();
            for file in sorted_entries(&set)?.iter().filter(|p| p.to_string_lossy().ends_with(".dynamics.tsv")) {
                let fields: Vec<f32> = std::fs::read_to_string(file)?.split_whitespace().filter_map(|field| field.parse().ok()).collect();
                if fields.len() == columns.len() {
                    for (column, value) in columns.iter_mut().zip(fields) {
                        column.push(value);
                    }
                }
            }
            if columns[0].is_empty() {
                continue;
            }
            let [frames, settle, held, rise, drift, motion] = &mut columns;
            println!(
                "{}",
                texts.render(
                    &Msg::new("dynamics-set")
                        .arg("set", set.display())
                        .num("touches", frames.len())
                        .num("frames", median(frames))
                        .num("settle", median(settle))
                        .num("held", median(held))
                        .arg("rise", format!("{:.3}", median(rise)))
                        .arg("drift", format!("{:.3}", median(drift)))
                        .arg("motion", format!("{:.3}", median(motion)))
                )
            );
        }
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let result = match std::env::args().nth(1).as_deref() {
        Some("list") => list(),
        Some("replay") => replay(None),
        Some("stained") => stain(&std::env::args().nth(2).unwrap_or_default()).and_then(|stain| replay(Some(stain))),
        Some("dynamics") => dynamics(),
        _ => Err(Fail::new("dataset-usage").into()),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", catalog().render(&as_msg(e.as_ref(), "error-internal")));
            std::process::ExitCode::from(2)
        }
    }
}
