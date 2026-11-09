//! Running commands. Each command gets its own bash process wired directly
//! to the terminal, so interactive programs work normally. The working
//! directory and exported variables are saved after each command and restored
//! before the next, so `cd` and `export` persist as in one long shell session.

use crate::util;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Command;

const WRAPPER: &str = r#"
__far_state="$FAR_STATE_DIR"; __far_cmd="$1"; __far_pwd="$PWD"
[ -r "$__far_state/env" ] && . "$__far_state/env" 2>/dev/null
export PWD="$__far_pwd"
unset FAR_STATE_DIR __far_pwd
__far_save() {
    __far_rc=$?
    pwd > "$__far_state/cwd"
    export -p > "$__far_state/env" 2>/dev/null
    exit $__far_rc
}
trap __far_save EXIT
shopt -s expand_aliases 2>/dev/null
[ -r "$HOME/.bash_aliases" ] && . "$HOME/.bash_aliases" 2>/dev/null
eval "$__far_cmd"
"#;

/// Commands bash knows that are not files on `PATH`.
const BUILTINS: &[&str] = &[
    "alias", "bg", "break", "builtin", "cd", "command", "continue", "declare", "dirs", "echo",
    "eval", "exec", "exit", "export", "false", "fg", "hash", "help", "history", "jobs", "kill",
    "local", "popd", "printf", "pushd", "pwd", "read", "return", "set", "shift", "source", "test",
    "times", "trap", "true", "type", "ulimit", "umask", "unalias", "unset", "wait",
];

/// The word Tab completes.
#[derive(Debug, Default, PartialEq, Eq)]
struct Word {
    /// Character index where it starts.
    start: usize,
    /// Its text with quotes and escapes removed.
    value: String,
    /// It is where a command name goes.
    is_command: bool,
    /// The command it is an argument of, with quotes removed.
    command: String,
}

/// Finds the last word of a command line, following shell quoting.
fn last_word(line: &str) -> Word {
    let mut word = Word {
        is_command: true,
        ..Word::default()
    };
    let mut started = false;
    let mut words = 0;
    let mut quote = None;
    let mut escaped = false;
    for (i, c) in line.chars().enumerate() {
        if escaped {
            escaped = false;
            word.value.push(c);
            continue;
        }
        match quote {
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' => escaped = true,
            Some(_) => word.value.push(c),
            None => match c {
                '\\' | '\'' | '"' => {
                    if !started {
                        started = true;
                        word.start = i;
                    }
                    match c {
                        '\\' => escaped = true,
                        _ => quote = Some(c),
                    }
                }
                ' ' | '\t' | '|' | ';' | '&' | '(' | ')' | '<' | '>' => {
                    if started {
                        if words == 0 {
                            word.command = std::mem::take(&mut word.value);
                        }
                        words += 1;
                    }
                    if matches!(c, '|' | ';' | '&' | '(') {
                        words = 0;
                        word.command.clear();
                    }
                    started = false;
                    word.value.clear();
                    word.start = i + 1;
                }
                _ => {
                    if !started {
                        started = true;
                        word.start = i;
                    }
                    word.value.push(c);
                }
            },
        }
    }
    word.is_command = words == 0;
    word
}

/// Names on `PATH` and among the builtins that start with `prefix`.
fn command_matches(prefix: &str, path: Option<&std::ffi::OsStr>) -> Vec<String> {
    let mut found: Vec<String> = BUILTINS
        .iter()
        .filter(|b| b.starts_with(prefix))
        .map(|b| b.to_string())
        .collect();
    for dir in std::env::split_paths(path.unwrap_or_default()) {
        let Ok(items) = fs::read_dir(dir) else {
            continue;
        };
        for item in items.flatten() {
            let raw = item.file_name();
            let Some(name) = raw.to_str().filter(|n| n.starts_with(prefix)) else {
                continue;
            };
            let runnable = fs::metadata(item.path())
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
            if runnable {
                found.push(name.to_string());
            }
        }
    }
    found
}

/// Paths matching `typed`, spelled as typed. Folders end in `/`.
fn file_matches(typed: &str, cwd: &Path, dirs_only: bool) -> Vec<String> {
    let (folder, prefix) = match typed.rfind('/') {
        Some(i) => typed.split_at(i + 1),
        None => ("", typed),
    };
    let dir = if folder.is_empty() {
        cwd.to_path_buf()
    } else {
        util::resolve_path(cwd, folder)
    };
    let Ok(items) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for item in items.flatten() {
        let raw = item.file_name();
        // A name that is not text, or holds a line break, cannot be typed here.
        let Some(name) = raw
            .to_str()
            .filter(|n| n.starts_with(prefix) && !n.chars().any(char::is_control))
        else {
            continue;
        };
        if name.starts_with('.') && !prefix.starts_with('.') {
            continue;
        }
        let is_dir = fs::metadata(item.path()).is_ok_and(|m| m.is_dir());
        if dirs_only && !is_dir {
            continue;
        }
        found.push(format!("{folder}{name}{}", if is_dir { "/" } else { "" }));
    }
    found
}

fn common_prefix(items: &[String]) -> String {
    let Some(first) = items.first() else {
        return String::new();
    };
    let mut len = first.chars().count();
    for item in items {
        let same = first.chars().zip(item.chars());
        len = len.min(same.take_while(|(a, b)| a == b).count());
    }
    first.chars().take(len).collect()
}

/// Escapes text for bash. A leading `~` stays active, because bash expands it.
fn escape_word(text: &str) -> String {
    let mut out = String::new();
    for (i, c) in text.chars().enumerate() {
        let tilde = c == '~' && (i > 0 || text == "~" || text.starts_with("~/"));
        let plain = !c.is_ascii() || c.is_ascii_alphanumeric() || "_./+=:,@%-".contains(c) || tilde;
        if !plain {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// What Tab does to a command line.
#[derive(Debug, PartialEq, Eq)]
pub struct Completion {
    /// Character index where the replaced word starts.
    pub start: usize,
    /// What replaces the word, or `None` if there is nothing to add.
    pub text: Option<String>,
    /// Names to list when the word could not be extended, sorted.
    pub choices: Vec<String>,
}

/// Completes the last word of `line`: a command name at the start of a command,
/// and otherwise a path, as bash does.
pub fn complete(line: &str, cwd: &Path, path_var: Option<&std::ffi::OsStr>) -> Completion {
    let word = last_word(line);
    let mut matches = if word.is_command && !word.value.is_empty() && !word.value.contains('/') {
        command_matches(&word.value, path_var)
    } else {
        file_matches(&word.value, cwd, word.command == "cd")
    };
    matches.sort();
    matches.dedup();
    let common = common_prefix(&matches);
    let single = matches.len() == 1;
    let extended = common.chars().count() > word.value.chars().count();
    let text = if single {
        let end = if common.ends_with('/') { "" } else { " " };
        Some(escape_word(&common) + end)
    } else if extended {
        Some(escape_word(&common))
    } else {
        None
    };
    let choices = if text.is_none() {
        matches
            .iter()
            .map(|m| {
                let bare = m.trim_end_matches('/');
                let tail = bare.rsplit('/').next().unwrap_or(bare);
                format!("{tail}{}", if m.ends_with('/') { "/" } else { "" })
            })
            .collect()
    } else {
        Vec::new()
    };
    Completion {
        start: word.start,
        text,
        choices,
    }
}

/// Result of a finished command.
#[derive(Debug, PartialEq, Eq)]
pub struct Finished {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub cwd: Option<PathBuf>,
}

/// Keeps state between commands.
pub struct Shell {
    state: PathBuf,
}

impl Shell {
    /// Creates a private state directory: exported variables may hold secrets.
    pub fn new() -> io::Result<Self> {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        for n in 0..1000u32 {
            let state = base.join(format!(
                "far_linux_{}_{}_{n}",
                std::process::id(),
                util::now_secs()
            ));
            match fs::DirBuilder::new().mode(0o700).create(&state) {
                Ok(()) => return Ok(Shell { state }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "cannot create a state directory",
        ))
    }

    /// Runs a command attached to the terminal and waits for it.
    pub fn run(&self, command: &str, cwd: &Path) -> io::Result<Finished> {
        let cwd_file = self.state.join("cwd");
        let _ = fs::remove_file(&cwd_file);
        let mut child = Command::new("bash");
        child
            .arg("--noprofile")
            .arg("--norc")
            .arg("-c")
            .arg(WRAPPER)
            .arg("far")
            .arg(command)
            .current_dir(cwd);
        // After the first command the saved file is the whole environment,
        // which is what lets `unset` persist too.
        if self.state.join("env").exists() {
            child.env_clear();
            for name in ["HOME", "PATH"] {
                if let Some(value) = std::env::var_os(name) {
                    child.env(name, value);
                }
            }
        }
        child.env("FAR_STATE_DIR", &self.state).env("PWD", cwd);
        // SAFETY: only async-signal-safe signal() calls run between fork and
        // exec. Ctrl+Z stays ignored because nothing here could resume a
        // stopped job, and waiting for it would hang.
        unsafe {
            child.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGQUIT, libc::SIG_DFL);
                libc::signal(libc::SIGTSTP, libc::SIG_IGN);
                Ok(())
            });
        }
        // The terminal delivers Ctrl+C to the whole foreground group. Only the
        // command should die from it.
        let _guard = IgnoreInterrupts::new();
        let status = child.status()?;
        let cwd = fs::read_to_string(&cwd_file)
            .ok()
            .map(|text| PathBuf::from(text.trim_end_matches('\n')));
        Ok(Finished {
            code: status.code(),
            signal: status.signal(),
            cwd,
        })
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.state);
    }
}

struct IgnoreInterrupts {
    int: libc::sighandler_t,
    quit: libc::sighandler_t,
}

impl IgnoreInterrupts {
    fn new() -> Self {
        // SAFETY: installing SIG_IGN and keeping the previous dispositions.
        unsafe {
            IgnoreInterrupts {
                int: libc::signal(libc::SIGINT, libc::SIG_IGN),
                quit: libc::signal(libc::SIGQUIT, libc::SIG_IGN),
            }
        }
    }
}

impl Drop for IgnoreInterrupts {
    fn drop(&mut self) {
        // SAFETY: restoring dispositions returned by signal().
        unsafe {
            libc::signal(libc::SIGINT, self.int);
            libc::signal(libc::SIGQUIT, self.quit);
        }
    }
}

/// Whether `Enter` runs a file rather than opening it in an application: it
/// must have an execute bit and be a program, ELF or a `#!` script, or on WSL
/// a Windows program. Files on Windows drives all have execute bits, so the
/// bits alone are not enough.
pub fn runs_directly(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return false;
    }
    let mut head = [0u8; 4];
    let n = fs::File::open(path)
        .and_then(|mut f| io::Read::read(&mut f, &mut head))
        .unwrap_or(0);
    let head = &head[..n];
    head.starts_with(b"\x7fELF") || head.starts_with(b"#!") || (head.starts_with(b"MZ") && is_wsl())
}

/// Running under WSL, where Windows programs and associations are reachable.
pub fn is_wsl() -> bool {
    std::env::var_os("WSL_DISTRO_NAME").is_some()
        || Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists()
}

/// The command that opens `path` in the application registered for its
/// type: on WSL the Windows one, through `wslview` or Explorer, otherwise
/// `xdg-open` when a desktop is there. `None` when there is no way to.
pub fn opener(path: &Path, path_var: Option<&std::ffi::OsStr>) -> Option<Command> {
    let desktop =
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
    opener_for(path, path_var, is_wsl(), desktop)
}

/// `opener` for a given system: WSL or plain Linux, with or without a
/// desktop.
fn opener_for(
    path: &Path,
    path_var: Option<&std::ffi::OsStr>,
    wsl: bool,
    desktop: bool,
) -> Option<Command> {
    let found = |name: &str| {
        std::env::split_paths(path_var.unwrap_or_default())
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
    };
    if wsl {
        if let Some(tool) = found("wslview") {
            let mut command = Command::new(tool);
            command.arg(path);
            return Some(command);
        }
        let explorer = found("explorer.exe").or_else(|| {
            Some(PathBuf::from("/mnt/c/Windows/explorer.exe")).filter(|p| p.is_file())
        })?;
        let windows = Command::new(found("wslpath")?)
            .arg("-w")
            .arg(path)
            .output()
            .ok()?;
        let windows = String::from_utf8(windows.stdout).ok()?;
        let windows = windows.trim_end_matches('\n');
        if windows.is_empty() {
            return None;
        }
        let mut command = Command::new(explorer);
        command.arg(windows);
        return Some(command);
    }
    let tool = found("xdg-open").filter(|_| desktop)?;
    let mut command = Command::new(tool);
    command.arg(path);
    Some(command)
}

/// Starts `command` apart from far: no terminal, its own process group so
/// Ctrl+C does not reach it, and reaped on a thread.
pub fn launch(mut command: Command) -> io::Result<()> {
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet(shell: &Shell, cmd: &str, cwd: &Path) -> Finished {
        shell
            .run(&format!("{{ {cmd} ; }} >/dev/null"), cwd)
            .unwrap()
    }

    struct Tree(PathBuf);

    impl Tree {
        fn new(tag: &str, files: &[&str]) -> Tree {
            let root = std::env::temp_dir().join(format!(
                "far_complete_{tag}_{}_{}",
                std::process::id(),
                util::now_secs()
            ));
            let _ = fs::remove_dir_all(&root);
            for file in files {
                let path = root.join(file);
                if let Some(dir) = file.strip_suffix('/') {
                    fs::create_dir_all(root.join(dir)).unwrap();
                } else {
                    fs::create_dir_all(path.parent().unwrap()).unwrap();
                    fs::write(path, "").unwrap();
                }
            }
            Tree(root)
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn text(line: &str, tree: &Tree) -> Option<String> {
        complete(line, &tree.0, None).text
    }

    #[test]
    fn tab_completes_a_single_file_and_adds_a_space() {
        let t = Tree::new("single", &["readme.txt"]);
        let c = complete("cat rea", &t.0, None);
        assert_eq!((c.start, c.text.as_deref()), (4, Some("readme.txt ")));
    }

    #[test]
    fn tab_completes_a_folder_with_a_slash_and_keeps_the_path_typed() {
        let t = Tree::new("folder", &["src/main.rs", "src/lib.rs"]);
        assert_eq!(text("ls sr", &t), Some("src/".into()));
        let c = complete("ls src/m", &t.0, None);
        assert_eq!((c.start, c.text.as_deref()), (3, Some("src/main.rs ")));
    }

    #[test]
    fn tab_extends_to_the_common_part_then_lists_choices() {
        let t = Tree::new("common", &["report_a", "report_b", "other"]);
        assert_eq!(text("cat re", &t), Some("report_".into()));
        let c = complete("cat report_", &t.0, None);
        assert_eq!(c.text, None);
        assert_eq!(c.choices, ["report_a", "report_b"]);
    }

    #[test]
    fn completed_names_are_escaped_and_quoted_words_are_understood() {
        let t = Tree::new("escape", &["my file (1).txt", "тест файл"]);
        assert_eq!(text("cat my", &t), Some("my\\ file\\ \\(1\\).txt ".into()));
        assert_eq!(
            text("cat \"my fi", &t),
            Some("my\\ file\\ \\(1\\).txt ".into())
        );
        assert_eq!(
            text("cat my\\ fi", &t),
            Some("my\\ file\\ \\(1\\).txt ".into())
        );
        assert_eq!(text("cat те", &t), Some("тест\\ файл ".into()));
    }

    #[test]
    fn hidden_names_need_a_leading_dot() {
        let t = Tree::new("hidden", &[".secret", "seen"]);
        assert_eq!(text("cat ", &t), Some("seen ".into()));
        assert_eq!(text("cat .s", &t), Some(".secret ".into()));
    }

    #[test]
    fn cd_completes_only_folders() {
        let t = Tree::new("cd", &["data/", "data.txt"]);
        assert_eq!(text("cd da", &t), Some("data/".into()));
        assert_eq!(text("cat data", &t), None);
    }

    #[test]
    fn first_word_completes_commands_after_any_separator() {
        let t = Tree::new("cmd", &["bin/frobnicate", "frobfile"]);
        let tool = t.0.join("bin/frobnicate");
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let path = t.0.join("bin");
        let run = |line: &str| complete(line, &t.0, Some(path.as_os_str())).text;
        assert_eq!(run("frob"), Some("frobnicate ".into()));
        assert_eq!(run("ls | frob"), Some("frobnicate ".into()));
        assert_eq!(run("ls; frob"), Some("frobnicate ".into()));
        assert_eq!(run("cat frob"), Some("frobfile ".into()));
        assert_eq!(run("expor"), Some("export ".into()));
    }

    #[test]
    fn nothing_to_complete_changes_nothing() {
        let t = Tree::new("none", &["a"]);
        let c = complete("cat zzz", &t.0, None);
        assert_eq!((c.text, c.choices.len()), (None, 0));
        assert_eq!(complete("cat /no/such/dir/x", &t.0, None).text, None);
    }

    #[test]
    fn cd_persists_through_the_returned_directory() {
        let shell = Shell::new().unwrap();
        let done = quiet(&shell, "cd /usr", Path::new("/"));
        assert_eq!(done.cwd.as_deref(), Some(Path::new("/usr")));
        assert_eq!(done.code, Some(0));
    }

    #[test]
    fn exported_variables_persist_between_commands() {
        let shell = Shell::new().unwrap();
        quiet(&shell, "export FAR_TEST_VALUE='a b'", Path::new("/"));
        let done = quiet(&shell, "test \"$FAR_TEST_VALUE\" = 'a b'", Path::new("/"));
        assert_eq!(done.code, Some(0));
    }

    #[test]
    fn unset_persists_and_pwd_matches_the_directory_used() {
        let shell = Shell::new().unwrap();
        quiet(&shell, "export FAR_GONE=1", Path::new("/"));
        quiet(&shell, "unset FAR_GONE", Path::new("/"));
        let done = quiet(
            &shell,
            "test -z \"$FAR_GONE\" && test \"$PWD\" = /usr",
            Path::new("/usr"),
        );
        assert_eq!(done.code, Some(0));
    }

    #[test]
    fn exit_code_is_reported_and_state_still_saved() {
        let shell = Shell::new().unwrap();
        let done = shell.run("cd /usr; exit 3", Path::new("/")).unwrap();
        assert_eq!(done.code, Some(3));
        assert_eq!(done.cwd.as_deref(), Some(Path::new("/usr")));
    }

    #[test]
    fn state_directory_is_private_and_removed() {
        use std::os::unix::fs::MetadataExt;
        let shell = Shell::new().unwrap();
        let path = shell.state.clone();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
        drop(shell);
        assert!(!path.exists());
    }

    #[test]
    fn enter_runs_programs_and_scripts_but_not_files_with_only_execute_bits() {
        let dir = std::env::temp_dir().join(format!("far-runs-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, body: &[u8], mode: u32| {
            let p = dir.join(name);
            fs::write(&p, body).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
            p
        };
        assert!(runs_directly(&write(
            "script",
            b"#!/bin/sh\necho hi\n",
            0o755
        )));
        assert!(runs_directly(&write("program", b"\x7fELF\x02\x01", 0o755)));
        // A document with execute bits, as everything on a Windows drive.
        assert!(!runs_directly(&write("notes.txt", b"hello\n", 0o777)));
        assert!(!runs_directly(&write("plain", b"#!/bin/sh\n", 0o644)));
        assert!(!runs_directly(&write("empty", b"", 0o755)));
        assert!(!runs_directly(&dir));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn plain_linux_opens_with_xdg_open_only_on_a_desktop() {
        let dir = std::env::temp_dir().join(format!("far-opener-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for name in ["xdg-open", "wslview"] {
            fs::write(dir.join(name), "#!/bin/sh\n").unwrap();
        }
        let file = Path::new("/etc/hostname");
        let var = Some(dir.as_os_str());
        let program = |c: Command| c.get_program().to_owned();
        // Plain Linux on a desktop: xdg-open, never the WSL tool.
        let desktop = opener_for(file, var, false, true).map(program);
        assert_eq!(desktop, Some(dir.join("xdg-open").into_os_string()));
        // Over SSH or on the console there is nothing, so Enter views it.
        assert!(opener_for(file, var, false, false).is_none());
        // WSL prefers wslview, desktop or not.
        let wsl = opener_for(file, var, true, false).map(program);
        assert_eq!(wsl, Some(dir.join("wslview").into_os_string()));
        fs::remove_dir_all(&dir).unwrap();
    }
}
