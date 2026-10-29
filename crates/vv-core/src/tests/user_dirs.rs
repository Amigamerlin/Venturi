use super::*;
use std::path::Path;

fn vars(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
    move |name| {
        pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| OsString::from(value))
    }
}

#[test]
fn unix_keeps_the_xdg_layout() {
    let env = vars(&[("HOME", "/home/u"), ("APPDATA", "C:\\ignored")]);
    assert_eq!(
        resolve(
            &env,
            false,
            "XDG_DATA_HOME",
            "APPDATA",
            &[".local", "share"]
        )
        .as_deref(),
        Some(Path::new("/home/u/.local/share"))
    );
}

#[test]
fn xdg_variable_wins_everywhere() {
    let env = vars(&[("XDG_CONFIG_HOME", "/cfg"), ("APPDATA", "C:\\Roaming")]);
    assert_eq!(
        resolve(&env, true, "XDG_CONFIG_HOME", "APPDATA", &[".config"]).as_deref(),
        Some(Path::new("/cfg"))
    );
}

#[test]
fn windows_uses_its_system_folders() {
    let env = vars(&[
        ("USERPROFILE", "C:\\Users\\u"),
        ("APPDATA", "C:\\Users\\u\\AppData\\Roaming"),
        ("LOCALAPPDATA", "C:\\Users\\u\\AppData\\Local"),
    ]);
    assert_eq!(
        resolve(&env, true, "XDG_CACHE_HOME", "LOCALAPPDATA", &[".cache"]),
        Some(PathBuf::from("C:\\Users\\u\\AppData\\Local"))
    );
    assert_eq!(
        resolve_home(&env, true),
        Some(PathBuf::from("C:\\Users\\u"))
    );
}

#[test]
fn windows_without_appdata_falls_back_to_the_profile() {
    let env = vars(&[("USERPROFILE", "C:\\Users\\u")]);
    assert_eq!(
        resolve(&env, true, "XDG_CONFIG_HOME", "APPDATA", &[".config"]),
        Some(PathBuf::from("C:\\Users\\u").join(".config"))
    );
}

#[test]
fn nothing_set_is_none() {
    let env = vars(&[]);
    assert_eq!(resolve_home(&env, true), None);
    assert_eq!(
        resolve(&env, false, "XDG_CACHE_HOME", "LOCALAPPDATA", &[".cache"]),
        None
    );
}
