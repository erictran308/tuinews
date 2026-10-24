//! The programs tuinews may start, found as absolute paths, and the
//! environment they get. tuinews runs none of its own: only tmux (to put a
//! pane's setting back) and, on Linux, xdg-open for your browser.
//!
//! A program is never named bare: `Command::new("tmux")` would search the
//! working directory on some systems, and an empty or relative `PATH` entry
//! everywhere, so one planted in a downloaded folder would run.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use crate::config;

/// Held while a program starts: one at a time.
static STARTING: Mutex<()> = Mutex::new(());

pub fn starting() -> MutexGuard<'static, ()> {
    STARTING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Folders looked in after `PATH`, where package managers put programs
/// that a GUI-started shell may not have on its `PATH`.
#[cfg(unix)]
const EXTRA_DIRS: [&str; 4] = [
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/run/current-system/sw/bin",
];
#[cfg(not(unix))]
const EXTRA_DIRS: [&str; 0] = [];

/// An absolute path to an existing file that this user can run.
pub fn usable(path: &Path) -> bool {
    if !path.is_absolute() || config::on_another_machine(path) {
        return false;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    meta.is_file()
}

/// `name` in the absolute folders on `PATH`, then in [`EXTRA_DIRS`].
pub fn on_path(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(EXTRA_DIRS.iter().map(PathBuf::from))
        .filter(|dir| dir.is_absolute() && !config::on_another_machine(dir))
        .map(|dir| dir.join(&exe))
        .find(|candidate| usable(candidate))
}

/// `PATH` with only its absolute, local folders; `None` when nothing is
/// left (an empty `PATH` would mean the working directory). tuinews's own
/// `PATH` is set to this at start, so a program that a dependency or a
/// child (xdg-open) starts by name can't come from the folder tuinews was
/// started in.
#[cfg(unix)]
pub fn absolute_path_entries(path: Option<OsString>) -> Option<OsString> {
    let kept: Vec<PathBuf> = std::env::split_paths(&path?)
        .filter(|dir| dir.is_absolute() && !config::on_another_machine(dir))
        .collect();
    if kept.is_empty() {
        return None;
    }
    std::env::join_paths(kept).ok()
}

/// Variables passed on to children: what a program needs to find its home,
/// its temporary folder and its language.
const KEPT: [&str; 12] = [
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "TZ",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "USERPROFILE",
];

/// The environment a child starts with: [`KEPT`] and a `PATH` of the
/// system's folders.
pub fn child_env() -> Vec<(OsString, OsString)> {
    child_env_from(|key| std::env::var_os(key))
}

fn child_env_from(var: impl Fn(&str) -> Option<OsString>) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = KEPT
        .iter()
        .filter_map(|&key| Some((key.into(), var(key)?)))
        .collect();
    #[cfg(unix)]
    let dirs: Vec<PathBuf> = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        .map(PathBuf::from)
        .into();
    #[cfg(not(unix))]
    let dirs: Vec<PathBuf> = var("SYSTEMROOT")
        .map(|root| vec![PathBuf::from(&root).join("System32"), PathBuf::from(root)])
        .unwrap_or_default();
    if !dirs.is_empty()
        && let Ok(path) = std::env::join_paths(dirs)
    {
        env.push(("PATH".into(), path));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn path_keeps_only_absolute_folders() {
        let clean = absolute_path_entries(Some(":.:bin:/usr/bin::/bin://host/x:".into()));
        assert_eq!(clean, Some("/usr/bin:/bin".into()));
        assert_eq!(absolute_path_entries(Some(".:bin:".into())), None);
        assert_eq!(absolute_path_entries(None), None);
    }

    #[test]
    fn relative_and_remote_paths_are_never_used() {
        assert!(!usable(Path::new("tmux")));
        assert!(!usable(Path::new("./tmux")));
        assert!(!usable(Path::new("//server/share/tmux")));
    }

    #[test]
    fn children_get_only_the_variables_kept() {
        let set = |key: &str| -> Option<OsString> {
            match key {
                "LD_PRELOAD" | "DYLD_INSERT_LIBRARIES" => Some("/nonexistent/evil".into()),
                "HOME" => Some("kept".into()),
                "PATH" => Some(".:relative:/usr/bin".into()),
                _ => None,
            }
        };
        let env = child_env_from(set);
        let keys: Vec<String> = env
            .iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert!(keys.contains(&"HOME".to_string()));
        assert!(!keys.contains(&"LD_PRELOAD".to_string()));
        let path = env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone());
        for dir in path.iter().flat_map(std::env::split_paths) {
            assert!(dir.is_absolute(), "the caller's PATH isn't used: {dir:?}");
        }
    }
}
