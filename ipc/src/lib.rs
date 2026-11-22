use std::io::{BufRead, BufReader, Read, Write};

#[cfg(target_os = "linux")]
mod socket;

#[cfg(target_os = "linux")]
pub use socket::{Bounded, Peer, SYSTEM_SOCKET, account_exists, call, connect, exchange, harden_process, name_of, peer, peer_uid, uid_of, user_socket};

pub const MAX_REQUEST: usize = 256;
pub const MAX_REPLY_LINE: usize = 4096;
pub const MAX_EVENT: usize = MAX_REPLY_LINE - 32;
pub const MAX_REPLY_TOTAL: u64 = 1 << 20;
pub const MAX_NAME: usize = 64;
pub const UNTRANSLATED: &str = "en";
pub const MIN_WAIT_S: u16 = 1;
pub const MAX_WAIT_S: u16 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Verify,
    Enroll,
    Delete,
    List,
    Unlock,
    Watch,
}

impl Verb {
    pub const ALL: [Verb; 6] = [Verb::Verify, Verb::Enroll, Verb::Delete, Verb::List, Verb::Unlock, Verb::Watch];

    pub fn word(self) -> &'static str {
        match self {
            Verb::Verify => "VERIFY",
            Verb::Enroll => "ENROLL",
            Verb::Delete => "DELETE",
            Verb::List => "LIST",
            Verb::Unlock => "UNLOCK",
            Verb::Watch => "WATCH",
        }
    }

    fn finger_fits(self, finger: Option<&str>) -> bool {
        match self {
            Verb::Enroll | Verb::Delete => finger.is_some(),
            Verb::List | Verb::Unlock | Verb::Watch => finger.is_none(),
            Verb::Verify => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub verb: Verb,
    pub uid: u32,
    pub finger: Option<String>,
    pub wait: Option<u16>,
    pub lang: Option<String>,
    pub acting: Option<u32>,
    pub program: Option<String>,
    pub keys: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detail {
    Text(String),
    Key(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub key: String,
    pub args: Vec<(String, Detail)>,
}

const KEY_MARK: char = '@';

fn plain(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'+' | b'-')
}

fn word(text: &str, extra: u8) -> bool {
    !text.is_empty() && text.len() <= MAX_NAME && text.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == extra)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn unescape(raw: &str) -> Option<String> {
    let mut value = Vec::with_capacity(raw.len());
    let mut bytes = raw.bytes();
    while let Some(byte) = bytes.next() {
        match byte {
            b'%' => value.push(hex_digit(bytes.next()?)? << 4 | hex_digit(bytes.next()?)?),
            byte if plain(byte) => value.push(byte),
            _ => return None,
        }
    }
    String::from_utf8(value).ok()
}

impl Event {
    pub fn new(key: &str) -> Self {
        Event { key: key.to_string(), args: Vec::new() }
    }

    pub fn with(mut self, name: &str, text: &str) -> Self {
        self.args.push((name.to_string(), Detail::Text(text.to_string())));
        self
    }

    pub fn with_key(mut self, name: &str, key: &str) -> Self {
        self.args.push((name.to_string(), Detail::Key(key.to_string())));
        self
    }

    fn detail(&self, name: &str) -> Option<&Detail> {
        self.args.iter().find(|(known, _)| known == name).map(|(_, detail)| detail)
    }

    pub fn text_of(&self, name: &str) -> Option<&str> {
        match self.detail(name)? {
            Detail::Text(text) => Some(text),
            Detail::Key(_) => None,
        }
    }

    pub fn key_of(&self, name: &str) -> Option<&str> {
        match self.detail(name)? {
            Detail::Key(key) => Some(key),
            Detail::Text(_) => None,
        }
    }

    pub fn encode(&self) -> String {
        let mut line = self.key.clone();
        for (name, detail) in &self.args {
            let mut piece = format!(" {name}=");
            let room = MAX_EVENT.saturating_sub(line.len());
            match detail {
                Detail::Key(key) => {
                    piece.push(KEY_MARK);
                    piece.push_str(key);
                }
                Detail::Text(text) => {
                    for character in text.chars() {
                        let mut escaped = String::new();
                        for byte in character.encode_utf8(&mut [0u8; 4]).bytes() {
                            if plain(byte) {
                                escaped.push(char::from(byte));
                            } else {
                                escaped.push_str(&format!("%{byte:02x}"));
                            }
                        }
                        if piece.len() + escaped.len() > room {
                            break;
                        }
                        piece.push_str(&escaped);
                    }
                }
            }
            if piece.len() > room {
                break;
            }
            line.push_str(&piece);
        }
        line
    }

    pub fn parse(text: &str) -> Option<Event> {
        let mut words = text.split(' ');
        let key = words.next().filter(|key| word(key, b'-'))?;
        let mut event = Event::new(key);
        for pair in words {
            let (name, raw) = pair.split_once('=')?;
            if !word(name, b'_') {
                return None;
            }
            let detail = match raw.strip_prefix(KEY_MARK) {
                Some(key) if word(key, b'-') => Detail::Key(key.to_string()),
                Some(_) => return None,
                None => Detail::Text(unescape(raw)?),
            };
            event.args.push((name.to_string(), detail));
        }
        Some(event)
    }
}

pub const FINGERS: [&str; 10] = [
    "left-thumb",
    "left-index-finger",
    "left-middle-finger",
    "left-ring-finger",
    "left-little-finger",
    "right-thumb",
    "right-index-finger",
    "right-middle-finger",
    "right-ring-finger",
    "right-little-finger",
];

pub fn standard_finger(name: &str) -> Option<&'static str> {
    FINGERS.iter().copied().find(|finger| *finger == name || finger.strip_suffix("-finger") == Some(name))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Message(String),
    Ok(String),
    No(String),
    Busy(String),
    Err(String),
}

pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_NAME
        && bytes[0].is_ascii_alphanumeric()
        && bytes.iter().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

pub fn valid_language(lang: &str) -> bool {
    (2..=8).contains(&lang.len()) && lang.bytes().all(|b| b.is_ascii_lowercase())
}

fn bare(request: &Request) -> bool {
    request.wait.is_none() && request.lang.is_none() && request.acting.is_none() && request.program.is_none() && !request.keys
}

fn parse_uid(word: &str) -> Option<u32> {
    let canonical = !word.is_empty() && word.len() <= 10 && word.bytes().all(|b| b.is_ascii_digit()) && (word == "0" || !word.starts_with('0'));
    if canonical { word.parse().ok() } else { None }
}

impl Request {
    pub fn new(verb: Verb, uid: u32) -> Self {
        Request { verb, uid, finger: None, wait: None, lang: None, acting: None, program: None, keys: false }
    }

    pub fn program(mut self, name: Option<String>) -> Self {
        self.program = name;
        self
    }

    pub fn acting(mut self, uid: Option<u32>) -> Self {
        self.acting = uid;
        self
    }

    pub fn keys(mut self, keys: bool) -> Self {
        self.keys = keys;
        self
    }

    pub fn finger(mut self, finger: Option<String>) -> Self {
        self.finger = finger;
        self
    }

    pub fn wait(mut self, seconds: Option<u16>) -> Self {
        self.wait = seconds;
        self
    }

    pub fn lang(mut self, lang: Option<String>) -> Self {
        self.lang = lang;
        self
    }

    pub fn encode(&self) -> Option<String> {
        let finger = self.finger.as_deref();
        let wait_fits = self.wait.is_none_or(|seconds| self.verb == Verb::Verify && (MIN_WAIT_S..=MAX_WAIT_S).contains(&seconds));
        let program_fits = self.program.as_deref().is_none_or(|name| self.verb == Verb::Verify && valid_name(name));
        if !self.verb.finger_fits(finger) || !finger.is_none_or(valid_name) || !wait_fits || !program_fits || !self.lang.as_deref().is_none_or(valid_language) {
            return None;
        }
        let mut line = format!("{} {}", self.verb.word(), self.uid);
        if let Some(finger) = &self.finger {
            line.push(' ');
            line.push_str(finger);
        }
        if let Some(wait) = self.wait {
            line.push_str(&format!(" wait={wait}"));
        }
        if let Some(lang) = &self.lang {
            line.push_str(&format!(" lang={lang}"));
        }
        if let Some(acting) = self.acting {
            line.push_str(&format!(" as={acting}"));
        }
        if let Some(program) = &self.program {
            line.push_str(&format!(" svc={program}"));
        }
        if self.keys {
            line.push_str(" fmt=keys");
        }
        line.push('\n');
        (line.len() <= MAX_REQUEST).then_some(line)
    }

    pub fn parse(line: &str) -> Option<Request> {
        if line.len() > MAX_REQUEST {
            return None;
        }
        let mut words = line.strip_suffix('\n')?.split(' ');
        let name = words.next()?;
        let verb = Verb::ALL.into_iter().find(|verb| verb.word() == name)?;
        let mut request = Request::new(verb, parse_uid(words.next()?)?);
        for word in words {
            match word.split_once('=') {
                Some(("wait", value)) if request.wait.is_none() && verb == Verb::Verify => {
                    let seconds = parse_uid(value)?;
                    if !(u32::from(MIN_WAIT_S)..=u32::from(MAX_WAIT_S)).contains(&seconds) {
                        return None;
                    }
                    request.wait = Some(seconds as u16);
                }
                Some(("lang", value)) if request.lang.is_none() && valid_language(value) => request.lang = Some(value.to_string()),
                Some(("as", value)) if request.acting.is_none() => request.acting = Some(parse_uid(value)?),
                Some(("svc", value)) if request.program.is_none() && verb == Verb::Verify && valid_name(value) => request.program = Some(value.to_string()),
                Some(("fmt", "keys")) if !request.keys => request.keys = true,
                None if request.finger.is_none() && bare(&request) && valid_name(word) => request.finger = Some(word.to_string()),
                _ => return None,
            }
        }
        verb.finger_fits(request.finger.as_deref()).then_some(request)
    }
}

fn single_line(text: &str) -> String {
    let mut out: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let mut limit = MAX_REPLY_LINE - 16;
    if out.len() > limit {
        while !out.is_char_boundary(limit) {
            limit -= 1;
        }
        out.truncate(limit);
    }
    out
}

impl Reply {
    pub fn encode(&self) -> String {
        let (kind, text) = match self {
            Reply::Message(t) => ("MSG", t),
            Reply::Ok(t) => ("OK", t),
            Reply::No(t) => ("NO", t),
            Reply::Busy(t) => ("BUSY", t),
            Reply::Err(t) => ("ERR", t),
        };
        format!("{kind} {}\n", single_line(text))
    }

    pub fn parse(line: &str) -> Option<Reply> {
        let line = line.strip_suffix('\n')?;
        let (kind, text) = line.split_once(' ').unwrap_or((line, ""));
        let text: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        match kind {
            "MSG" => Some(Reply::Message(text)),
            "OK" => Some(Reply::Ok(text)),
            "NO" => Some(Reply::No(text)),
            "BUSY" => Some(Reply::Busy(text)),
            "ERR" => Some(Reply::Err(text)),
            _ => None,
        }
    }

    pub fn is_final(&self) -> bool {
        !matches!(self, Reply::Message(_))
    }
}

pub fn language_of(locale: &str) -> Option<String> {
    let lang = locale.split(['_', '.', '@', '-']).next().unwrap_or("").to_ascii_lowercase();
    (valid_language(&lang) && lang != "posix").then_some(lang)
}

fn untranslated(locale: &str) -> bool {
    let name = locale.split(['.', '@']).next().unwrap_or("");
    name == "C" || name == "POSIX"
}

pub fn language_from(variable: impl Fn(&str) -> Option<String>) -> Option<String> {
    let set = |name: &str| variable(name).filter(|value| !value.is_empty());
    let base = ["LC_ALL", "LC_MESSAGES", "LANG"].into_iter().find_map(set).as_deref().and_then(language_of)?;
    let Some(preferred) = set("LANGUAGE") else { return Some(base) };
    Some(preferred.split(':').take_while(|item| !untranslated(item)).find_map(language_of).unwrap_or_else(|| UNTRANSLATED.to_string()))
}

pub fn environment_language() -> Option<String> {
    language_from(|name| std::env::var(name).ok())
}

#[derive(Debug)]
pub enum CallError {
    Io(std::io::Error),
    Garbled,
    Closed,
    Unsendable,
}

impl From<std::io::Error> for CallError {
    fn from(e: std::io::Error) -> Self {
        CallError::Io(e)
    }
}

pub fn converse<S: Read + Write>(mut stream: S, request: &Request, mut on_message: impl FnMut(&str)) -> Result<Reply, CallError> {
    stream.write_all(request.encode().ok_or(CallError::Unsendable)?.as_bytes())?;
    let mut reader = BufReader::new(stream.take(MAX_REPLY_TOTAL));
    loop {
        let mut line = Vec::new();
        let read = (&mut reader).take(MAX_REPLY_LINE as u64).read_until(b'\n', &mut line)?;
        if read == 0 {
            return Err(CallError::Closed);
        }
        let text = String::from_utf8(line).map_err(|_| CallError::Garbled)?;
        match Reply::parse(&text) {
            Some(Reply::Message(m)) => on_message(&m),
            Some(final_reply) => return Ok(final_reply),
            None => return Err(CallError::Garbled),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verify(uid: u32) -> Request {
        Request::new(Verb::Verify, uid)
    }

    #[test]
    fn requests_roundtrip() {
        for r in [
            verify(1000),
            verify(0).finger(Some("right-index".into())),
            verify(1000).wait(Some(10)).lang(Some("ru".into())),
            verify(4294967295).finger(Some("a.b_c-d".into())).wait(Some(60)),
            Request::new(Verb::Enroll, 1000).finger(Some("right-index".into())).lang(Some("en".into())),
            Request::new(Verb::Delete, 1000).finger(Some("x".into())),
            Request::new(Verb::List, 1000),
            Request::new(Verb::Unlock, 1000).acting(Some(0)),
            Request::new(Verb::Watch, 1000).keys(true),
            verify(1000).wait(Some(10)).program(Some("sudo".into())).keys(true),
            verify(1000).finger(Some("right-index".into())).program(Some("polkit-1".into())),
        ] {
            assert_eq!(Request::parse(&r.encode().unwrap()), Some(r));
        }
    }

    #[test]
    fn a_request_that_would_not_parse_back_is_never_sent() {
        let long = "a".repeat(65);
        for finger in ["x as=0", "x\nVERIFY 0", "", "../x", ".x", "a b", "ё", long.as_str()] {
            assert_eq!(verify(1000).finger(Some(finger.to_string())).encode(), None, "{finger:?}");
        }
        for lang in ["RU", "r", "ru as=0", "ru\n", "abcdefghi"] {
            assert_eq!(verify(1000).lang(Some(lang.to_string())).encode(), None, "{lang:?}");
        }
        assert_eq!(verify(1000).wait(Some(0)).encode(), None);
        assert_eq!(verify(1000).wait(Some(61)).encode(), None);
        assert_eq!(Request::new(Verb::List, 1000).wait(Some(5)).encode(), None);
        assert_eq!(Request::new(Verb::Enroll, 1000).encode(), None);
        assert_eq!(Request::new(Verb::Delete, 1000).encode(), None);
        assert_eq!(Request::new(Verb::List, 1000).finger(Some("x".into())).encode(), None);
        assert_eq!(Request::new(Verb::Unlock, 1000).finger(Some("x".into())).encode(), None);
        assert_eq!(Request::new(Verb::Watch, 1000).finger(Some("x".into())).encode(), None);
        assert_eq!(Request::new(Verb::Watch, 1000).wait(Some(5)).encode(), None);
        assert_eq!(Request::parse("WATCH 1000 x\n"), None);
        assert_eq!(Request::parse("WATCH 1000 wait=5\n"), None);
        assert_eq!(Request::new(Verb::List, 1000).program(Some("sudo".into())).encode(), None);
        assert_eq!(verify(1000).program(Some("../x y".into())).encode(), None);
        for line in
            ["LIST 1000 svc=sudo\n", "VERIFY 1000 svc=\n", "VERIFY 1000 svc=a/b\n", "VERIFY 1000 svc=sudo svc=su\n", "VERIFY 1000 svc=sudo right-index\n"]
        {
            assert_eq!(Request::parse(line), None, "{line:?}");
        }
        let mut stream = Scripted { reply: std::io::Cursor::new(b"OK\n".to_vec()), sent: Vec::new() };
        assert!(matches!(converse(&mut stream, &verify(1000).finger(Some("x as=0".into())), |_| {}), Err(CallError::Unsendable)));
        assert!(stream.sent.is_empty());
    }

    #[test]
    fn hostile_or_sloppy_requests_are_rejected() {
        let long = format!("VERIFY 1000 {}\n", "a".repeat(65));
        let huge = format!("VERIFY 1000 lang={}\n", "a".repeat(400));
        for bad in [
            "VERIFY 1000",
            "VERIFY\n",
            "VERIFY  1000\n",
            "VERIFY 1000 \n",
            "VERIFY alice\n",
            "VERIFY -1\n",
            "VERIFY +5\n",
            "VERIFY 01000\n",
            "VERIFY 4294967296\n",
            "VERIFY 1000 ../../etc/passwd\n",
            "VERIFY 1000 .hidden\n",
            "VERIFY 1000 a b\n",
            "VERIFY 1000 wait=0\n",
            "VERIFY 1000 wait=61\n",
            "VERIFY 1000 wait=5 wait=6\n",
            "VERIFY 1000 wait=5 finger\n",
            "VERIFY 1000 lang=RU\n",
            "VERIFY 1000 lang=../x\n",
            "VERIFY 1000 debug=1\n",
            "ENROLL 1000\n",
            "ENROLL 1000 x wait=5\n",
            "DELETE 1000\n",
            "LIST 1000 x\n",
            "UNLOCK 1000 x\n",
            "UNLOCK\n",
            "REBOOT 0\n",
            "verify 1000\n",
            "VERIFY 1000\r\n",
            "VERIFY 1000\n\n",
            "VERIFY 1000\0\n",
            long.as_str(),
            huge.as_str(),
        ] {
            assert_eq!(Request::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn replies_stay_on_one_bounded_line() {
        let forged = Reply::Message("a\nOK forged\r\x1b[2J".into()).encode();
        assert_eq!(forged, "MSG a OK forged  [2J\n");
        assert_eq!(Reply::parse(&forged), Some(Reply::Message("a OK forged  [2J".into())));
        let long = Reply::Err("я".repeat(5000)).encode();
        assert!(long.len() <= MAX_REPLY_LINE && long.ends_with('\n') && std::str::from_utf8(long.as_bytes()).is_ok());
        assert!(Reply::Ok(String::new()).is_final() && !Reply::Message(String::new()).is_final());
        assert_eq!(Reply::parse("BUSY x\n"), Some(Reply::Busy("x".into())));
        assert_eq!(Reply::parse("OK\n"), Some(Reply::Ok(String::new())));
        assert_eq!(Reply::parse("WHAT x\n"), None);
        assert_eq!(Reply::parse("OK x"), None);
    }

    #[test]
    fn delegation_and_key_format_are_explicit_options() {
        let delegated = Request::new(Verb::Verify, 1000).wait(Some(30)).acting(Some(0)).keys(true);
        assert_eq!(delegated.encode().as_deref(), Some("VERIFY 1000 wait=30 as=0 fmt=keys\n"));
        assert_eq!(Request::parse(&delegated.encode().unwrap()), Some(delegated));
        let enroll = Request::new(Verb::Enroll, 1000).finger(Some("right-index-finger".into())).acting(Some(1000)).keys(true);
        assert_eq!(Request::parse(&enroll.encode().unwrap()), Some(enroll));
        assert_eq!(Request::parse("VERIFY 1000 keys\n").and_then(|r| r.finger).as_deref(), Some("keys"));
        for bad in [
            "VERIFY 1000 as=\n",
            "VERIFY 1000 as=-1\n",
            "VERIFY 1000 as=1 as=2\n",
            "VERIFY 1000 fmt=text\n",
            "VERIFY 1000 fmt=keys fmt=keys\n",
            "VERIFY 1000 as=1 finger\n",
            "LIST 1000 as=01\n",
        ] {
            assert_eq!(Request::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn events_survive_the_wire_with_any_text() {
        let event = Event::new("verify-bad-frame")
            .with_key("reason", "reject-partial")
            .with("detail", "50% «мимо» = плохо\n\x1b[2J")
            .with("mail", "@reject-partial")
            .with("n", "3");
        let line = event.encode();
        assert!(line.is_ascii() && !line.contains(['\n', '\x1b']), "{line}");
        assert_eq!(line.matches('@').count(), 1, "{line}");
        assert_eq!(Event::parse(&line), Some(event.clone()));
        assert_eq!((event.text_of("n"), event.text_of("missing"), event.text_of("reason")), (Some("3"), None, None));
        assert_eq!((event.key_of("reason"), event.key_of("mail"), event.text_of("mail")), (Some("reject-partial"), None, Some("@reject-partial")));
        assert_eq!(Event::parse("verified"), Some(Event::new("verified")));
        let bad = [
            "",
            "Verified",
            "ok name",
            "ok =x",
            "ok a=%zz",
            "ok a=%4",
            "ok a=b c",
            "ok a=\u{442}",
            "ok a=%ff",
            "ok a=%+f",
            "ok a=%4A",
            "ok a=@",
            "ok a=@Up",
            "ok a=x@y",
        ];
        for bad in bad {
            assert_eq!(Event::parse(bad), None, "{bad:?}");
        }
        let reply = Reply::Message(event.encode()).encode();
        assert_eq!(Reply::parse(&reply), Some(Reply::Message(event.encode())));
    }

    #[test]
    fn an_event_too_long_for_one_line_is_shortened_between_characters() {
        let long = Event::new("list").with("fingers", &"палец, ".repeat(900)).with_key("after", "dropped").with("tail", "x");
        let line = long.encode();
        assert!(line.len() <= MAX_EVENT, "{}", line.len());
        let back = Event::parse(&line).expect("still one valid event");
        let kept = back.text_of("fingers").unwrap();
        assert!(kept.len() > 1000 && "палец, ".repeat(900).starts_with(kept));
        assert_eq!((back.key_of("after"), back.text_of("tail")), (None, None));
        let reply = Reply::Err(line.clone()).encode();
        assert_eq!(Reply::parse(&reply), Some(Reply::Err(line)));
        let crowded = (0..200).fold(Event::new("many"), |event, n| event.with(&format!("name{n}"), &"z".repeat(40)));
        assert!(Event::parse(&crowded.encode()).is_some_and(|event| event.args.len() < 200 && event.args.len() > 50));
    }

    #[test]
    fn standard_finger_names_are_recognised() {
        assert_eq!(standard_finger("right-index-finger"), Some("right-index-finger"));
        assert_eq!(standard_finger("right-index"), Some("right-index-finger"));
        assert_eq!(standard_finger("left-thumb"), Some("left-thumb"));
        assert_eq!(standard_finger("any"), None);
        assert_eq!(standard_finger("thumb"), None);
        assert!(FINGERS.iter().all(|finger| valid_name(finger)));
    }

    #[test]
    fn locale_variables_follow_gettext_rules() {
        let env =
            |pairs: &'static [(&'static str, &'static str)]| move |name: &str| pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| value.to_string());
        assert_eq!(language_from(env(&[("LANG", "ru_RU.UTF-8")])).as_deref(), Some("ru"));
        assert_eq!(language_from(env(&[("LANG", "ru_RU.UTF-8"), ("LC_ALL", "C")])), None);
        assert_eq!(language_from(env(&[("LANGUAGE", "ru"), ("LC_ALL", "POSIX")])), None);
        assert_eq!(language_from(env(&[("LANGUAGE", "de:ru"), ("LANG", "en_US.UTF-8")])).as_deref(), Some("de"));
        assert_eq!(language_from(env(&[("LANGUAGE", "C:ru"), ("LANG", "ru_RU.UTF-8")])).as_deref(), Some("en"));
        assert_eq!(language_from(env(&[("LANGUAGE", "xx"), ("LANG", "ru_RU.UTF-8")])).as_deref(), Some("xx"));
        assert_eq!(language_from(env(&[("LANGUAGE", "../x"), ("LANG", "ru_RU.UTF-8")])).as_deref(), Some("en"));
        assert_eq!(language_from(env(&[("LANGUAGE", ""), ("LANG", "ru_RU.UTF-8")])).as_deref(), Some("ru"));
        assert_eq!(language_from(env(&[("LC_MESSAGES", "en_GB"), ("LANG", "ru_RU")])).as_deref(), Some("en"));
        assert_eq!(language_from(env(&[("LANGUAGE", "ru")])), None);
        assert_eq!(language_from(env(&[])), None);
    }

    #[test]
    fn language_tags() {
        assert_eq!(language_of("ru_RU.UTF-8").as_deref(), Some("ru"));
        assert_eq!(language_of("en-GB").as_deref(), Some("en"));
        assert_eq!(language_of("C"), None);
        assert_eq!(language_of("POSIX"), None);
        assert_eq!(language_of("../../etc"), None);
    }

    struct Scripted {
        reply: std::io::Cursor<Vec<u8>>,
        sent: Vec<u8>,
    }

    impl Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reply.read(buf)
        }
    }

    impl Write for Scripted {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.sent.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn talk(reply: &[u8]) -> (Result<Reply, CallError>, Vec<String>, Vec<u8>) {
        let mut stream = Scripted { reply: std::io::Cursor::new(reply.to_vec()), sent: Vec::new() };
        let mut seen = Vec::new();
        let outcome = converse(&mut stream, &verify(1000), |m| seen.push(m.to_string()));
        (outcome, seen, stream.sent)
    }

    #[test]
    fn control_characters_from_the_peer_never_reach_the_terminal() {
        assert_eq!(Reply::parse("OK fine\x1b[2J\x07\r!\n"), Some(Reply::Ok("fine [2J  !".into())));
        assert_eq!(Reply::parse("MSG a\tb\n"), Some(Reply::Message("a b".into())));
    }

    #[test]
    fn conversation_streams_messages_and_survives_a_hostile_peer() {
        let (reply, seen, sent) = talk(b"MSG one\nMSG two\nOK done\nOK ignored\n");
        assert_eq!((reply.unwrap(), seen, sent), (Reply::Ok("done".into()), vec!["one".to_string(), "two".to_string()], b"VERIFY 1000\n".to_vec()));
        assert!(matches!(talk(b"MSG partial").0, Err(CallError::Garbled)));
        assert!(matches!(talk(b"\xff\xfe\n").0, Err(CallError::Garbled)));
        assert!(matches!(talk(b"").0, Err(CallError::Closed)));
        assert!(matches!(talk(b"MSG only progress\n").0, Err(CallError::Closed)));
        assert_eq!(talk(b"OK \n").0.unwrap(), Reply::Ok(String::new()));
        assert!(matches!(talk(b"WHAT now\n").0, Err(CallError::Garbled)));
        assert!(matches!(talk(&vec![b'x'; MAX_REPLY_LINE * 3]).0, Err(CallError::Garbled)));
        let endless: Vec<u8> = b"MSG again\n".iter().copied().cycle().take(MAX_REPLY_TOTAL as usize + 4096).collect();
        let (outcome, seen, _) = talk(&endless);
        assert!(outcome.is_err() && seen.len() <= MAX_REPLY_TOTAL as usize / 10);
    }
}
