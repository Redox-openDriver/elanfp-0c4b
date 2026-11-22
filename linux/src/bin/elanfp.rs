use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::Duration;

use elanfp_engine::client::{self, Command, EXIT_ERROR};
use elanfp_engine::service::ROOT;
use elanfp_engine::{BoxError, Console, Fail, Msg};
use elanfp_ipc::{Request, Verb};
use elanfp_linux::users;

const STEP_TIMEOUT: Duration = Duration::from_secs(120);
const SERVICE_USER: &str = "elanfp";

fn run_by_the_service(stream: &UnixStream) -> bool {
    users::peer_uid(stream).is_ok_and(|peer| peer == ROOT || peer == users::current_uid() || users::uid_of(SERVICE_USER) == Some(peer))
}

fn started_by_sudo() -> bool {
    let parent = unsafe { libc::getppid() };
    std::fs::read_to_string(format!("/proc/{parent}/comm")).is_ok_and(|name| matches!(name.trim(), "sudo" | "doas"))
}

fn target_uid(command: &Command) -> Result<u32, Fail> {
    if let Some(uid) = command.uid {
        return Ok(uid);
    }
    if users::effective_uid() != ROOT {
        return Ok(users::current_uid());
    }
    let invoker = started_by_sudo()
        .then(|| {
            std::env::var("SUDO_UID")
                .ok()
                .and_then(|value| value.parse().ok())
                .or_else(|| std::env::var("DOAS_USER").ok().and_then(|name| users::uid_of(&name)))
        })
        .flatten();
    match invoker {
        Some(uid) => Ok(uid),
        None if command.verb == Verb::List => Ok(ROOT),
        None => Err(Fail::new("uid-required")),
    }
}

fn socket_path() -> std::path::PathBuf {
    if let Some(explicit) = std::env::var_os("ELANFP_SOCKET").filter(|p| !p.is_empty()) {
        return explicit.into();
    }
    let system = std::path::PathBuf::from(elanfp_ipc::SYSTEM_SOCKET);
    if system.exists() {
        return system;
    }
    elanfp_ipc::user_socket().filter(|p| p.exists()).unwrap_or(system)
}

fn remote(console: &Console, command: Command) -> Result<ExitCode, BoxError> {
    let target = target_uid(&command)?;
    if users::effective_uid() == ROOT {
        let name = users::name_of(target).unwrap_or_else(|| target.to_string());
        eprintln!("{}", console.catalog.render(&Msg::new("acting-for").arg("name", &name).num("uid", target)));
    }
    let request = Request::new(command.verb, target).finger(command.finger).lang(elanfp_ipc::environment_language());
    let path = socket_path();
    let connection = elanfp_ipc::connect(&path, STEP_TIMEOUT);
    if connection.as_ref().is_ok_and(|stream| !run_by_the_service(stream)) {
        return Err(Fail::new("ipc-untrusted").arg("path", path.display()).into());
    }
    let code = client::converse(console, &path.to_string_lossy(), connection, &request)?;
    Ok(ExitCode::from(code))
}

fn run(console: &Console) -> Result<ExitCode, BoxError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match client::command(&args)? {
        Some(command) => remote(console, command),
        None => {
            println!("{}", console.catalog.text("usage"));
            Ok(ExitCode::from(EXIT_ERROR))
        }
    }
}

fn main() -> ExitCode {
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let console = Console::new();
    match run(&console) {
        Ok(code) => code,
        Err(e) => ExitCode::from(client::fail(&console, &e)),
    }
}
