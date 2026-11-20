use std::io::{Read, Write};

use elanfp_ipc::{CallError, Reply, Request, Verb};

use crate::{BoxError, Console, Fail, Msg, check_name};

pub const EXIT_NO: u8 = 1;
pub const EXIT_ERROR: u8 = 2;
pub const EXIT_BUSY: u8 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub verb: Verb,
    pub finger: Option<String>,
    pub uid: Option<u32>,
}

pub fn command(args: &[String]) -> Result<Option<Command>, Fail> {
    let Some(verb) = args.first().and_then(|word| match word.as_str() {
        "enroll" => Some(Verb::Enroll),
        "verify" => Some(Verb::Verify),
        "list" => Some(Verb::List),
        "delete" => Some(Verb::Delete),
        "unlock" => Some(Verb::Unlock),
        _ => None,
    }) else {
        return Ok(None);
    };
    let (mut finger, mut uid) = (None, None);
    let mut rest = args[1..].iter();
    while let Some(word) = rest.next() {
        if word == "--uid" && uid.is_none() {
            let value = rest.next().ok_or_else(|| Fail::new("missing-argument").sub("what", "arg-uid"))?;
            uid = Some(value.parse().map_err(|_| Fail::new("bad-uid").arg("value", value))?);
        } else if finger.is_none() && !matches!(verb, Verb::List | Verb::Unlock) && !word.starts_with('-') {
            check_name(word)?;
            finger = Some(word.clone());
        } else {
            return Err(Fail::new("unexpected-argument").arg("value", word));
        }
    }
    if finger.is_none() && matches!(verb, Verb::Enroll | Verb::Delete) {
        return Err(Fail::new("missing-argument").sub("what", "arg-finger"));
    }
    Ok(Some(Command { verb, finger, uid }))
}

pub fn converse<S: Read + Write>(console: &Console, service: &str, connection: std::io::Result<S>, request: &Request) -> Result<u8, BoxError> {
    let unavailable = |detail: String| Fail::new("ipc-unavailable").arg("path", service).arg("detail", detail);
    let stream = connection.map_err(|e| unavailable(e.to_string()))?;
    let reply = elanfp_ipc::converse(stream, request, |text| println!("{text}")).map_err(|e| match e {
        CallError::Io(io) => unavailable(io.to_string()),
        CallError::Garbled => unavailable(console.catalog.text("ipc-garbled")),
        CallError::Closed => unavailable(console.catalog.text("ipc-closed")),
        CallError::Unsendable => Fail::new("bad-request"),
    })?;
    let line = |key: &str, text: &str| console.catalog.render(&Msg::new(key).arg("text", text));
    Ok(match reply {
        Reply::Ok(text) => {
            println!("{}", line("reply-ok", &text));
            0
        }
        Reply::No(text) => {
            println!("{}", line("reply-no", &text));
            EXIT_NO
        }
        Reply::Busy(text) => {
            eprintln!("{}", line("error", &text));
            EXIT_BUSY
        }
        Reply::Err(text) | Reply::Message(text) => {
            eprintln!("{}", line("error", &text));
            EXIT_ERROR
        }
    })
}

pub fn fail(console: &Console, error: &BoxError) -> u8 {
    let text = console.catalog.render(&crate::as_msg(error.as_ref(), "error-internal"));
    eprintln!("{}", console.catalog.render(&Msg::new("error").arg("text", text)));
    EXIT_ERROR
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Result<Option<Command>, String> {
        let args: Vec<String> = line.split_whitespace().map(str::to_string).collect();
        command(&args).map_err(|e| e.0.key.to_string())
    }

    #[test]
    fn client_commands_are_parsed_strictly() {
        let finger = |name: &str| Some(name.to_string());
        assert_eq!(parse("list"), Ok(Some(Command { verb: Verb::List, finger: None, uid: None })));
        assert_eq!(parse("verify"), Ok(Some(Command { verb: Verb::Verify, finger: None, uid: None })));
        assert_eq!(parse("verify right-index"), Ok(Some(Command { verb: Verb::Verify, finger: finger("right-index"), uid: None })));
        assert_eq!(parse("enroll right-index --uid 1000"), Ok(Some(Command { verb: Verb::Enroll, finger: finger("right-index"), uid: Some(1000) })));
        assert_eq!(parse("delete --uid 1000 right-index"), Ok(Some(Command { verb: Verb::Delete, finger: finger("right-index"), uid: Some(1000) })));
        assert_eq!(parse("unlock --uid 1000"), Ok(Some(Command { verb: Verb::Unlock, finger: None, uid: Some(1000) })));
        assert_eq!(parse("unlock right-index"), Err("unexpected-argument".to_string()));
        assert_eq!(parse("probe"), Ok(None));
        assert_eq!(parse(""), Ok(None));
        assert_eq!(parse("enroll"), Err("missing-argument".to_string()));
        assert_eq!(parse("delete --uid 1000"), Err("missing-argument".to_string()));
        assert_eq!(parse("list right-index"), Err("unexpected-argument".to_string()));
        assert_eq!(parse("verify a b"), Err("unexpected-argument".to_string()));
        assert_eq!(parse("verify --uid"), Err("missing-argument".to_string()));
        assert_eq!(parse("verify --uid -1"), Err("bad-uid".to_string()));
        assert_eq!(parse("verify --uid 1 --uid 2"), Err("unexpected-argument".to_string()));
        assert_eq!(parse("enroll ../x"), Err("finger-name-invalid".to_string()));
        assert_eq!(parse("verify --force"), Err("unexpected-argument".to_string()));
    }
}
