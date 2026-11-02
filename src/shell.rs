//! Running commands. Each command gets its own bash process wired directly
//! to the terminal, so interactive programs work normally. The working
//! directory and exported variables are saved after each command and restored
//! before the next, so `cd` and `export` persist as in one long shell session.

use crate::util;
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet(shell: &Shell, cmd: &str, cwd: &Path) -> Finished {
        shell
            .run(&format!("{{ {cmd} ; }} >/dev/null"), cwd)
            .unwrap()
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
}
