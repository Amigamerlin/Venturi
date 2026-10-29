//! Per-user folders: the XDG layout on Linux and macOS; on Windows, where
//! `HOME` is usually unset, `%APPDATA%`, `%LOCALAPPDATA%` and `%USERPROFILE%`.

use std::ffi::OsString;
use std::path::PathBuf;

/// The user's home folder.
pub fn home() -> Option<PathBuf> {
    resolve_home(&env, cfg!(windows))
}

/// Where settings go (`venturi/settings.json` under it).
pub fn config() -> Option<PathBuf> {
    resolve(
        &env,
        cfg!(windows),
        "XDG_CONFIG_HOME",
        "APPDATA",
        &[".config"],
    )
}

/// Where user data goes (the library shared by all projects).
pub fn data() -> Option<PathBuf> {
    resolve(
        &env,
        cfg!(windows),
        "XDG_DATA_HOME",
        "APPDATA",
        &[".local", "share"],
    )
}

/// Where caches go (proxies, waveforms, levels).
pub fn cache() -> Option<PathBuf> {
    resolve(
        &env,
        cfg!(windows),
        "XDG_CACHE_HOME",
        "LOCALAPPDATA",
        &[".cache"],
    )
}

fn env(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

/// Windows sets `USERPROFILE`; `HOME` only exists there under tools like Git Bash.
pub(crate) fn resolve_home(
    env: &dyn Fn(&str) -> Option<OsString>,
    windows: bool,
) -> Option<PathBuf> {
    env("HOME")
        .or_else(|| windows.then(|| env("USERPROFILE")).flatten())
        .map(PathBuf::from)
}

pub(crate) fn resolve(
    env: &dyn Fn(&str) -> Option<OsString>,
    windows: bool,
    xdg: &str,
    windows_var: &str,
    under_home: &[&str],
) -> Option<PathBuf> {
    env(xdg)
        .map(PathBuf::from)
        .or_else(|| {
            windows
                .then(|| env(windows_var))
                .flatten()
                .map(PathBuf::from)
        })
        .or_else(|| {
            resolve_home(env, windows)
                .map(|home| under_home.iter().fold(home, |path, part| path.join(part)))
        })
}

#[cfg(test)]
#[path = "tests/user_dirs.rs"]
mod tests;
