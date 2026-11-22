use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use drv_i18n::{Catalog, Msg};

const LANGUAGES: [&str; 2] = ["en", "ru"];
const SCRIPTS: [&str; 2] = ["install.sh", "uninstall.sh"];
const HELPERS: [&str; 3] = ["messages.sh", "trust.sh", "newkey.sh"];
const SPEAKERS: [&str; 4] = ["say", "item", "fail", "text"];
const FAMILIES: [&str; 2] = ["setup-", "remove-"];

fn dist() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("dist")
}

fn development() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("scripts")
}

fn source(language: &str) -> String {
    std::fs::read_to_string(dist().join("locales").join(format!("{language}.ftl"))).unwrap()
}

fn calls() -> Vec<(String, BTreeSet<String>)> {
    let mut found = Vec::new();
    for script in SCRIPTS {
        let text = std::fs::read_to_string(dist().join(script)).unwrap();
        for line in text.lines() {
            let words: Vec<&str> = line.split_whitespace().collect();
            for (at, word) in words.iter().enumerate() {
                let Some(key) = words.get(at + 1).filter(|key| SPEAKERS.contains(word) && FAMILIES.iter().any(|family| key.starts_with(family))) else {
                    continue;
                };
                let names = words[at + 2..].iter().map_while(|word| word.split_once('=').map(|(name, _)| name.to_string())).collect();
                found.push((key.to_string(), names));
            }
        }
    }
    found
}

fn shell(script: &str, arguments: &[String], environment: &[(&str, &str)]) -> String {
    let output = Command::new("sh")
        .arg("-c")
        .arg(format!(". \"$1/messages.sh\"; catalogs=$1/locales; shift; {script}"))
        .arg("sh")
        .arg(dist())
        .args(arguments)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .envs(environment.iter().copied())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim_end_matches('\n').to_string()
}

#[test]
fn installer_catalogues_agree_with_each_other_and_with_the_scripts() {
    let reference: BTreeSet<String> = drv_i18n::keys(&source("en")).into_iter().collect();
    assert!(reference.len() > 40, "{}", reference.len());
    let mut used: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (key, names) in calls() {
        assert!(reference.contains(&key), "{key} is not in the catalogue");
        let expected: BTreeSet<String> = drv_i18n::variables(&source("en"), &key).into_iter().collect();
        assert_eq!(names, expected, "arguments passed with {key}");
        used.insert(key, names);
    }
    let unused: Vec<&String> = reference.iter().filter(|key| !used.contains_key(*key)).collect();
    assert!(unused.is_empty(), "never printed: {unused:?}");
    for language in LANGUAGES {
        let text = source(language);
        let present: BTreeSet<String> = drv_i18n::keys(&text).into_iter().collect();
        assert_eq!(present, reference, "{language}: key set differs from en");
        for line in text.lines().filter(|line| !line.is_empty()) {
            let (key, message) = line.split_once(" = ").unwrap_or_else(|| panic!("{language}: not a one-line message: {line}"));
            assert!(reference.contains(key), "{language}: {line}");
            assert_eq!(drv_i18n::variables(&text, key), drv_i18n::variables(&source("en"), key), "{language}: variables of {key}");
            let plain = message.replace("{ $", "").replace(" }", "");
            assert!(
                !plain.contains(['{', '}']) && !message.starts_with(' ') && !message.ends_with(' '),
                "{language}: {key} needs more than the shell renderer does"
            );
        }
    }
}

#[test]
fn the_shell_renders_installer_messages_exactly_as_the_library_does() {
    for language in LANGUAGES {
        let text = source(language);
        let catalog = Catalog::for_languages(&[(language, text.as_str())], &[language.to_string()]);
        assert!(catalog.is_loaded() && catalog.language() == language);
        for key in drv_i18n::keys(&text) {
            let names = drv_i18n::variables(&text, &key);
            let value = |name: &str| format!("/{name} & \\1 50% $HOME `x` a=b");
            let message = names.iter().fold(Msg::new(&key), |message, name| message.arg(name, value(name)));
            let mut arguments = vec![key.clone()];
            arguments.extend(names.iter().map(|name| format!("{name}={}", value(name))));
            let rendered = shell(&format!("language={language}; text \"$@\""), &arguments, &[]);
            assert_eq!(rendered, catalog.try_render(&message).unwrap_or_else(|missing| panic!("{language}: {key}: {missing:?}")), "{language}: {key}");
        }
    }
    assert_eq!(shell("language=en; text setup-no-such-message one=1", &[], &[]), "setup-no-such-message");
}

#[test]
fn the_shell_picks_the_language_the_way_the_library_does() {
    let cases: [&[(&str, &str)]; 20] = [
        &[],
        &[("LANG", "ru_RU.UTF-8")],
        &[("LANG", "en_US.UTF-8")],
        &[("LANG", "de_DE.UTF-8")],
        &[("LANG", "ru_RU.UTF-8"), ("LC_ALL", "C")],
        &[("LANG", "ru_RU.UTF-8"), ("LC_MESSAGES", "en_GB.UTF-8")],
        &[("LANG", "en_US.UTF-8"), ("LANGUAGE", "de:ru:en")],
        &[("LANG", "C.UTF-8"), ("LANGUAGE", "ru")],
        &[("LANG", "de_DE.UTF-8"), ("LANGUAGE", "../../etc:*:ru_RU")],
        &[("LANG", "ru_RU.UTF-8"), ("LANGUAGE", "xx")],
        &[("LANG", "ru_RU.UTF-8"), ("LANGUAGE", "C")],
        &[("LANG", "ru_RU.UTF-8"), ("LANGUAGE", "de:C:ru")],
        &[("LANG", "ru_RU.UTF-8"), ("LANGUAGE", "de:POSIX.UTF-8:ru")],
        &[("LANG", "en_US.UTF-8"), ("LANGUAGE", "RU_ru:en")],
        &[("LANG", "RU"), ("LANGUAGE", "")],
        &[("LANG", "ru-RU")],
        &[("LANG", "!!"), ("LANGUAGE", "ru")],
        &[("LANG", "POSIX"), ("LANGUAGE", "ru")],
        &[("LANG", "abcdefghi_X"), ("LANGUAGE", "ru")],
        &[("LC_ALL", ""), ("LC_MESSAGES", "ru_RU"), ("LANG", "en_US")],
    ];
    for environment in cases {
        let wanted = drv_i18n::languages_from(|name| environment.iter().find(|(variable, _)| *variable == name).map(|(_, value)| value.to_string()));
        let expected = wanted.iter().map(String::as_str).find(|language| LANGUAGES.contains(language)).unwrap_or(drv_i18n::FALLBACK);
        assert_eq!(shell("language", &[], environment), expected, "{environment:?}");
    }
}

const SEALER: &str = r#"#!/bin/sh
printf '%s\n' "$@" > "$SEEN"
[ "$TROUBLE" != encrypt ] || exit 1
for last; do :; done
[ "$TROUBLE" = empty ] || printf 'SEALED ' > "$last"
[ "$TROUBLE" = empty ] || cat >> "$last"
[ "$TROUBLE" != empty ] || : > "$last"
"#;

struct Vault {
    dir: PathBuf,
}

impl Vault {
    fn new(tag: &str) -> Vault {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("elanfp-newkey-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::create_dir_all(dir.join("keys")).unwrap();
        std::fs::write(dir.join("bin/systemd-creds"), SEALER).unwrap();
        std::fs::set_permissions(dir.join("bin/systemd-creds"), std::fs::Permissions::from_mode(0o755)).unwrap();
        Vault { dir }
    }

    fn key(&self) -> PathBuf {
        self.dir.join("keys/elanfp-master")
    }

    fn create(&self, kind: &str, last: &str, trouble: &str) -> bool {
        let path = format!("{}:{}", self.dir.join("bin").display(), std::env::var("PATH").unwrap_or_default());
        let made = Command::new("sh")
            .arg(dist().join("newkey.sh"))
            .args([kind, self.key().to_str().unwrap(), last])
            .env("PATH", path)
            .env("TROUBLE", trouble)
            .env("SEEN", self.dir.join("seen"))
            .stderr(std::process::Stdio::null())
            .status();
        made.unwrap().success()
    }

    fn seen(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("seen")).unwrap_or_default().lines().map(str::to_string).collect()
    }

    fn kept(&self) -> Vec<String> {
        let mut names: Vec<String> =
            std::fs::read_dir(self.dir.join("keys")).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }
}

impl Drop for Vault {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn own_account() -> String {
    let id = |flag: &str| String::from_utf8(Command::new("id").arg(flag).output().unwrap().stdout).unwrap().trim().to_string();
    format!("{}:{}", id("-u"), id("-g"))
}

#[test]
fn a_new_key_never_replaces_one_that_is_already_there() {
    use std::os::unix::fs::PermissionsExt;
    let vault = Vault::new("file");
    assert!(vault.create("file", &own_account(), "none"));
    let first = std::fs::read(vault.key()).unwrap();
    assert_eq!((first.len(), std::fs::metadata(vault.key()).unwrap().permissions().mode() & 0o7777), (32, 0o400));
    assert_eq!(vault.kept(), ["elanfp-master"]);
    for kind in ["file", "tpm"] {
        assert!(!vault.create(kind, &if kind == "file" { own_account() } else { String::new() }, "none"), "{kind}");
        assert_eq!(std::fs::read(vault.key()).unwrap(), first, "{kind}");
        assert_eq!(vault.kept(), ["elanfp-master"], "{kind}");
    }
    let other = Vault::new("file-again");
    assert!(other.create("file", &own_account(), "none"));
    assert_ne!(std::fs::read(other.key()).unwrap(), first);
    let foreign = Vault::new("file-foreign");
    if own_account() != "0:0" {
        assert!(!foreign.create("file", "0:0", "none"));
        assert!(foreign.kept().is_empty());
    }
    assert!(!foreign.create("neither", "", "none"));
    assert!(foreign.kept().is_empty());
}

#[test]
fn a_sealed_key_is_bound_to_exactly_the_registers_that_were_asked_for() {
    for (pcrs, tag) in [("", "unbound"), ("7+11", "bound")] {
        let vault = Vault::new(tag);
        assert!(vault.create("tpm", pcrs, "none"), "{tag}");
        let sealed = std::fs::read(vault.key()).unwrap();
        assert!(sealed.starts_with(b"SEALED ") && sealed.len() == 7 + 32, "{tag}: {}", sealed.len());
        assert_eq!(vault.kept(), ["elanfp-master"], "{tag}");
        let seen = vault.seen();
        assert!(seen.contains(&format!("--tpm2-pcrs={pcrs}")) && seen.contains(&"--with-key=host+tpm2".to_string()), "{tag}: {seen:?}");
        assert_eq!(seen.iter().filter(|word| word.starts_with("--tpm2-pcrs")).count(), 1, "{tag}: {seen:?}");
    }
    for trouble in ["encrypt", "empty"] {
        let vault = Vault::new(trouble);
        assert!(!vault.create("tpm", "7", trouble), "{trouble}");
        assert!(vault.kept().is_empty(), "{trouble}: {:?}", vault.kept());
    }
}

fn trust(script: &str) -> bool {
    let run = Command::new("sh")
        .arg("-c")
        .arg(format!("as_root() {{ \"$@\"; }}; . \"$1/trust.sh\"; {script}"))
        .arg("sh")
        .arg(dist())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    run.unwrap().success()
}

#[test]
fn only_directories_that_root_alone_can_change_are_trusted() {
    use std::os::unix::fs::PermissionsExt;
    let root = unsafe { libc::getuid() } == 0;
    let dir = std::env::temp_dir().join(format!("elanfp-trust-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("own")).unwrap();
    std::fs::set_permissions(dir.join("own"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir_all(dir.join("open")).unwrap();
    std::fs::set_permissions(dir.join("open"), std::fs::Permissions::from_mode(0o775)).unwrap();
    std::os::unix::fs::symlink("/usr", dir.join("link")).unwrap();
    let trusted = |path: &Path| trust(&format!("trusted_dir '{}'", path.display()));
    assert!(trusted(Path::new("/usr")));
    assert!(!trusted(Path::new("/tmp")));
    assert!(!trusted(Path::new("")));
    assert!(!trusted(Path::new("/no/such/directory")));
    assert_eq!(trusted(&dir.join("link")), root);
    if root {
        std::os::unix::fs::lchown(dir.join("link"), Some(65534), Some(65534)).unwrap();
        assert!(!trusted(&dir.join("link")));
    }
    assert!(!trusted(&dir.join("open")));
    assert_eq!(trusted(&dir.join("own")), root);
    let mut sleeper = Command::new("sleep").arg("30").spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(trust("root_process sleep"), root);
    assert!(!trust("root_process no-such-program-anywhere"));
    assert!(!trust("watched_by sleep"));
    sleeper.kill().unwrap();
    sleeper.wait().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_installer_script_parses_in_the_shells_at_hand() {
    let shells: Vec<Vec<&str>> = vec![vec!["sh"], vec!["dash"], vec!["busybox", "sh"]];
    let mut checked = 0;
    for shell in shells {
        if Command::new(shell[0]).arg("-c").arg("exit 0").status().is_err() {
            continue;
        }
        for script in SCRIPTS.iter().chain(HELPERS.iter()) {
            let parsed = Command::new(shell[0]).args(&shell[1..]).arg("-n").arg(dist().join(script)).output().unwrap();
            assert!(parsed.status.success(), "{shell:?} {script}: {}", String::from_utf8_lossy(&parsed.stderr));
            checked += 1;
        }
    }
    assert!(checked >= SCRIPTS.len() + HELPERS.len());
}

#[test]
fn development_script_catalogues_agree_with_each_other_and_with_the_scripts() {
    let catalog = |language: &str| std::fs::read_to_string(development().join("locales").join(format!("{language}.ftl"))).unwrap();
    let reference: BTreeSet<String> = drv_i18n::keys(&catalog("en")).into_iter().collect();
    for language in LANGUAGES {
        let text = catalog(language);
        assert_eq!(drv_i18n::keys(&text).into_iter().collect::<BTreeSet<_>>(), reference, "{language}");
        for key in &reference {
            assert_eq!(drv_i18n::variables(&text, key), drv_i18n::variables(&catalog("en"), key), "{language}: variables of {key}");
        }
    }
    let mut used = BTreeSet::new();
    for script in ["redox-test.sh", "redox-hw.sh", "portability/alpine.sh"] {
        let text = std::fs::read_to_string(development().join(script)).unwrap();
        for line in text.lines() {
            let words: Vec<&str> = line.split_whitespace().collect();
            for pair in words.windows(2).filter(|pair| pair[0] == "text") {
                let names: BTreeSet<String> = words
                    .iter()
                    .skip_while(|word| **word != pair[1])
                    .skip(1)
                    .map_while(|word| word.split_once('=').map(|(name, _)| name.to_string()))
                    .collect();
                let expected: BTreeSet<String> = drv_i18n::variables(&catalog("en"), pair[1]).into_iter().collect();
                assert_eq!(names, expected, "{script}: {}", pair[1]);
                used.insert(pair[1].to_string());
            }
        }
    }
    assert_eq!(used, reference);
}
