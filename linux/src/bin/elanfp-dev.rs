use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use elanfp_engine::client::{self, EXIT_ERROR, EXIT_NO};
use elanfp_engine::dynamics::Dynamics;
use elanfp_engine::session::{Detect, Session, Timing, Touch, pixels, sensor_fail, with_quality};
use elanfp_engine::store::{private_dir, write_private};
use elanfp_engine::{BoxError, Console, Fail, Msg, check_name, hex, region_key};
use elanfp_linux::usb;
use nusb::descriptors::TransferType;
use nusb::transfer::Direction;
use nusb::{MaybeFuture, Speed};

const COLLECT_WAIT: Duration = Duration::from_secs(20);
const DATASET: &str = "captures/dataset";

fn usb_fail(command: &'static str) -> impl Fn(nusb::Error) -> Fail {
    move |e| Fail::new("usb-error").arg("command", command).arg("detail", e)
}

fn speed_key(speed: Option<Speed>) -> &'static str {
    match speed {
        Some(Speed::Low) => "usb-speed-low",
        Some(Speed::Full) => "usb-speed-full",
        Some(Speed::High) => "usb-speed-high",
        Some(Speed::Super) => "usb-speed-super",
        Some(Speed::SuperPlus) => "usb-speed-super-plus",
        _ => "usb-speed-unknown",
    }
}

fn direction_key(direction: Direction) -> &'static str {
    match direction {
        Direction::In => "usb-direction-in",
        Direction::Out => "usb-direction-out",
    }
}

fn kind_key(kind: TransferType) -> &'static str {
    match kind {
        TransferType::Control => "usb-kind-control",
        TransferType::Isochronous => "usb-kind-isochronous",
        TransferType::Bulk => "usb-kind-bulk",
        TransferType::Interrupt => "usb-kind-interrupt",
    }
}

fn info(console: &Console) -> Result<(), BoxError> {
    let dev = usb::find()?;
    console.print(
        &Msg::new("info-device")
            .arg("vid", format!("{:04x}", dev.vendor_id()))
            .arg("pid", format!("{:04x}", dev.product_id()))
            .arg("bus", dev.bus_id())
            .arg("address", dev.device_address())
            .arg("maker", dev.manufacturer_string().unwrap_or_default())
            .arg("product", dev.product_string().unwrap_or_default())
            .sub("speed", speed_key(dev.speed())),
    );
    let handle = dev.open().wait().map_err(usb_fail("open"))?;
    let configuration = handle.active_configuration().map_err(|e| Fail::new("usb-error").arg("command", "configuration").arg("detail", e))?;
    for iface in configuration.interfaces() {
        for alt in iface.alt_settings() {
            console.print(
                &Msg::new("info-interface")
                    .arg("number", alt.interface_number())
                    .arg("alt", alt.alternate_setting())
                    .arg("class", hex(&[alt.class(), alt.subclass(), alt.protocol()])),
            );
            for ep in alt.endpoints() {
                console.print(
                    &Msg::new("info-endpoint")
                        .arg("address", format!("0x{:02x}", ep.address()))
                        .sub("direction", direction_key(ep.direction()))
                        .sub("kind", kind_key(ep.transfer_type()))
                        .num("size", ep.max_packet_size()),
                );
            }
        }
    }
    Ok(())
}

fn session(console: &mut Console) -> Result<Session<usb::NusbTransport>, BoxError> {
    let opened = usb::open()?;
    Session::open(opened.sensor, opened.vendor_id, opened.product_id, console)
}

fn probe(console: &mut Console) -> Result<(), BoxError> {
    let mut session = session(console)?;
    let identity = *session.identity();
    let (width, height, raw) = (session.dim().width(), session.dim().height(), session.dim().raw);
    console.print(&Msg::new("probe-firmware").arg("fw", hex(&identity.firmware)));
    console.print(&Msg::new("probe-generation").arg("gen", format!("{:02x}", identity.generation)));
    console.print(&Msg::new("probe-size").num("width", width).num("height", height).arg("raw", hex(&raw)));
    let sensor = session.sensor();
    let dac = sensor.dac().map_err(sensor_fail::<usb::NusbTransport>("40 21 06"))?;
    let mean = sensor.calibration_mean().map_err(sensor_fail::<usb::NusbTransport>("40 24"))?;
    let status = sensor.sensor_status().map_err(sensor_fail::<usb::NusbTransport>("40 13"))?;
    console.print(&Msg::new("probe-dac").arg("dac", format!("{dac:02x}")));
    console.print(&Msg::new("probe-mean").num("mean", mean));
    console.print(&Msg::new("probe-status").arg("status", format!("{status:02x}")));
    session.close()
}

fn touch(console: &mut Console, timing: Timing) -> Result<ExitCode, BoxError> {
    let opened = usb::open()?;
    let mut session = Session::open_with(opened.sensor, opened.vendor_id, opened.product_id, timing, console)?;
    let mode = if timing.detect == Detect::Sensor { "sensor" } else { "frames" };
    console.print(&Msg::new("touch-test-prompt").arg("mode", mode).num("seconds", COLLECT_WAIT.as_secs()));
    let outcome = session.touch(console, COLLECT_WAIT)?;
    session.close()?;
    Ok(match outcome {
        Touch::Accepted { verdict, .. } => {
            console.print(&with_quality(Msg::new("collect-accepted"), &verdict));
            ExitCode::SUCCESS
        }
        Touch::Rejected { reason, verdict } => {
            console.print(&with_quality(Msg::new("collect-rejected").sub("reason", reason), &verdict));
            ExitCode::from(EXIT_NO)
        }
        Touch::Missed(status) => {
            console.print(&Msg::new("touch-missed-retry").arg("status", format!("{status:02x}")));
            ExitCode::from(EXIT_NO)
        }
        Touch::Timeout => {
            console.print(&Msg::new("verify-no-touch"));
            ExitCode::from(EXIT_NO)
        }
    })
}

fn pgm(width: usize, height: usize, values: &[i32]) -> Vec<u8> {
    let lo = values.iter().copied().min().unwrap_or(0);
    let hi = values.iter().copied().max().unwrap_or(0);
    let span = (hi - lo).max(1);
    let mut out = format!("P5\n{width} {height}\n255\n").into_bytes();
    out.extend(values.iter().map(|&v| ((v - lo) * 255 / span) as u8));
    out
}

fn collect(console: &mut Console, label: &str, count: usize) -> Result<(), BoxError> {
    check_name(label)?;
    let dir = Path::new(DATASET).join(label);
    private_dir(Path::new(DATASET))?;
    private_dir(&dir)?;
    let first = std::fs::read_dir(&dir)?.filter_map(Result::ok).filter(|e| e.path().extension().is_some_and(|x| x == "raw")).count();
    let mut session = session(console)?;
    write_private(&dir.join(format!("base-{first:03}.bin")), session.base())?;
    let mut taken = 0;
    while taken < count {
        let n = first + taken;
        console.print(&Msg::new("collect-prompt").num("n", taken + 1).num("count", count).arg("label", label).sub("region", &region_key(n)));
        match session.touch(console, COLLECT_WAIT)? {
            Touch::Timeout => console.print(&Msg::new("touch-timeout-wait")),
            Touch::Missed(status) => console.print(&Msg::new("touch-missed-retry").arg("status", format!("{status:02x}"))),
            Touch::Rejected { reason, verdict } => console.print(&with_quality(Msg::new("collect-rejected").sub("reason", reason), &verdict)),
            Touch::Accepted { frame, verdict, trace } => {
                if let Some(dynamics) = Dynamics::of(&trace) {
                    write_private(&dir.join(format!("{n:03}.dynamics.tsv")), (dynamics.record() + "\n").as_bytes())?;
                }
                write_private(&dir.join(format!("{n:03}.raw")), frame.as_slice())?;
                let diff: Vec<i32> = pixels(&frame).zip(pixels(session.base())).map(|(f, b)| f - b).collect();
                write_private(&dir.join(format!("{n:03}.pgm")), &pgm(session.dim().width(), session.dim().height(), &diff))?;
                console.print(&with_quality(Msg::new("collect-accepted"), &verdict));
                taken += 1;
            }
        }
        session.pause(console);
    }
    session.close()?;
    console.print(&Msg::new("collect-done").num("count", count).arg("dir", dir.display()));
    Ok(())
}

fn run(console: &mut Console) -> Result<ExitCode, BoxError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let done = |r: Result<(), BoxError>| r.map(|_| ExitCode::SUCCESS);
    match args.first().map(String::as_str) {
        Some("info") => done(info(console)),
        Some("probe") => done(probe(console)),
        Some("touch") => match args.get(1).map(String::as_str) {
            None | Some("sensor") => touch(console, Timing::SENSOR),
            Some("frames") => touch(console, Timing::POLLING),
            Some(_) => Err(Fail::new("missing-argument").sub("what", "arg-touch-mode").into()),
        },
        Some("collect") => {
            let count = match args.get(2) {
                Some(text) => text.parse().map_err(|_| Fail::new("bad-count").arg("value", text))?,
                None => 10,
            };
            let label = args.get(1).ok_or_else(|| Fail::new("missing-argument").sub("what", "arg-label"))?;
            done(collect(console, label, count))
        }
        _ => {
            println!("{}", console.catalog.text("usage-dev"));
            Ok(ExitCode::from(EXIT_ERROR))
        }
    }
}

fn main() -> ExitCode {
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let mut console = Console::new();
    match run(&mut console) {
        Ok(code) => code,
        Err(e) => ExitCode::from(client::fail(&console, &e)),
    }
}
