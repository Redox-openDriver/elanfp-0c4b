use std::collections::BTreeSet;
use std::path::Path;

use elanfp_engine::{Catalog, LOCALES, REGIONS, region_key};

const LANGUAGES: [&str; 2] = ["ru", "en"];
const MARKERS: [&str; 6] = ["Msg::new(\"", "Fail::new(\"", "text(\"", "line(\"", "required(1, \"", "\"error-internal\""];
const DYNAMIC: [&str; 19] = [
    "reject-partial",
    "reject-ridges",
    "reject-moving",
    "arg-finger",
    "arg-label",
    "arg-touch-mode",
    "arg-uid",
    "usb-speed-low",
    "usb-speed-full",
    "usb-speed-high",
    "usb-speed-super",
    "usb-speed-super-plus",
    "usb-speed-unknown",
    "usb-direction-in",
    "usb-direction-out",
    "usb-kind-control",
    "usb-kind-isochronous",
    "usb-kind-bulk",
    "usb-kind-interrupt",
];

fn own_sources() -> Vec<(&'static str, &'static str)> {
    LOCALES.iter().copied().filter(|(_, source)| !source.contains("papilla-storage-")).collect()
}

fn scan(dir: &Path, used: &mut BTreeSet<String>, text: &mut String) {
    for entry in std::fs::read_dir(dir).unwrap().filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n != "target" && n != "tests") {
                scan(&path, used, text);
            }
            continue;
        }
        if path.extension().is_none_or(|x| x != "rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        for marker in MARKERS {
            for (at, _) in source.match_indices(marker) {
                let start = if marker.starts_with('"') { at + 1 } else { at + marker.len() };
                let key: String = source[start..].chars().take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-').collect();
                if !key.is_empty() && source[start + key.len()..].starts_with('"') {
                    used.insert(key);
                }
            }
        }
        for (at, _) in source.match_indices(".sub(\"") {
            let after_name = source[at + 6..].trim_start_matches(|c: char| c.is_ascii_lowercase());
            if let Some(rest) = after_name.strip_prefix("\", \"") {
                let key: String = rest.chars().take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-').collect();
                if !key.is_empty() && rest[key.len()..].starts_with('"') {
                    used.insert(key);
                }
            }
        }
        text.push_str(&source);
    }
}

fn used_keys() -> (BTreeSet<String>, String) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let (mut used, mut text) = (BTreeSet::new(), String::new());
    scan(&root, &mut used, &mut text);
    used.extend(DYNAMIC.iter().map(|k| k.to_string()));
    used.extend((0..REGIONS).map(region_key));
    (used, text)
}

#[test]
fn every_message_exists_in_every_language() {
    let (used, _) = used_keys();
    assert!(used.len() > 80, "{}", used.len());
    for lang in LANGUAGES {
        let catalog = Catalog::for_languages(&LOCALES, &[lang.to_string()]);
        assert!(catalog.is_loaded(), "{lang}: catalogue does not parse or has duplicate keys");
        assert_eq!(catalog.language(), lang);
        let missing: Vec<&String> = used.iter().filter(|k| !catalog.has(k)).collect();
        assert!(missing.is_empty(), "{lang}: missing {missing:?}");
    }
}

#[test]
fn languages_define_the_same_keys_and_variables_and_nothing_unused() {
    let (used, text) = used_keys();
    let sources = own_sources();
    let of = |lang: &str| sources.iter().find(|(l, _)| *l == lang).map(|(_, s)| *s).unwrap();
    let reference: BTreeSet<String> = drv_i18n::keys(of("ru")).into_iter().collect();
    for lang in LANGUAGES {
        let present: BTreeSet<String> = drv_i18n::keys(of(lang)).into_iter().collect();
        assert_eq!(present, reference, "{lang}: key set differs from ru");
        for key in &present {
            assert!(used.contains(key) || text.contains(&format!("\"{key}\"")), "{lang}: unused {key}");
            assert_eq!(drv_i18n::variables(of(lang), key), drv_i18n::variables(of("ru"), key), "{lang}: variables of {key}");
        }
    }
}

#[test]
fn name_rules_agree_between_protocol_and_store() {
    let long = "a".repeat(65);
    for name in ["right-index", "a", "0", "A.b_c-d", "", ".", "..", "-x", "a/b", "a b", "ё", "x\n", long.as_str()] {
        assert_eq!(elanfp_ipc::valid_name(name), elanfp_engine::valid_name(name), "{name:?}");
    }
    assert_eq!(elanfp_ipc::MAX_NAME, elanfp_engine::MAX_NAME);
}

#[test]
fn plural_forms_read_naturally() {
    let ru = Catalog::for_languages(&LOCALES, &["ru".to_string()]);
    let done = |n: usize| ru.render(&elanfp_engine::Msg::new("enroll-done").arg("finger", "x").num("count", n).arg("algorithm", "A"));
    assert!(done(1).contains("1 касание,") && done(42).contains("42 касания,") && done(45).contains("45 касаний,"), "{}", done(42));
    let en = Catalog::for_languages(&LOCALES, &["en".to_string()]);
    let dropped = |n: usize| en.render(&elanfp_engine::Msg::new("enroll-consistency").arg("seconds", "0.1").num("dropped", n));
    assert!(dropped(0).contains("all touches agree") && dropped(1).contains("1 touch that") && dropped(3).contains("3 touches that"), "{}", dropped(1));
}
