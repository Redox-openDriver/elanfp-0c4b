use std::fs::OpenOptions;
use std::process::ExitCode;

use elanfp_engine::service::ROOT;
use elanfp_engine::{BoxError, Console, Fail, client};
use elanfp_ipc::{Request, Verb};
use elanfp_redox::{SCHEME, resource_path};

fn run(console: &Console) -> Result<u8, BoxError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = client::command(&args)? else {
        println!("{}", console.catalog.text("usage-redox"));
        return Ok(client::EXIT_ERROR);
    };
    let own = unsafe { libc::getuid() };
    let target = match command.uid {
        Some(uid) => uid,
        None if own == ROOT && command.verb != Verb::List => return Err(Fail::new("uid-required").into()),
        None => own,
    };
    let request = Request::new(command.verb, target).finger(command.finger).lang(elanfp_ipc::environment_language());
    let path = resource_path(SCHEME);
    client::converse(console, &path, OpenOptions::new().read(true).write(true).open(&path), &request)
}

fn main() -> ExitCode {
    let console = Console::new();
    ExitCode::from(run(&console).unwrap_or_else(|e| client::fail(&console, &e)))
}
