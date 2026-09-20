//! The `open-in-app` host route: `GET /open-in-app/apps`.
//!
//! Upstream (`dsh/packages/host/open-in-app/src/index.ts:202`) answers with
//! `{ apps: [...(await availability()).keys()] }`, where `availability()` is
//! `resolveOpenInAppApps(...)` memoized once per plugin life. The map's keys are
//! the catalog ids whose locator chain proved a launcher on this host, in
//! **catalog order** — `resolver.ts:669` resolves every entry through one
//! `Promise.all` and then inserts into a `Map` in array order, so
//! probe-completion order never reaches the wire.
//!
//! **Only the Linux half of the catalog is ported.** The catalog declares
//! `darwin`/`win32` specs for the same ids, and `resolver.ts:464` selects one
//! spec by host platform; vocoderd runs on Linux, so a `fixed`/`app`/`xcode`/
//! `scan`/`app-paths`/`install-record`/`github-desktop` locator can never be
//! reached here. Porting them would be untestable code asserted only by
//! construction, so they are omitted rather than stubbed.
//!
//! The launch args, `PATH_TOKEN` substitution, icon extraction, and the
//! `POST /open-in-app/open` route are likewise out of scope: the boot cell this
//! fixes fetches the app list and nothing else.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use axum::http::header;
use axum::response::{IntoResponse, Response};

/// One catalog entry's Linux locator chain, tried in order; the first locator
/// that proves a launcher wins (`resolver.ts:643`).
struct Entry {
    id: &'static str,
    locators: &'static [Locator],
}

/// The locator kinds reachable on Linux. `catalog.ts:52-85` declares nine; the
/// five Windows/macOS kinds are unreachable from this host (module doc).
enum Locator {
    /// Resolve the bare name on PATH through the subprocess capability
    /// (`resolver.ts:525`). `requires_desktop` carries `desktopCli`'s gate.
    Cli {
        name: &'static str,
        requires_desktop: bool,
    },
    /// First candidate that expands to an existing regular file
    /// (`resolver.ts:530`).
    File { candidates: &'static [&'static str] },
    /// Read the XDG entry, then verify its `TryExec`/`Exec` executable
    /// (`resolver.ts:607`).
    Desktop { desktop_id: &'static str },
}

const fn cli(name: &'static str) -> Locator {
    Locator::Cli {
        name,
        requires_desktop: false,
    }
}

/// `catalog.ts:126` — a PATH name that means nothing without a desktop session.
const fn desktop_cli(name: &'static str) -> Locator {
    Locator::Cli {
        name,
        requires_desktop: true,
    }
}

const fn desktop(desktop_id: &'static str) -> Locator {
    Locator::Desktop { desktop_id }
}

/// `catalog.ts:151-171` — a JetBrains entry's Linux chain: the product's PATH
/// command, then the Toolbox script, whose file name is the command name.
macro_rules! jetbrains {
    ($id:literal, $cli:literal) => {
        Entry {
            id: $id,
            locators: &[
                cli($cli),
                Locator::File {
                    candidates: &[concat!("~/.local/share/JetBrains/Toolbox/scripts/", $cli)],
                },
            ],
        }
    };
}

/// The Linux specs of `OPEN_IN_APP_CATALOG` (`catalog.ts:181-393`), in menu
/// order and with the platform-less entries omitted. Each row is exactly the
/// `linux:` field of the upstream entry of the same id; a row whose id has no
/// `linux` spec upstream (finder, xcode, the Git GUIs, the macOS terminals)
/// does not appear at all, because `specFor` (`resolver.ts:464`) returns
/// undefined for it and `resolveWithRegistry` (`resolver.ts:642`) then yields
/// null.
const CATALOG: &[Entry] = &[
    Entry {
        id: "filemanager",
        locators: &[desktop_cli("xdg-open")],
    },
    Entry {
        id: "cursor",
        locators: &[cli("cursor")],
    },
    Entry {
        id: "vscode",
        // `catalog.ts:227` carries `desktopId: 'code'` for its icon only; the
        // locator chain is the bare PATH name.
        locators: &[cli("code")],
    },
    Entry {
        id: "vscodeinsiders",
        locators: &[cli("code-insiders")],
    },
    Entry {
        id: "windsurf",
        locators: &[cli("windsurf")],
    },
    Entry {
        id: "zed",
        locators: &[cli("zed"), desktop("dev.zed.Zed")],
    },
    Entry {
        id: "sublimetext",
        locators: &[cli("subl")],
    },
    Entry {
        id: "androidstudio",
        locators: &[
            cli("studio"),
            Locator::File {
                candidates: &[
                    "~/.local/share/JetBrains/Toolbox/scripts/studio",
                    "/opt/android-studio/bin/studio.sh",
                ],
            },
        ],
    },
    jetbrains!("intellij", "idea"),
    jetbrains!("pycharm", "pycharm"),
    jetbrains!("webstorm", "webstorm"),
    jetbrains!("phpstorm", "phpstorm"),
    jetbrains!("goland", "goland"),
    jetbrains!("rider", "rider"),
    jetbrains!("rustrover", "rustrover"),
    Entry {
        id: "sublimemerge",
        locators: &[cli("smerge")],
    },
    Entry {
        id: "ghostty",
        locators: &[cli("ghostty"), desktop("com.mitchellh.ghostty")],
    },
    Entry {
        id: "kitty",
        locators: &[cli("kitty"), desktop("kitty")],
    },
    Entry {
        id: "gnometerminal",
        locators: &[cli("gnome-terminal"), desktop("org.gnome.Terminal")],
    },
    Entry {
        id: "konsole",
        locators: &[cli("konsole"), desktop("org.kde.konsole")],
    },
];

/// The host facts resolution reads, held as plain values so every helper below
/// is a pure function of its arguments. `resolver.ts:135` does the same
/// defaulting once at each public entry; there is exactly one public entry here
/// ([`apps`]), so the defaulting lives in [`Facts::detect`].
pub struct Facts {
    env: HashMap<String, String>,
    home: String,
    /// Directory relative PATH entries resolve against — `subprocess-local`
    /// resolves each candidate with `resolve(process.cwd(), directory, name)`
    /// (`packages/subprocess/subprocess-local/src/index.ts:145`).
    cwd: PathBuf,
    /// Kernel release, for the WSL marker (`native-command/src/path-opener.ts:99`).
    os_release: String,
}

impl Facts {
    /// Read this process's environment once. Upstream's SSH fact comes from the
    /// launcher snapshot's `process` source (`launch-environment/src/index.ts:125`),
    /// which is the inherited environment — exactly `std::env::vars`.
    pub fn detect() -> Self {
        let kernel_release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        Self {
            env: std::env::vars().collect(),
            home: std::env::var("HOME").unwrap_or_default(),
            cwd: std::env::current_dir().unwrap_or_default(),
            os_release: kernel_release,
        }
    }
}

/// Whether one environment marker is set to a non-empty value
/// (`path-opener.ts:91`); an empty `SSH_CONNECTION` is not an SSH launch.
fn present(value: Option<&String>) -> bool {
    value.is_some_and(|v| !v.is_empty())
}

/// `launchedThroughSsh` (`launch-environment/src/index.ts:125-130`).
fn launched_through_ssh(facts: &Facts) -> bool {
    present(facts.env.get("SSH_CONNECTION")) || present(facts.env.get("SSH_TTY"))
}

/// `isWsl` (`path-opener.ts:96-100`): a WSL launch reaches the Windows desktop,
/// so its `xdg-open` is meaningful without a local display server.
fn is_wsl(facts: &Facts) -> bool {
    if present(facts.env.get("WSL_DISTRO_NAME")) || present(facts.env.get("WSL_INTEROP")) {
        return true;
    }
    facts.os_release.to_lowercase().contains("microsoft")
}

/// `canOpenNativePath` on Linux (`path-opener.ts:168-174`). macOS and Windows
/// always answer true upstream; on this host the platform is Linux by
/// construction, so the constant-true arms are not ported.
fn can_open_native_path(facts: &Facts) -> bool {
    is_wsl(facts) || present(facts.env.get("DISPLAY")) || present(facts.env.get("WAYLAND_DISPLAY"))
}

/// `expandCandidate` (`resolver.ts:216-225`): `${VAR}` substitution that
/// retreats on any unset variable, then a leading `~/`. Substitution is by
/// plain string replacement, so the expanded value's own `/` separators land in
/// the result unchanged.
pub fn expand_candidate(template: &str, facts: &Facts) -> Option<String> {
    let mut unset = false;
    let mut expanded = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        expanded.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // No closing brace: upstream's `/\$\{([^}]+)\}/g` cannot match, so
            // the token is literal text and stays as written.
            expanded.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let name = &after[..end];
        match facts.env.get(name) {
            Some(value) => expanded.push_str(value),
            None => {
                unset = true;
                // Keep scanning: upstream collects every unset name and bails
                // once, but the result is the same null.
                expanded.push_str(&rest[start..start + 2 + end + 1]);
            }
        }
        rest = &after[end + 1..];
    }
    expanded.push_str(rest);
    if unset {
        return None;
    }
    Some(match expanded.strip_prefix("~/") {
        Some(tail) => Path::new(&facts.home)
            .join(tail)
            .to_string_lossy()
            .into_owned(),
        None => expanded,
    })
}

/// Resolve a bare name on PATH the way the subprocess capability does
/// (`subprocess-local/src/index.ts:122-153`): split PATH, resolve each directory
/// against the cwd, keep the first entry that is a regular file with an execute
/// bit. No shell, no `which`.
fn resolve_on_path(name: &str, facts: &Facts) -> Option<String> {
    let path = facts.env.get("PATH").map(String::as_str).unwrap_or("");
    for directory in path.split(':') {
        // `path.resolve(cwd, directory, name)`: an absolute directory ignores
        // the cwd, and an empty PATH entry is the cwd itself.
        let candidate = if directory.is_empty() {
            facts.cwd.join(name)
        } else {
            let dir = Path::new(directory);
            if dir.is_absolute() {
                dir.join(name)
            } else {
                facts.cwd.join(dir).join(name)
            }
        };
        if is_executable_file(&candidate) {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

/// `stat().isFile()` via `path.resolve`-normalized candidates. `std::fs::metadata`
/// follows symlinks, as Node's `stat` does.
fn is_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file())
        .unwrap_or(false)
}

/// The subprocess capability's `stat` + `access(X_OK)` pair. `access` is not in
/// `std`; the execute bits are the same predicate for a non-root caller on
/// Linux, which is the only platform this module serves.
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Fields of one parsed XDG desktop entry this route needs. Only `Exec` and
/// `TryExec` matter here; `Icon` is the icon route's.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DesktopEntry {
    pub exec: Option<String>,
    pub try_exec: Option<String>,
}

/// `parseDesktopEntry` (`resolver.ts:381-400`): read only the `[Desktop Entry]`
/// section, keep `Exec`/`TryExec`, and let a repeated key win with its last
/// value. Every other section header (and every key before one) is ignored.
pub fn parse_desktop_entry(text: &str) -> DesktopEntry {
    let mut entry = DesktopEntry::default();
    let mut in_entry = false;
    for line in text.split('\n') {
        let trimmed = line.trim_end_matches('\r').trim();
        if trimmed.starts_with('[') {
            in_entry = trimmed == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some(separator) = trimmed.find('=') else {
            continue;
        };
        let key = trimmed[..separator].trim();
        let value = trimmed[separator + 1..].trim();
        match key {
            "Exec" => entry.exec = Some(value.to_string()),
            "TryExec" => entry.try_exec = Some(value.to_string()),
            _ => {}
        }
    }
    entry
}

/// `execCommand` (`resolver.ts:450-456`): the `Exec=` value's first token, a
/// leading `"..."` run when quoted and otherwise the run up to the first
/// whitespace.
pub fn exec_command(exec: Option<&str>) -> Option<String> {
    let exec = exec?;
    // `^"([^"]+)"` needs at least one character between the quotes; an empty
    // quoted run is not a match and falls through to the bare read.
    if let Some(rest) = exec.strip_prefix('"')
        && let Some(end) = rest.find('"')
        && end > 0
    {
        return Some(rest[..end].to_string());
    }
    let bare: String = exec.chars().take_while(|c| !c.is_whitespace()).collect();
    (!bare.is_empty()).then_some(bare)
}

/// `xdgDataDirectories` (`resolver.ts:407-411`): `XDG_DATA_HOME` (or
/// `~/.local/share`), then `XDG_DATA_DIRS` (or the freedesktop default), with
/// empty colon-separated entries dropped.
pub fn xdg_data_directories(facts: &Facts) -> Vec<String> {
    let data_home = match facts.env.get("XDG_DATA_HOME") {
        Some(dir) => dir.clone(),
        None => Path::new(&facts.home)
            .join(".local/share")
            .to_string_lossy()
            .into_owned(),
    };
    let data_dirs = facts
        .env
        .get("XDG_DATA_DIRS")
        .cloned()
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    let mut dirs = vec![data_home];
    dirs.extend(
        data_dirs
            .split(':')
            .filter(|dir| !dir.is_empty())
            .map(str::to_string),
    );
    dirs
}

/// `findDesktopEntry` (`resolver.ts:419-431`): walk the data directories and
/// return the first readable `<dir>/applications/<id>.desktop`, even when it
/// parses to nothing. A byte sequence that is not UTF-8 cannot make `readFile`
/// throw upstream, so replacement characters are the faithful read here rather
/// than a skipped directory.
fn find_desktop_entry(desktop_id: &str, facts: &Facts) -> Option<DesktopEntry> {
    for data_dir in xdg_data_directories(facts) {
        let path = Path::new(&data_dir)
            .join("applications")
            .join(format!("{desktop_id}.desktop"));
        if let Ok(bytes) = std::fs::read(&path) {
            return Some(parse_desktop_entry(&String::from_utf8_lossy(&bytes)));
        }
    }
    None
}

/// `desktopLauncher` (`resolver.ts:438-443`): `TryExec` when present, else
/// `Exec`'s first token; an absolute path verifies on disk and a bare name
/// resolves on PATH.
fn desktop_launcher(entry: &DesktopEntry, facts: &Facts) -> Option<String> {
    let candidate = entry
        .try_exec
        .clone()
        .or_else(|| exec_command(entry.exec.as_deref()))?;
    if candidate.is_empty() {
        return None;
    }
    let path = Path::new(&candidate);
    if path.is_absolute() {
        return is_file(path).then_some(candidate);
    }
    resolve_on_path(&candidate, facts)
}

/// The `file` locator's body (`resolver.ts:530-538`), split from [`locate`] so
/// tests can pass borrowed candidates.
fn file_candidates(candidates: &[&str], facts: &Facts) -> bool {
    candidates.iter().any(|candidate| {
        expand_candidate(candidate, facts).is_some_and(|path| is_file(Path::new(&path)))
    })
}

/// `locate` for the three Linux kinds (`resolver.ts:476-616`): resolve one
/// locator to a verified launcher, or null when it proves nothing.
fn locate(locator: &Locator, facts: &Facts) -> bool {
    match locator {
        Locator::Cli {
            name,
            requires_desktop,
        } => {
            if *requires_desktop && !can_open_native_path(facts) {
                return false;
            }
            resolve_on_path(name, facts).is_some()
        }
        Locator::File { candidates } => file_candidates(candidates, facts),
        Locator::Desktop { desktop_id } => find_desktop_entry(desktop_id, facts)
            .is_some_and(|entry| desktop_launcher(&entry, facts).is_some()),
    }
}

/// The catalog ids available on this host, in catalog order.
///
/// Split from [`apps`] so tests inject host facts instead of reading this
/// machine's environment and installed applications.
pub fn apps_with(facts: &Facts) -> Vec<String> {
    // An SSH launch offers no GUI: upstream returns the empty map before
    // probing anything (`resolver.ts:665-667`), so no locator runs.
    if launched_through_ssh(facts) {
        return Vec::new();
    }
    CATALOG
        .iter()
        .filter(|entry| entry.locators.iter().any(|locator| locate(locator, facts)))
        .map(|entry| entry.id.to_string())
        .collect()
}

/// The resolved app list, memoized once per process.
///
/// Upstream resolves lazily on the first request that needs the catalog and
/// holds the map for the plugin's life (`index.ts:155-157`), so a page reload
/// never re-runs detection. A `OnceLock` is that fact: resolution touches the
/// filesystem and PATH, and the answer is a property of the host, not of the
/// request.
pub fn apps() -> Vec<String> {
    static RESOLVED: OnceLock<Vec<String>> = OnceLock::new();
    RESOLVED.get_or_init(|| apps_with(&Facts::detect())).clone()
}

/// The `GET /open-in-app/apps` response: `{"apps":[...]}`, JSON, `no-store`
/// (`index.ts:92-97` and `:202`).
pub async fn handler() -> Response {
    let body = serde_json::json!({ "apps": apps() }).to_string();
    (
        [
            (
                header::CONTENT_TYPE,
                "application/json; charset=utf-8".to_string(),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Facts with an explicit environment; `home` and the data directories are
    /// supplied per test so nothing here depends on this host's installs.
    fn facts(vars: &[(&str, &str)]) -> Facts {
        Facts {
            env: vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            home: "/home/tester".to_string(),
            cwd: PathBuf::from("/work"),
            os_release: "6.9.0-arch1-1".to_string(),
        }
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).expect("parent");
        std::fs::write(path, contents).expect("write");
    }

    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    // ---- parse_desktop_entry ------------------------------------------------

    #[test]
    fn desktop_entry_reads_only_the_desktop_entry_section() {
        let entry = parse_desktop_entry(
            "[Desktop Entry]\n\
             Type=Application\n\
             TryExec=/usr/bin/zeditor\n\
             Exec=zeditor %U\n\
             Icon=zed\n\
             \n\
             [Desktop Action New]\n\
             Exec=zeditor --new %U\n",
        );
        assert_eq!(entry.try_exec.as_deref(), Some("/usr/bin/zeditor"));
        // The action section's Exec must not overwrite the entry's own.
        assert_eq!(entry.exec.as_deref(), Some("zeditor %U"));
    }

    #[test]
    fn desktop_entry_takes_the_last_repeated_key_and_ignores_unknown_keys() {
        let entry = parse_desktop_entry("[Desktop Entry]\nExec=first\nExec=second\nComment=hi\n");
        assert_eq!(entry.exec.as_deref(), Some("second"));
        assert_eq!(entry.try_exec, None);
    }

    #[test]
    fn desktop_entry_before_any_section_reads_nothing() {
        let entry = parse_desktop_entry("Exec=stray\n[Other]\nTryExec=stray\n");
        assert_eq!(entry, DesktopEntry::default());
    }

    #[test]
    fn desktop_entry_tolerates_crlf_and_absent_values() {
        let entry = parse_desktop_entry("[Desktop Entry]\r\nExec=zeditor %U\r\nBare\r\n");
        assert_eq!(entry.exec.as_deref(), Some("zeditor %U"));
        assert_eq!(entry.try_exec, None);
    }

    // ---- exec_command -------------------------------------------------------

    #[test]
    fn exec_command_reads_a_quoted_path_with_spaces() {
        assert_eq!(
            exec_command(Some("\"/opt/My App/bin/app\" --flag")).as_deref(),
            Some("/opt/My App/bin/app")
        );
    }

    #[test]
    fn exec_command_reads_the_bare_first_run() {
        assert_eq!(
            exec_command(Some("zeditor --new %U")).as_deref(),
            Some("zeditor")
        );
        // `^\S+` is anchored, so a leading space is not a token at all.
        assert_eq!(exec_command(Some("  spaced\targ")), None);
    }

    #[test]
    fn exec_command_is_none_when_absent_or_blank() {
        assert_eq!(exec_command(None), None);
        assert_eq!(exec_command(Some("   ")), None);
        // An unterminated quote is not a quoted run; the bare run is `"x`.
        assert_eq!(
            exec_command(Some("\"unterminated")).as_deref(),
            Some("\"unterminated")
        );
    }

    // ---- expand_candidate ---------------------------------------------------

    #[test]
    fn expand_candidate_substitutes_variables() {
        let facts = facts(&[("ProgramFiles", "/opt/apps")]);
        assert_eq!(
            expand_candidate("${ProgramFiles}/Toolbox/scripts/idea", &facts).as_deref(),
            Some("/opt/apps/Toolbox/scripts/idea")
        );
    }

    #[test]
    fn expand_candidate_retreats_on_an_unset_variable() {
        let facts = facts(&[("HOME", "/home/tester")]);
        assert_eq!(expand_candidate("${NOPE}/bin/x", &facts), None);
        // One unset name among many is still a retreat.
        assert_eq!(expand_candidate("${HOME}/${NOPE}", &facts), None);
    }

    #[test]
    fn expand_candidate_expands_a_leading_tilde() {
        let facts = facts(&[]);
        assert_eq!(
            expand_candidate("~/.local/share/JetBrains/Toolbox/scripts/studio", &facts).as_deref(),
            Some("/home/tester/.local/share/JetBrains/Toolbox/scripts/studio")
        );
        // Only a leading `~/` expands; one mid-path stays literal.
        assert_eq!(
            expand_candidate("/opt/~/x", &facts).as_deref(),
            Some("/opt/~/x")
        );
    }

    #[test]
    fn expand_candidate_leaves_an_unclosed_token_literal() {
        let facts = facts(&[("HOME", "/home/tester")]);
        assert_eq!(
            expand_candidate("/a/${HOME/x", &facts).as_deref(),
            Some("/a/${HOME/x")
        );
    }

    // ---- xdg_data_directories ----------------------------------------------

    #[test]
    fn xdg_dirs_default_to_the_freedesktop_paths() {
        let facts = facts(&[]);
        assert_eq!(
            xdg_data_directories(&facts),
            vec![
                "/home/tester/.local/share".to_string(),
                "/usr/local/share".to_string(),
                "/usr/share".to_string(),
            ]
        );
    }

    #[test]
    fn xdg_dirs_honour_the_environment_and_drop_empty_entries() {
        let facts = facts(&[
            ("XDG_DATA_HOME", "/data/home"),
            ("XDG_DATA_DIRS", "/a::/b:"),
        ]);
        assert_eq!(
            xdg_data_directories(&facts),
            vec!["/data/home".to_string(), "/a".to_string(), "/b".to_string()]
        );
    }

    // ---- PATH resolution ----------------------------------------------------

    #[test]
    fn path_resolution_finds_an_executable_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        let exe = bin.join("xdg-open");
        write(&exe, "#!/bin/sh\n");
        make_executable(&exe);
        let facts = facts(&[("PATH", bin.to_str().unwrap())]);
        assert_eq!(
            resolve_on_path("xdg-open", &facts).as_deref(),
            Some(exe.to_str().unwrap())
        );
    }

    #[test]
    fn path_resolution_skips_a_name_that_is_absent_or_not_executable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        let plain = bin.join("code");
        write(&plain, "not executable\n");
        std::fs::create_dir_all(bin.join("dircode")).expect("dir");
        let facts = facts(&[("PATH", bin.to_str().unwrap())]);
        assert_eq!(resolve_on_path("code", &facts), None);
        assert_eq!(resolve_on_path("absent", &facts), None);
        // A directory with the matching name is not a launcher.
        assert_eq!(resolve_on_path("dircode", &facts), None);
    }

    #[test]
    fn path_resolution_takes_the_first_match_across_entries() {
        let first = tempfile::tempdir().expect("tempdir");
        let second = tempfile::tempdir().expect("tempdir");
        let a = first.path().join("tool");
        let b = second.path().join("tool");
        write(&a, "#!/bin/sh\n");
        write(&b, "#!/bin/sh\n");
        make_executable(&a);
        make_executable(&b);
        let facts = facts(&[(
            "PATH",
            &format!("{}:{}", first.path().display(), second.path().display()),
        )]);
        assert_eq!(
            resolve_on_path("tool", &facts).as_deref(),
            Some(a.to_str().unwrap())
        );
    }

    // ---- desktop-entry resolution -------------------------------------------

    #[test]
    fn a_desktop_entry_resolves_through_its_try_exec() {
        let home = tempfile::tempdir().expect("tempdir");
        let bin = home.path().join("bin");
        let exe = bin.join("zeditor");
        write(&exe, "#!/bin/sh\n");
        make_executable(&exe);
        write(
            &home
                .path()
                .join(".local/share/applications/dev.zed.Zed.desktop"),
            "[Desktop Entry]\nTryExec=zeditor\nExec=zeditor %U\n",
        );
        let mut f = facts(&[("PATH", bin.to_str().unwrap()), ("XDG_DATA_DIRS", "")]);
        f.home = home.path().to_str().unwrap().to_string();
        assert!(locate(&desktop("dev.zed.Zed"), &f));
        // The id is only found in the directory the walk reaches.
        assert!(!locate(&desktop("kitty"), &f));
    }

    #[test]
    fn a_desktop_entry_without_an_executable_launcher_proves_nothing() {
        let home = tempfile::tempdir().expect("tempdir");
        write(
            &home.path().join(".local/share/applications/kitty.desktop"),
            "[Desktop Entry]\nExec=kitty --directory\n",
        );
        let mut f = facts(&[("PATH", "/nonexistent-bin"), ("XDG_DATA_DIRS", "")]);
        f.home = home.path().to_str().unwrap().to_string();
        assert!(!locate(&desktop("kitty"), &f));
    }

    #[test]
    fn a_desktop_entry_is_read_from_the_first_directory_that_holds_it() {
        // Precedence: XDG_DATA_HOME wins over XDG_DATA_DIRS, and the first
        // readable file is parsed even when it yields no launcher.
        let home = tempfile::tempdir().expect("tempdir");
        let share = tempfile::tempdir().expect("tempdir");
        let bin = home.path().join("bin");
        let exe = bin.join("from-share");
        write(&exe, "#!/bin/sh\n");
        make_executable(&exe);
        write(
            &home.path().join(".local/share/applications/kitty.desktop"),
            "[Desktop Entry]\nComment=no launcher here\n",
        );
        write(
            &share.path().join("applications/kitty.desktop"),
            "[Desktop Entry]\nExec=from-share\n",
        );
        let mut f = facts(&[
            ("PATH", bin.to_str().unwrap()),
            ("XDG_DATA_DIRS", share.path().to_str().unwrap()),
        ]);
        f.home = home.path().to_str().unwrap().to_string();
        assert!(!locate(&desktop("kitty"), &f));

        // With the shadowing file gone the later directory is reached.
        std::fs::remove_file(home.path().join(".local/share/applications/kitty.desktop"))
            .expect("remove");
        assert!(locate(&desktop("kitty"), &f));
    }

    // ---- the gates ----------------------------------------------------------

    #[test]
    fn ssh_launches_offer_no_apps_without_probing() {
        let over_ssh = facts(&[("SSH_CONNECTION", "10.0.0.1 1 10.0.0.2 2")]);
        assert!(launched_through_ssh(&over_ssh));
        assert!(apps_with(&over_ssh).is_empty());
        // An empty marker is not an SSH launch, and neither marker alone is.
        assert!(!launched_through_ssh(&facts(&[("SSH_TTY", "")])));
        assert!(!launched_through_ssh(&facts(&[("SSH_CONNECTION", "")])));
    }

    #[test]
    fn a_headless_host_offers_no_filemanager() {
        let headless = facts(&[("PATH", "/usr/bin")]);
        assert!(!can_open_native_path(&headless));
        // A DISPLAY, WAYLAND_DISPLAY, or WSL marker each suffice.
        assert!(can_open_native_path(&facts(&[("DISPLAY", ":0")])));
        assert!(can_open_native_path(&facts(&[(
            "WAYLAND_DISPLAY",
            "wayland-0"
        )])));
        assert!(can_open_native_path(&facts(&[(
            "WSL_INTEROP",
            "/run/WSL/1"
        )])));
        let mut microsoft = facts(&[("DISPLAY", ":0")]);
        microsoft.os_release = "5.15.90.1-microsoft-standard-WSL2".to_string();
        assert!(is_wsl(&microsoft));
    }

    #[test]
    fn a_cli_locator_carrying_requires_desktop_is_gated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join("xdg-open");
        write(&exe, "#!/bin/sh\n");
        make_executable(&exe);
        let path = dir.path().to_str().unwrap();
        assert!(!locate(&desktop_cli("xdg-open"), &facts(&[("PATH", path)])));
        assert!(locate(
            &desktop_cli("xdg-open"),
            &facts(&[("PATH", path), ("DISPLAY", ":0")])
        ));
        // The gate belongs to the locator, not to `cli` in general.
        assert!(locate(&cli("xdg-open"), &facts(&[("PATH", path)])));
    }

    #[test]
    fn a_file_locator_takes_the_first_existing_candidate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let second = dir.path().join("studio.sh");
        let third = dir.path().join("studio");
        write(&second, "#!/bin/sh\n");
        // The first candidate expands to nothing (unset variable) and drops
        // out; the second is a file but the third name matches no file either,
        // so the second is what a winning chain would carry.
        let candidates: Vec<String> = vec![
            "${MISSING_VAR}/studio".to_string(),
            second.to_str().unwrap().to_string(),
            third.to_str().unwrap().to_string(),
        ];
        let borrowed: Vec<&str> = candidates.iter().map(String::as_str).collect();
        assert!(file_candidates(&borrowed, &facts(&[])));

        // Every candidate unset or missing proves nothing.
        let none: Vec<&str> = vec!["${MISSING_VAR}/studio", "/nonexistent/bin/x"];
        assert!(!file_candidates(&none, &facts(&[])));
    }

    #[test]
    fn apps_are_reported_in_catalog_order() {
        // Every id in the returned list must appear in `CATALOG` order, which
        // is what the client renders; the ids present depend on the host. The
        // XDG directories point at an empty temp tree so a desktop locator
        // cannot reach this machine's installed entries.
        let dir = tempfile::tempdir().expect("tempdir");
        let empty_share = tempfile::tempdir().expect("tempdir");
        for name in ["konsole", "code"] {
            let exe = dir.path().join(name);
            write(&exe, "#!/bin/sh\n");
            make_executable(&exe);
        }
        let facts = facts(&[
            ("PATH", dir.path().to_str().unwrap()),
            ("XDG_DATA_HOME", empty_share.path().to_str().unwrap()),
            ("XDG_DATA_DIRS", ":"),
        ]);
        let ids = apps_with(&facts);
        assert_eq!(ids, vec!["vscode".to_string(), "konsole".to_string()]);
        let positions: Vec<usize> = ids
            .iter()
            .map(|id| CATALOG.iter().position(|entry| entry.id == id).unwrap())
            .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]));
    }
}
