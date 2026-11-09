//! End-to-end tests: they run the real binary inside tmux, press real keys
//! and read the real screen. They are skipped when tmux is not installed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU32 = AtomicU32::new(0);

struct Session {
    socket: String,
    root: PathBuf,
}

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
}

impl Session {
    /// Starts far in a private tmux server with panels on `left` and `right`.
    fn start(setup: impl FnOnce(&Path)) -> Option<Session> {
        Self::start_with("", setup)
    }

    /// Starts far with extra `env` arguments, such as `NAME=value`.
    fn start_with(extra_env: &str, setup: impl FnOnce(&Path)) -> Option<Session> {
        if !tmux_available() {
            eprintln!("tmux not found: skipping");
            return None;
        }
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let socket = format!("far_e2e_{}_{id}", std::process::id());
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(&socket);
        let _ = fs::remove_dir_all(&root);
        for dir in ["left", "right", "home"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        setup(&root);
        let session = Session { socket, root };
        let launch = format!(
            "env HOME={home} XDG_DATA_HOME={home}/data XDG_CONFIG_HOME={home}/config PS1= {extra_env} {bin} {left} {right}; sleep 30",
            home = session.root.join("home").display(),
            bin = env!("CARGO_BIN_EXE_far"),
            left = session.root.join("left").display(),
            right = session.root.join("right").display(),
        );
        session.tmux(&[
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-x",
            "100",
            "-y",
            "30",
            "-s",
            "t",
            &launch,
        ]);
        session.tmux(&["set", "-s", "escape-time", "0"]);
        session.wait("left");
        Some(session)
    }

    fn tmux(&self, args: &[&str]) -> String {
        let output = Command::new("tmux")
            .args(["-L", &self.socket])
            .args(args)
            .output()
            .expect("tmux runs");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn keys(&self, keys: &[&str]) {
        for key in keys {
            self.tmux(&["send-keys", "-t", "t", key]);
        }
    }

    fn text(&self, text: &str) {
        self.tmux(&["send-keys", "-t", "t", "-l", text]);
    }

    fn screen(&self) -> String {
        self.tmux(&["capture-pane", "-p", "-t", "t"])
    }

    fn wait(&self, wanted: &str) {
        self.wait_for(wanted, |s| s.contains(wanted));
    }

    fn wait_gone(&self, unwanted: &str) {
        self.wait_for(unwanted, |s| !s.contains(unwanted));
    }

    fn wait_for(&self, label: &str, check: impl Fn(&str) -> bool) {
        let start = Instant::now();
        loop {
            let screen = self.screen();
            if check(&screen) {
                return;
            }
            if start.elapsed() > Duration::from_secs(8) {
                panic!("timed out on {label:?}. Screen:\n{screen}");
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    fn wait_file(&self, path: &Path, exists: bool) {
        let start = Instant::now();
        while path.exists() != exists {
            if start.elapsed() > Duration::from_secs(8) {
                panic!(
                    "{} exists={exists} not reached.\n{}",
                    path.display(),
                    self.screen()
                );
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.tmux(&["kill-server"]);
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn with_file(root: &Path) {
    fs::write(root.join("left/alpha.txt"), "alpha contents\nsecond line\n").unwrap();
}

#[test]
fn panels_show_files_and_f10_quits() {
    let Some(s) = Session::start(with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.wait("right");
    assert!(s.screen().contains("Copy"), "key bar:\n{}", s.screen());
    s.keys(&["F10"]);
    s.wait("Do you want to quit?");
    s.keys(&["Enter"]);
    s.wait_gone("alpha.txt");
}

#[test]
fn f5_copies_to_the_other_panel() {
    let Some(s) = Session::start(with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.keys(&["Down", "F5"]);
    s.wait("Copy alpha.txt to:");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("right/alpha.txt"), true);
    assert_eq!(
        fs::read_to_string(s.p("right/alpha.txt")).unwrap(),
        "alpha contents\nsecond line\n"
    );
    assert!(s.p("left/alpha.txt").exists());
}

#[test]
fn copying_over_a_file_asks_first_and_skip_keeps_it() {
    let Some(s) = Session::start(|root| {
        with_file(root);
        fs::write(root.join("right/alpha.txt"), "existing").unwrap();
    }) else {
        return;
    };
    s.wait("alpha.txt");
    s.keys(&["Down", "F5"]);
    s.wait("Copy alpha.txt to:");
    s.keys(&["Enter"]);
    s.wait("already exists");
    s.keys(&["s"]);
    s.wait_gone("already exists");
    assert_eq!(
        fs::read_to_string(s.p("right/alpha.txt")).unwrap(),
        "existing"
    );
}

#[test]
fn f6_moves_and_shift_f6_renames() {
    let Some(s) = Session::start(with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.keys(&["Down", "S-F6"]);
    s.wait("Rename or move alpha.txt to:");
    s.text("beta.txt");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("left/beta.txt"), true);
    // The name also appears in the dialogs, and keys pressed while an
    // operation runs are ignored: wait until the panels are back.
    s.wait_for("panels after rename", |t| {
        t.contains("beta.txt") && !t.contains("Rename or move") && !t.contains("Esc cancels")
    });
    assert!(!s.p("left/alpha.txt").exists());
    s.keys(&["F6"]);
    s.wait("Rename or move beta.txt to:");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("right/beta.txt"), true);
    assert!(!s.p("left/beta.txt").exists());
}

#[test]
fn f7_makes_a_folder() {
    let Some(s) = Session::start(|_| {}) else {
        return;
    };
    s.keys(&["F7"]);
    s.wait("Create the folder:");
    s.text("new_folder");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("left/new_folder"), true);
    s.wait("new_folder");
    assert!(s.p("left/new_folder").is_dir());
}

#[test]
fn f8_uses_the_trash_and_shift_delete_is_permanent() {
    let Some(s) = Session::start(|root| {
        with_file(root);
        fs::write(root.join("left/gone.txt"), "x").unwrap();
    }) else {
        return;
    };
    s.wait("alpha.txt");
    s.keys(&["Down", "F8"]);
    s.wait("Move to the trash:");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("left/alpha.txt"), false);
    let trashed = s.p("home/data/Trash/files/alpha.txt");
    assert_eq!(
        fs::read_to_string(trashed).unwrap(),
        "alpha contents\nsecond line\n"
    );
    s.wait_gone("alpha.txt");
    s.keys(&["S-DC"]);
    s.wait("Permanently delete");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("left/gone.txt"), false);
    assert!(!s.p("home/data/Trash/files/gone.txt").exists());
}

#[test]
fn delete_can_be_cancelled() {
    let Some(s) = Session::start(with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.keys(&["Down", "F8"]);
    s.wait("Move to the trash:");
    s.keys(&["Escape"]);
    s.wait_gone("Move to the trash:");
    assert!(s.p("left/alpha.txt").exists());
}

#[test]
fn insert_selects_several_files_for_one_copy() {
    let Some(s) = Session::start(|root| {
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(root.join("left").join(name), name).unwrap();
        }
    }) else {
        return;
    };
    s.wait("c.txt");
    s.keys(&["Down", "IC", "IC", "F5"]);
    s.wait("Copy 2 items to:");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("right/a.txt"), true);
    s.wait_file(&s.p("right/b.txt"), true);
    assert!(!s.p("right/c.txt").exists());
}

#[test]
fn ctrl_a_changes_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let Some(s) = Session::start(with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.keys(&["Down", "C-a"]);
    s.wait("Permissions");
    s.text("600");
    s.keys(&["Enter"]);
    let start = Instant::now();
    let mode = || {
        fs::metadata(s.p("left/alpha.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    while mode() != 0o600 {
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "mode {:o}\n{}",
            mode(),
            s.screen()
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn alt_f6_creates_a_symbolic_link() {
    let Some(s) = Session::start(with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.keys(&["Down", "M-F6"]);
    s.wait("Link name:");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("right/alpha.txt"), true);
    assert!(fs::symlink_metadata(s.p("right/alpha.txt"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn f3_views_and_escape_returns() {
    let Some(s) = Session::start(with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.keys(&["Down", "F3"]);
    s.wait("alpha contents");
    s.wait("second line");
    s.keys(&["F4"]);
    s.wait("61 6C 70 68 61");
    s.keys(&["Escape"]);
    s.wait_gone("61 6C 70 68 61");
    s.wait("RenMov");
}

#[test]
fn f4_edits_saves_and_warns_about_unsaved_changes() {
    let Some(s) = Session::start(with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.keys(&["Down", "F4"]);
    s.wait("alpha contents");
    s.text("NEW ");
    s.wait("NEW alpha contents");
    s.keys(&["Escape"]);
    s.wait("has been modified");
    s.keys(&["c"]);
    s.wait_gone("has been modified");
    s.keys(&["F2"]);
    let start = Instant::now();
    while !fs::read_to_string(s.p("left/alpha.txt"))
        .unwrap()
        .starts_with("NEW ")
    {
        assert!(start.elapsed() < Duration::from_secs(8), "{}", s.screen());
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(
        fs::read_to_string(s.p("left/alpha.txt")).unwrap(),
        "NEW alpha contents\nsecond line\n"
    );
    s.keys(&["Escape"]);
    s.wait("RenMov");
}

#[test]
fn editor_undo_returns_to_the_saved_state_without_a_prompt() {
    let Some(s) = Session::start(with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.keys(&["Down", "F4"]);
    s.wait("alpha contents");
    s.text("x");
    s.wait("xalpha contents");
    s.keys(&["C-z"]);
    s.wait_gone("xalpha contents");
    s.keys(&["Escape"]);
    s.wait("RenMov");
}

#[test]
fn ctrl_o_shows_command_output_and_returns_to_panels() {
    let Some(s) = Session::start(with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.text("echo hello_from_shell");
    s.wait("echo hello_from_shell");
    s.keys(&["Enter"]);
    s.wait("alpha.txt");
    s.wait("╔");
    s.keys(&["C-o"]);
    s.wait_for("console", |t| {
        t.contains("\nhello_from_shell") && !t.contains("╔")
    });
    s.keys(&["C-o"]);
    s.wait("╔");
    s.wait("alpha.txt");
}

#[test]
fn commands_can_be_typed_on_the_console_screen() {
    let Some(s) = Session::start(with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.keys(&["C-o"]);
    s.wait_gone("╔");
    s.text("echo typed_on_console");
    s.keys(&["Enter"]);
    s.wait("\ntyped_on_console");
    assert!(!s.screen().contains("╔"));
    s.keys(&["C-o"]);
    s.wait("╔");
}

#[test]
fn cd_and_export_persist_across_commands() {
    let Some(s) = Session::start(|root| {
        fs::create_dir(root.join("left/sub")).unwrap();
        fs::write(root.join("left/sub/inner.txt"), "x").unwrap();
    }) else {
        return;
    };
    s.wait("sub");
    s.text("cd sub && export FAR_E2E=persisted");
    s.keys(&["Enter"]);
    s.wait("inner.txt");
    s.text("echo $FAR_E2E > out.txt");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("left/sub/out.txt"), true);
    let start = Instant::now();
    while fs::read_to_string(s.p("left/sub/out.txt")).unwrap().trim() != "persisted" {
        assert!(start.elapsed() < Duration::from_secs(8), "{}", s.screen());
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn ctrl_p_hides_the_inactive_panel() {
    let Some(s) = Session::start(|root| {
        fs::write(root.join("right/on_the_right.txt"), "x").unwrap();
    }) else {
        return;
    };
    s.wait("on_the_right.txt");
    s.keys(&["C-p"]);
    s.wait_gone("on_the_right.txt");
    s.keys(&["C-p"]);
    s.wait("on_the_right.txt");
}

#[test]
fn tab_switches_panel_and_enter_opens_folders() {
    let Some(s) = Session::start(|root| {
        fs::create_dir(root.join("right/folder")).unwrap();
        fs::write(root.join("right/folder/deep.txt"), "x").unwrap();
    }) else {
        return;
    };
    s.wait("folder");
    s.keys(&["Tab", "Down", "Enter"]);
    s.wait("deep.txt");
    s.keys(&["C-PageUp"]);
    s.wait_gone("deep.txt");
}

#[test]
fn alt_f7_finds_files_by_name() {
    let Some(s) = Session::start(|root| {
        fs::create_dir(root.join("left/nested")).unwrap();
        fs::write(root.join("left/nested/target_file.log"), "x").unwrap();
    }) else {
        return;
    };
    s.wait("nested");
    s.keys(&["M-F7"]);
    s.wait("File mask");
    s.text("*.log");
    s.keys(&["Enter"]);
    s.wait("target_file.log");
    s.keys(&["Enter"]);
    s.wait_gone("File mask");
    s.wait("target_file.log");
}

#[test]
fn f9_menu_runs_commands_and_escape_closes_it() {
    let Some(s) = Session::start(with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.keys(&["F9"]);
    s.wait("Quick view");
    s.keys(&["Escape"]);
    s.wait_gone("Quick view");
    s.keys(&["F9", "Right"]);
    s.wait("Make folder");
    s.keys(&["Escape"]);
    s.wait_gone("Make folder");
}

#[test]
fn editor_search_has_far_options_and_shift_f7_continues() {
    let Some(s) = Session::start(|root| {
        fs::write(
            root.join("left/f.txt"),
            "alpha1\nbeta\nalpha22\ngamma\nalpha333\n",
        )
        .unwrap();
    }) else {
        return;
    };
    s.wait("f.txt");
    s.keys(&["Down", "F4"]);
    s.wait("Ln 1/");
    s.keys(&["F7"]);
    s.wait("Regular expressions");
    let dialog = s.screen();
    for label in [
        "Case sensitive",
        "Whole words",
        "Fuzzy search",
        "Find previous",
    ] {
        assert!(dialog.contains(label), "{label} missing:\n{dialog}");
    }
    s.text(r"alpha\d+");
    s.keys(&["M-e", "Enter"]);
    s.wait_gone("Regular expressions");
    s.wait("Ln 1/");
    s.keys(&["S-F7"]);
    s.wait("Ln 3/");
    s.keys(&["S-F7"]);
    s.wait("Ln 5/");
    s.keys(&["M-F7"]);
    s.wait("Ln 3/");
    s.keys(&["S-F7", "S-F7"]);
    s.wait("Not found");
}

#[test]
fn editor_replace_continues_and_preserves_the_rest() {
    let Some(s) = Session::start(|root| {
        fs::write(root.join("left/f.txt"), "old one\nold two\n").unwrap();
    }) else {
        return;
    };
    s.wait("f.txt");
    s.keys(&["Down", "F4"]);
    s.wait("Ln 1/");
    s.keys(&["C-F7"]);
    s.wait("Replace with");
    assert!(s.screen().contains("Preserve style"));
    s.text("old");
    s.keys(&["Tab"]);
    s.text("new");
    s.keys(&["Enter"]);
    s.wait("Skip");
    s.keys(&["Enter"]);
    s.wait_for("second prompt", |t| {
        t.contains("Skip") && t.contains("new one")
    });
    s.keys(&["Enter"]);
    s.wait_gone("Skip");
    s.keys(&["F2", "F10"]);
    s.wait_gone("Ln ");
    assert_eq!(
        fs::read_to_string(s.p("left/f.txt")).unwrap(),
        "new one\nnew two\n"
    );
}

#[test]
fn viewer_space_and_shift_f7_continue_a_search() {
    let Some(s) = Session::start(|root| {
        fs::write(
            root.join("left/f.txt"),
            "first\nneedle a\nthird\nneedle b\n",
        )
        .unwrap();
    }) else {
        return;
    };
    s.wait("f.txt");
    s.keys(&["Down", "F3"]);
    s.wait("needle a");
    s.keys(&["F7"]);
    s.wait("Regular expressions");
    s.text("needle");
    s.keys(&["Enter"]);
    s.wait_gone("Regular expressions");
    s.keys(&["Space"]);
    s.wait_for("second match on top", |t| {
        t.lines().nth(1).is_some_and(|l| l.contains("needle b"))
    });
    s.keys(&["Space"]);
    s.wait("Not found");
}

#[test]
fn search_history_is_kept_in_the_database() {
    let Some(s) = Session::start(|root| {
        fs::write(root.join("left/f.txt"), "x\n").unwrap();
        let db = root.join("home/data/far_linux/history.db");
        let h = far_linux::history::History::open(&db).unwrap();
        h.add(far_linux::history::Kind::Search, "from an earlier run");
    }) else {
        return;
    };
    s.wait("f.txt");
    s.keys(&["Down", "F4"]);
    s.wait("Ln 1/");
    s.keys(&["F7"]);
    s.wait("from an earlier run");
    s.keys(&["BSpace"]);
    s.keys(&["C-a"]);
    s.text("typed now");
    s.keys(&["Enter"]);
    s.wait("Not found");
    s.keys(&["Enter"]);
    s.wait_gone("Not found");
    s.keys(&["F7"]);
    s.wait("typed now");
    s.keys(&["C-Down"]);
    s.wait("History");
    assert!(s.screen().contains("from an earlier run"));
    s.keys(&["Escape", "Escape", "F10"]);
    s.wait_gone("Ln 1/");
    s.keys(&["F10"]);
    s.wait("Do you want to quit");
    s.keys(&["Enter"]);
    s.wait_gone("Do you want to quit");
    let db = s.p("home/data/far_linux/history.db");
    s.wait_file(&db, true);
    let h = far_linux::history::History::open(&db).unwrap();
    let list = h.list(far_linux::history::Kind::Search);
    assert!(list.iter().any(|v| v.contains("typed now")), "{list:?}");
}

#[test]
fn shift_f1_packs_enter_browses_f5_extracts_and_f8_deletes() {
    let Some(s) = Session::start(|root| {
        fs::create_dir_all(root.join("left/data/sub")).unwrap();
        fs::write(root.join("left/data/a.txt"), "aaa").unwrap();
        fs::write(root.join("left/data/sub/b.txt"), "bbb").unwrap();
        fs::write(root.join("left/data/c.txt"), "ccc").unwrap();
    }) else {
        return;
    };
    s.wait("data");
    s.keys(&["Down", "S-F1"]);
    s.wait("Archive path");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("left/data.zip"), true);
    s.wait("1 files");
    // The cursor is on the new archive. Enter opens it like a folder.
    s.keys(&["Enter"]);
    s.wait("data.zip:/");
    s.keys(&["Down", "Enter"]);
    s.wait("data.zip:/data");
    s.wait("a.txt");
    // ".." then a.txt, c.txt or sub, depending on sorting: folders come first.
    s.keys(&["End", "F3"]);
    s.wait("ccc");
    s.keys(&["Escape"]);
    s.wait_gone("ccc");
    s.keys(&["F5"]);
    s.wait("Extract");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("right/c.txt"), true);
    assert_eq!(fs::read_to_string(s.p("right/c.txt")).unwrap(), "ccc");
    // The file appears before the operation ends, and keys sent during an
    // operation are discarded. The right panel's count shows that it ended.
    s.wait("3 bytes, 1 files");
    s.keys(&["F8"]);
    s.wait("Delete from the archive");
    s.keys(&["Enter"]);
    s.wait_gone("Delete from the archive");
    // Both panels now hold one 3-byte file: a.txt on the left, c.txt on the right.
    s.wait_for("the file to leave the archive", |t| {
        t.matches("3 bytes, 1 files").count() == 2
    });
    assert!(fs::read_to_string(s.p("left/data/c.txt")).is_ok());
    // Leaving through ".." at the top puts the cursor back on the archive.
    s.keys(&["C-PageUp", "C-PageUp"]);
    s.wait_gone("data.zip:/");
    let entries = far_linux::archive::list(&s.p("left/data.zip"), far_linux::archive::Format::Zip);
    let names: Vec<String> = entries.unwrap().into_iter().map(|e| e.path).collect();
    assert!(names.iter().any(|n| n == "data/sub/b.txt"), "{names:?}");
    assert!(!names.iter().any(|n| n == "data/c.txt"), "{names:?}");
}

#[test]
fn shift_f2_extracts_a_tar_gz_made_with_the_format_button() {
    let Some(s) = Session::start(|root| {
        fs::write(root.join("left/note.txt"), "hello").unwrap();
    }) else {
        return;
    };
    s.wait("note.txt");
    s.keys(&["Down", "S-F1"]);
    s.wait("Archive path");
    // Down to the archiver row: Right chooses tar.gz, and the name follows.
    s.keys(&["Down", "Down", "Right"]);
    s.wait("note.txt.tar.gz");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("left/note.txt.tar.gz"), true);
    s.wait("2 files");
    s.keys(&["S-F2"]);
    s.wait("Extract to");
    s.keys(&["M-i", "Tab"]);
    s.keys(&["Tab", "Enter"]);
    let out = s.p("right/note.txt/note.txt");
    s.wait_file(&out, true);
    assert_eq!(fs::read_to_string(out).unwrap(), "hello");
}

#[test]
fn f6_to_an_archive_on_the_other_panel_moves_the_files() {
    let Some(s) = Session::start(|root| {
        fs::write(root.join("left/move.txt"), "m").unwrap();
        fs::write(root.join("right/seed.txt"), "s").unwrap();
    }) else {
        return;
    };
    s.wait("move.txt");
    s.keys(&["Tab"]);
    s.wait("seed.txt");
    s.keys(&["Down", "S-F1"]);
    s.wait("Archive path");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("right/seed.txt.zip"), true);
    s.wait("2 files");
    s.keys(&["Enter"]);
    s.wait("seed.txt.zip:/");
    s.keys(&["Tab", "Down", "F6"]);
    s.wait("Move to archive");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("left/move.txt"), false);
    let entries =
        far_linux::archive::list(&s.p("right/seed.txt.zip"), far_linux::archive::Format::Zip);
    let names: Vec<String> = entries.unwrap().into_iter().map(|e| e.path).collect();
    assert!(names.contains(&"move.txt".to_string()), "{names:?}");
}

#[test]
fn renaming_a_non_utf8_name_without_editing_keeps_its_bytes() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let name = OsStr::from_bytes(b"bad\xff.txt");
    let Some(s) = Session::start(|root| {
        fs::write(root.join("left").join(name), "x").unwrap();
    }) else {
        return;
    };
    s.wait("bad");
    s.keys(&["Down", "S-F6"]);
    s.wait("Rename or move");
    s.keys(&["Enter"]);
    s.wait_gone("Rename or move");
    s.keys(&["S-F5"]);
    s.wait("Copy contents of symbolic links");
    s.keys(&["Enter"]);
    s.wait_gone("Copy contents of symbolic links");
    let names: Vec<_> = fs::read_dir(s.p("left"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(names, [name]);
}

#[test]
fn ctrl_a_recursive_leaves_unchanged_modes_alone() {
    use std::os::unix::fs::PermissionsExt;
    let Some(s) = Session::start(|root| {
        fs::create_dir(root.join("left/d")).unwrap();
        fs::write(root.join("left/d/secret"), "x").unwrap();
        let private = fs::Permissions::from_mode(0o600);
        fs::set_permissions(root.join("left/d/secret"), private).unwrap();
    }) else {
        return;
    };
    s.wait("d");
    s.keys(&["Down", "C-a"]);
    s.wait("Apply to folder contents");
    s.keys(&["M-Up"]);
    s.keys(&["Up", "Space", "Enter"]);
    s.wait_gone("Apply to folder contents");
    let mode = fs::metadata(s.p("left/d/secret"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn hostile_file_name_does_not_reach_the_terminal() {
    let Some(s) = Session::start(|root| {
        fs::write(root.join("left/evil\x1b[2Jname"), "x").unwrap();
        fs::write(root.join("left/plain.txt"), "x").unwrap();
    }) else {
        return;
    };
    s.wait("plain.txt");
    s.wait("evil?[2Jname");
}

#[test]
fn terminal_is_restored_on_exit() {
    let Some(s) = Session::start(with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.keys(&["F10"]);
    s.wait("Do you want to quit?");
    s.keys(&["Enter"]);
    s.wait_gone("╔");
    let on_alternate = s.tmux(&["display-message", "-p", "-t", "t", "#{alternate_on}"]);
    assert_eq!(on_alternate.trim(), "0");
    assert_eq!(
        s.tmux(&["display-message", "-p", "-t", "t", "#{cursor_flag}"])
            .trim(),
        "1"
    );
}

/// Sends a Windows Terminal input mode event for a key going down.
fn win32_key(vk: u32, unit: u32, state: u32) -> String {
    format!("\x1b[{vk};0;{unit};1;{state};1_")
}

const WIN32: &str = "FAR_WIN32_INPUT=1";
const SHIFT: u32 = 0x10;
const LEFT_CTRL: u32 = 0x08;
const LEFT_ALT: u32 = 0x02;

#[test]
fn key_bar_follows_held_modifiers_in_windows_terminal_mode() {
    let Some(s) = Session::start_with(WIN32, with_file) else {
        return;
    };
    s.wait("5Copy");
    assert!(s.screen().contains("6RenMov"), "{}", s.screen());
    s.text(&win32_key(0x10, 0, SHIFT));
    s.wait("6Rename");
    s.wait("1Add");
    s.text(&format!(
        "\x1b[16;0;0;0;0;1_{}",
        win32_key(0x11, 0, LEFT_CTRL)
    ));
    s.wait("3Name");
    s.text(&format!(
        "\x1b[17;0;0;0;0;1_{}",
        win32_key(0x12, 0, LEFT_ALT)
    ));
    s.wait("7Find");
    // Losing focus can swallow the release of a modifier.
    s.text("\x1b[O");
    s.wait("6RenMov");
}

#[test]
fn keys_work_in_windows_terminal_mode() {
    let Some(s) = Session::start_with(WIN32, with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.text(&win32_key(0x76, 0, 0));
    s.wait("Create the folder");
    s.text(&win32_key(0x1B, 27, 0));
    s.wait_gone("Create the folder");
    // Typed text reaches the command line, with its repeat count.
    s.text("\x1b[65;0;97;1;0;2_");
    s.wait("aa");
    s.text(&win32_key(0x08, 8, 0));
    s.text(&win32_key(0x08, 8, 0));
    // Ctrl+O goes through the Ctrl key, not the control code alone.
    s.text(&win32_key(0x4F, 15, LEFT_CTRL));
    s.wait_gone("5Copy");
    // The console is back to ordinary keys, as the mode is off there.
    s.keys(&["C-o"]);
    s.wait("5Copy");
}

#[test]
fn selected_names_and_headers_use_colours_a_scheme_cannot_change() {
    let Some(s) = Session::start_with("COLORTERM=truecolor", with_file) else {
        return;
    };
    s.wait("alpha.txt");
    let color = |wanted: &str| {
        s.tmux(&["capture-pane", "-p", "-e", "-t", "t"])
            .contains(wanted)
    };
    // Far's bright yellow: the column titles straight away.
    assert!(color("38;5;226"), "no fixed yellow");
    s.keys(&["Down", "IC"]);
    s.wait("in 1 items");
    assert!(color("38;5;226"));
}

#[test]
fn f3_on_up_counts_the_current_folder_into_the_size_column() {
    let Some(s) = Session::start(|root| {
        fs::write(root.join("left/data.bin"), vec![0u8; 12345]).unwrap();
        fs::create_dir(root.join("left/sub")).unwrap();
        fs::write(root.join("left/sub/more.bin"), vec![0u8; 10000]).unwrap();
    }) else {
        return;
    };
    s.wait("data.bin");
    // The cursor starts on `..`, and the footer shows it.
    s.keys(&["F3"]);
    s.wait("22345");
    assert!(!s.screen().contains("Folder size"), "{}", s.screen());
}

#[test]
fn f3_on_a_folder_counts_it_and_leaves_the_panel_usable() {
    let Some(s) = Session::start(|root| {
        fs::create_dir(root.join("left/sub")).unwrap();
        fs::write(root.join("left/sub/more.bin"), vec![0u8; 4321]).unwrap();
    }) else {
        return;
    };
    s.wait("sub");
    s.keys(&["Down", "F3"]);
    s.wait("4321");
    s.keys(&["Enter"]);
    s.wait("more.bin");
}

#[test]
fn delete_of_full_folder_asks_with_all_and_skip_buttons() {
    let Some(s) = Session::start(|root| {
        for name in ["one", "two"] {
            fs::create_dir(root.join("left").join(name)).unwrap();
            fs::write(root.join("left").join(name).join("inner"), "x").unwrap();
        }
    }) else {
        return;
    };
    s.wait("two");
    s.keys(&["Down", "IC", "IC"]);
    s.wait("in 2 items");
    // Shift+Del: no trash is involved, so the test leaves nothing behind.
    s.keys(&["S-DC"]);
    s.wait("Permanently delete");
    s.keys(&["Enter"]);
    s.wait("The following folder will be deleted:");
    assert!(s.screen().contains("[ All ]"), "{}", s.screen());
    assert!(s.screen().contains("[ Skip all ]"), "{}", s.screen());
    s.keys(&["Right", "Enter"]);
    s.wait_file(&s.p("left/one"), false);
    s.wait_file(&s.p("left/two"), false);
    s.wait_gone("will be deleted");
}

#[test]
fn tab_completes_on_the_console_and_changes_panel_otherwise() {
    let Some(s) = Session::start(|root| {
        fs::write(root.join("left/unique_name.txt"), "x").unwrap();
        fs::write(root.join("left/two_a"), "x").unwrap();
        fs::write(root.join("left/two_b"), "x").unwrap();
    }) else {
        return;
    };
    s.wait("unique_name");
    s.keys(&["C-o"]);
    s.wait_gone("5Copy");
    s.text("cat uni");
    s.wait("cat uni");
    s.keys(&["Tab"]);
    s.wait("cat unique_name.txt");
    s.keys(&["C-u"]);
    s.keys(&["Escape"]);
    s.wait_gone("cat unique");
    s.text("ls two");
    s.keys(&["Tab"]);
    s.wait("ls two_");
    // No further progress is possible, so the choices are listed.
    s.keys(&["Tab"]);
    s.wait("two_a");
    s.wait("two_b");
    s.keys(&["Escape", "C-o"]);
    s.wait("5Copy");
    // With panels on screen Tab still changes panel, as in Far: the cursor can
    // now reach the right panel, so `F7` creates its folder there.
    s.keys(&["Tab", "F7"]);
    s.wait("Create the folder");
    s.text("made_right");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("right/made_right"), true);
}

#[test]
fn being_told_to_end_gives_the_terminal_back() {
    let Some(s) = Session::start_with(WIN32, with_file) else {
        return;
    };
    s.wait("alpha.txt");
    assert_eq!(
        s.tmux(&["display", "-p", "-t", "t", "#{alternate_on}"])
            .trim(),
        "1"
    );
    let shell = s.tmux(&["display", "-p", "-t", "t", "#{pane_pid}"]);
    let far = Command::new("pgrep")
        .args(["-P", shell.trim(), "far"])
        .output()
        .expect("pgrep runs");
    let far = String::from_utf8_lossy(&far.stdout);
    Command::new("kill")
        .args(["-TERM", far.trim()])
        .status()
        .expect("kill runs");
    let start = Instant::now();
    while s
        .tmux(&["display", "-p", "-t", "t", "#{alternate_on}"])
        .trim()
        != "0"
    {
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "still on the alternate screen"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn exit_command_ends_far_from_panels_and_console() {
    for console in [false, true] {
        let Some(s) = Session::start(with_file) else {
            return;
        };
        s.wait("alpha.txt");
        if console {
            s.keys(&["C-o"]);
            s.wait_gone("5Copy");
        }
        s.text("exit");
        s.wait("exit");
        s.keys(&["Enter"]);
        let start = Instant::now();
        while s
            .tmux(&["display", "-p", "-t", "t", "#{alternate_on}"])
            .trim()
            != "0"
            || s.screen().contains("5Copy")
        {
            assert!(start.elapsed() < Duration::from_secs(8), "{}", s.screen());
            std::thread::sleep(Duration::from_millis(30));
        }
        // The harness keeps the pane alive with `sleep`: far itself must be gone.
        // It restores the screen first and then closes the history, so give
        // it a moment to finish exiting.
        let shell = s.tmux(&["display", "-p", "-t", "t", "#{pane_pid}"]);
        let running = || {
            !Command::new("pgrep")
                .args(["-P", shell.trim(), "far"])
                .output()
                .expect("pgrep runs")
                .stdout
                .is_empty()
        };
        let start = Instant::now();
        while running() {
            assert!(
                start.elapsed() < Duration::from_secs(8),
                "far is still running"
            );
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}

#[test]
fn shift_f6_and_shift_f5_on_a_file_named_tilde_stay_in_the_folder() {
    let Some(s) = Session::start(|root| {
        fs::write(root.join("left/~"), "tilde").unwrap();
    }) else {
        return;
    };
    s.wait("~");
    s.keys(&["Down", "S-F6"]);
    s.wait("Rename or move");
    s.keys(&["Enter"]);
    s.wait_gone("Rename or move");
    s.keys(&["S-F5"]);
    s.wait(" to:");
    s.keys(&["Enter"]);
    std::thread::sleep(Duration::from_millis(300));
    s.keys(&["Escape", "Escape"]);
    assert_eq!(fs::read_to_string(s.p("left/~")).unwrap(), "tilde");
    assert!(!s.p("home/~").exists(), "ended up inside the home folder");
    let home: Vec<_> = fs::read_dir(s.p("home"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .filter(|n| n != "data")
        .collect();
    assert!(home.is_empty(), "{home:?}");
}

#[test]
fn ctrl_c_clears_the_command_line_on_panels_and_console() {
    let Some(s) = Session::start(with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.text("typed_on_panels");
    s.wait("typed_on_panels");
    s.keys(&["C-c"]);
    s.wait_gone("typed_on_panels");
    // Still running, with the panels working.
    s.wait("5Copy");
    s.keys(&["C-o"]);
    s.wait_gone("5Copy");
    s.text("typed_on_console");
    s.wait("typed_on_console");
    s.keys(&["C-c"]);
    s.wait("^C");
    s.keys(&["C-o"]);
    s.wait("5Copy");
}

fn has_color(s: &Session, color: &str, name: &str) -> bool {
    s.tmux(&["capture-pane", "-p", "-e", "-t", "t"])
        .contains(&format!("38;5;{color}m{name}"))
}

#[test]
fn highlighting_comes_from_the_settings_file() {
    let Some(s) = Session::start_with("COLORTERM=truecolor", |root| {
        fs::write(root.join("left/a.foo"), "").unwrap();
        fs::create_dir_all(root.join("home/config/far_linux")).unwrap();
        fs::write(
            root.join("home/config/far_linux/highlight.ini"),
            "[group]\nmask = *.foo\nnormal = 123\n",
        )
        .unwrap();
    }) else {
        return;
    };
    s.wait("a.foo");
    assert!(has_color(&s, "123", "a.foo"), "{}", s.screen());
}

#[test]
fn a_broken_settings_file_is_reported_and_far_still_starts() {
    let Some(s) = Session::start(|root| {
        fs::write(root.join("left/a.zip"), "").unwrap();
        fs::create_dir_all(root.join("home/config/far_linux")).unwrap();
        fs::write(
            root.join("home/config/far_linux/highlight.ini"),
            "nonsense\n",
        )
        .unwrap();
    }) else {
        return;
    };
    s.wait("line 1: expected key = value");
    s.keys(&["Enter"]);
    s.wait("a.zip");
    s.wait("5Copy");
}

#[test]
fn the_menu_opens_the_settings_as_the_defaults_and_applies_them_on_close() {
    let Some(s) = Session::start_with("COLORTERM=truecolor", |root| {
        fs::write(root.join("left/a.zip"), "").unwrap();
    }) else {
        return;
    };
    s.wait("a.zip");
    let file = s.p("home/config/far_linux/highlight.ini");
    assert!(!file.exists());
    s.keys(&["F9"]);
    s.wait("Left");
    s.keys(&["Right", "Right"]);
    s.wait("Files highlighting");
    for _ in 0..30 {
        if s.tmux(&["capture-pane", "-p", "-e", "-t", "t"])
            .contains("48;5;30m Files highlighting")
        {
            break;
        }
        s.keys(&["Down"]);
        std::thread::sleep(Duration::from_millis(30));
    }
    s.keys(&["Enter"]);
    s.wait("File highlighting for far");
    assert!(fs::read_to_string(&file).unwrap().contains("<arc>"));
    // Change the file as if edited, then leave: the panels must follow it.
    fs::write(&file, "[group]\nmask = *.zip\nnormal = 123\n").unwrap();
    s.keys(&["F10"]);
    s.wait("a.zip");
    s.wait_for("new colour", |_| has_color(&s, "123", "a.zip"));
}

#[test]
fn ctrl_letters_do_not_answer_a_delete_prompt() {
    let Some(s) = Session::start(|root| {
        fs::create_dir(root.join("left/full")).unwrap();
        fs::write(root.join("left/full/inner"), "x").unwrap();
    }) else {
        return;
    };
    s.wait("full");
    s.keys(&["Down", "S-DC"]);
    s.wait("Permanently delete");
    s.keys(&["Enter"]);
    s.wait("The following folder will be deleted:");
    s.keys(&["C-a", "C-d"]);
    std::thread::sleep(Duration::from_millis(200));
    assert!(s.p("left/full/inner").exists());
    assert!(s.screen().contains("will be deleted"), "{}", s.screen());
    s.keys(&["Escape"]);
    s.wait_gone("will be deleted");
    assert!(s.p("left/full/inner").exists());
}

#[test]
fn resizing_at_a_prompt_during_an_operation_leaves_a_clean_screen() {
    let Some(s) = Session::start(|root| {
        fs::create_dir(root.join("left/full")).unwrap();
        fs::write(root.join("left/full/inner"), "x").unwrap();
    }) else {
        return;
    };
    s.wait("full");
    s.keys(&["Down", "S-DC"]);
    s.wait("Permanently delete");
    s.keys(&["Enter"]);
    s.wait("will be deleted");
    s.tmux(&["resize-window", "-t", "t", "-x", "70", "-y", "30"]);
    std::thread::sleep(Duration::from_millis(200));
    s.keys(&["Escape"]);
    s.wait_gone("will be deleted");
    s.wait("5Copy");
    let screen = s.screen();
    let top = screen.lines().next().unwrap_or("");
    assert!(
        top.chars().count() <= 70 && top.starts_with('╔'),
        "{screen}"
    );
    assert!(
        screen.lines().any(|l| l.trim_end().ends_with('╝')),
        "{screen}"
    );
}

#[test]
fn keys_typed_on_the_console_do_not_wait_for_a_later_visit() {
    let Some(s) = Session::start_with(WIN32, with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.text(&win32_key(0x4F, 15, LEFT_CTRL));
    s.wait_gone("5Copy");
    // One write holding Ctrl+O and more: crossterm reads it all at once.
    s.text("\x0ftouch typed_ahead\r");
    s.wait_file(&s.p("left/typed_ahead"), true);
    s.wait("5Copy");
}

#[test]
fn f8_moves_to_the_trash() {
    let Some(s) = Session::start(with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.keys(&["Down", "F8"]);
    s.wait("Move to the trash");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("home/data/Trash/files/alpha.txt"), true);
    s.wait_file(&s.p("left/alpha.txt"), false);
}

/// The trash cannot be created under /proc, as on another file system.
const NO_TRASH: &str = "XDG_DATA_HOME=/proc/far_no_trash";

#[test]
fn a_file_that_cannot_go_to_the_trash_asks_and_skip_keeps_it() {
    let Some(s) = Session::start_with(NO_TRASH, with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.keys(&["Down", "F8"]);
    s.wait("Move to the trash");
    s.keys(&["Enter"]);
    s.wait("This cannot go to the trash");
    assert!(s.p("left/alpha.txt").exists());
    // Delete, All, Skip, Cancel.
    s.keys(&["Right", "Right", "Enter"]);
    s.wait_gone("This cannot go to the trash");
    s.wait("5Copy");
    assert!(s.p("left/alpha.txt").exists());
}

#[test]
fn a_file_that_cannot_go_to_the_trash_can_be_deleted_for_good() {
    let Some(s) = Session::start_with(NO_TRASH, with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.keys(&["Down", "F8"]);
    s.wait("Move to the trash");
    s.keys(&["Enter"]);
    s.wait("This cannot go to the trash");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("left/alpha.txt"), false);
}

#[test]
fn moving_a_full_folder_into_an_archive_does_not_ask_about_deleting_it() {
    let Some(s) = Session::start(|root| {
        fs::create_dir(root.join("left/full")).unwrap();
        fs::write(root.join("left/full/inner"), "x").unwrap();
        fs::write(root.join("right/seed.txt"), "s").unwrap();
    }) else {
        return;
    };
    s.wait("full");
    s.keys(&["Tab"]);
    s.wait("seed.txt");
    s.keys(&["Down", "S-F1"]);
    s.wait("Archive path");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("right/seed.txt.zip"), true);
    s.wait("2 files");
    s.keys(&["Enter"]);
    s.wait("seed.txt.zip:/");
    s.keys(&["Tab", "Down", "F6"]);
    s.wait("Move to archive");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("left/full"), false);
    assert!(!s.screen().contains("will be deleted"), "{}", s.screen());
}

#[test]
fn f3_counts_all_selected_folders() {
    let Some(s) = Session::start(|root| {
        for (name, size) in [("one", 1111), ("two", 2222)] {
            fs::create_dir(root.join("left").join(name)).unwrap();
            fs::write(root.join("left").join(name).join("data"), vec![0u8; size]).unwrap();
        }
    }) else {
        return;
    };
    s.wait("two");
    s.keys(&["Down", "IC", "IC"]);
    s.wait("in 2 items");
    // Back onto a folder, as the selection is what gets counted.
    s.keys(&["Up", "F3"]);
    s.wait("3 333 bytes in 2 items");
}

#[test]
fn exit_inside_a_longer_command_goes_to_bash_and_far_stays() {
    let Some(s) = Session::start(with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.text("(exit 3); touch still_here");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("left/still_here"), true);
    s.wait("5Copy");
}

#[test]
fn key_bar_follows_modifiers_in_the_viewer_and_editor() {
    let Some(s) = Session::start_with(WIN32, with_file) else {
        return;
    };
    s.wait("alpha.txt");
    let release = "\x1b[16;0;0;0;0;1_";
    s.text(&win32_key(0x28, 0, 0));
    s.text(&win32_key(0x72, 0, 0));
    s.wait("alpha contents");
    s.wait("7Search");
    s.text(&win32_key(0x10, 0, SHIFT));
    s.wait("2WWrap");
    s.text(release);
    s.wait("7Search");
    s.text(&win32_key(0x11, 0, LEFT_CTRL));
    s.wait("10GoFile");
    s.text("\x1b[17;0;0;0;0;1_");
    s.text(&win32_key(0x79, 0, 0));
    s.wait("5Copy");
    s.text(&win32_key(0x73, 0, 0));
    s.wait("alpha contents");
    s.wait("2Save");
    s.text(&win32_key(0x10, 0, SHIFT));
    s.wait("2SaveAs");
    s.text(release);
    s.wait("2Save ");
    s.text(&win32_key(0x11, 0, LEFT_CTRL));
    s.wait("7Replac");
}

#[test]
fn a_failed_save_as_keeps_the_original_file_for_f2() {
    let Some(s) = Session::start(with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.keys(&["Down", "F4"]);
    s.wait("alpha contents");
    s.text("NEW ");
    s.wait("NEW alpha");
    s.keys(&["S-F2"]);
    s.wait("File name:");
    s.keys(&["C-y"]);
    s.text("/proc/no/such/folder/file");
    s.keys(&["Enter"]);
    s.wait("Save failed");
    s.keys(&["Enter"]);
    s.wait_gone("Save failed");
    s.keys(&["F2"]);
    s.wait_for("the save", |_| {
        fs::read_to_string(s.p("left/alpha.txt")).is_ok_and(|t| t.starts_with("NEW alpha"))
    });
}

#[test]
fn the_results_of_find_show_the_end_of_long_paths() {
    let deep = "a_very_long_folder_name_that_fills_the_screen/".repeat(4);
    let Some(s) = Session::start(|root| {
        fs::create_dir_all(root.join("left").join(&deep)).unwrap();
        fs::write(root.join("left").join(&deep).join("needle.log"), "x").unwrap();
    }) else {
        return;
    };
    s.wait("a_very");
    s.keys(&["M-F7"]);
    s.wait("File mask");
    s.keys(&["C-y"]);
    s.text("needle*");
    s.keys(&["Enter"]);
    s.wait("needle.log");
}

#[test]
fn ctrl_c_cancels_a_prompt_free_panel_state_without_quitting() {
    let Some(s) = Session::start(with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.keys(&["C-c", "C-c"]);
    s.keys(&["Down", "IC"]);
    s.wait("in 1 items");
}

#[test]
fn marked_names_are_fixed_yellow_on_screen() {
    let Some(s) = Session::start_with("COLORTERM=truecolor", with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.keys(&["Down", "IC"]);
    s.wait("in 1 items");
    s.keys(&["Up"]);
    s.wait_for("the yellow name", |_| has_color(&s, "226", "alpha.txt"));
}

fn editor_with_lines(s: &Session) {
    s.keys(&["Down", "F4"]);
    s.wait("line_a");
}

fn with_three_lines(root: &Path) {
    fs::write(root.join("left/text.txt"), "line_a\nline_b\nline_c\n").unwrap();
}

#[test]
fn ctrl_c_then_ctrl_v_in_the_editor_keeps_line_ends() {
    let Some(s) = Session::start(with_three_lines) else {
        return;
    };
    s.wait("text.txt");
    editor_with_lines(&s);
    // Mark all three lines, copy, go to the end and paste.
    s.keys(&["S-Down", "S-Down", "S-Down", "C-c", "C-End", "C-v", "F2"]);
    s.wait_for("the save", |_| {
        fs::read_to_string(s.p("left/text.txt")).is_ok_and(|t| t.lines().count() > 3)
    });
    assert_eq!(
        fs::read_to_string(s.p("left/text.txt")).unwrap(),
        "line_a\nline_b\nline_c\nline_a\nline_b\nline_c\n"
    );
}

#[test]
fn a_terminal_paste_in_the_editor_keeps_line_ends() {
    let Some(s) = Session::start(with_three_lines) else {
        return;
    };
    s.wait("text.txt");
    editor_with_lines(&s);
    s.tmux(&["set-buffer", "-b", "p", "pasted_1\npasted_2\n"]);
    s.keys(&["C-End"]);
    s.tmux(&["paste-buffer", "-p", "-b", "p", "-t", "t"]);
    s.wait("pasted_2");
    s.keys(&["F2"]);
    s.wait_for("the save", |_| {
        fs::read_to_string(s.p("left/text.txt")).is_ok_and(|t| t.contains("pasted_2"))
    });
    assert_eq!(
        fs::read_to_string(s.p("left/text.txt")).unwrap(),
        "line_a\nline_b\nline_c\npasted_1\npasted_2\n"
    );
}

/// How a paste appears to arrive in Windows Terminal's input mode: characters
/// sent as key events with no key code. Not confirmed in Windows Terminal.
fn synthetic_text(text: &str) -> String {
    text.encode_utf16()
        .map(|unit| format!("\x1b[0;0;{unit};1;0;1_\x1b[0;0;{unit};0;0;1_"))
        .collect()
}

#[test]
fn a_paste_sent_as_key_events_keeps_line_ends() {
    let Some(s) = Session::start_with(WIN32, with_three_lines) else {
        return;
    };
    s.wait("text.txt");
    s.text(&win32_key(0x28, 0, 0));
    s.text(&win32_key(0x73, 0, 0));
    s.wait("line_a");
    s.text(&win32_key(0x23, 0, LEFT_CTRL));
    s.text(&synthetic_text("pasted_1\rpasted_2\r"));
    s.wait("pasted_2");
    s.text(&win32_key(0x71, 0, 0));
    s.wait_for("the save", |_| {
        fs::read_to_string(s.p("left/text.txt")).is_ok_and(|t| t.contains("pasted_2"))
    });
    assert_eq!(
        fs::read_to_string(s.p("left/text.txt")).unwrap(),
        "line_a\nline_b\nline_c\npasted_1\npasted_2\n"
    );
}

#[test]
fn source_code_is_coloured_in_the_editor() {
    let Some(s) = Session::start_with("COLORTERM=truecolor", |root| {
        fs::write(
            root.join("left/code.cpp"),
            "int first; // a_comment\nint main() { return 42; }\nconst char* s = \"text\";\n",
        )
        .unwrap();
    }) else {
        return;
    };
    s.wait("code.cpp");
    s.keys(&["Down", "F4"]);
    s.wait("a_comment");
    let raw = |wanted: &str| {
        s.tmux(&["capture-pane", "-p", "-e", "-t", "t"])
            .contains(wanted)
    };
    s.wait_for("the comment colour", |_| raw("38;5;30m// a_comment"));
    assert!(raw("38;5;231mreturn"), "keyword");
    assert!(raw("38;5;46m42"), "number");
    assert!(raw("38;5;226mint"), "type");
    assert!(raw("38;5;226m\"text\""), "string");
    // Colours must not reach the saved text.
    s.keys(&["C-End"]);
    s.text("tail");
    s.wait("tail");
    s.keys(&["F2"]);
    s.wait_for("the save", |_| {
        fs::read_to_string(s.p("left/code.cpp")).is_ok_and(|t| t.ends_with("tail"))
    });
    assert_eq!(
        fs::read_to_string(s.p("left/code.cpp")).unwrap(),
        "int first; // a_comment\nint main() { return 42; }\nconst char* s = \"text\";\ntail"
    );
}

/// Pasted text as ConPTY delivers it: real key codes, with a line feed as
/// Ctrl+Enter. Recorded from conhost 10.0.26200.
fn conpty_text(text: &str) -> String {
    let press = |vk: u32, unit: u32, state: u32| {
        format!("\x1b[{vk};0;{unit};1;{state};1_\x1b[{vk};0;{unit};0;{state};1_")
    };
    text.chars()
        .map(|c| match c {
            '\n' => format!("\x1b[17;29;0;1;8;1_{}\x1b[17;29;0;0;0;1_", press(13, 10, 8)),
            '\r' => press(13, 13, 0),
            c => press(c.to_ascii_uppercase() as u32, c as u32, 0),
        })
        .collect()
}

/// A paste marker as ConPTY delivers it: key events with no key code, pressed
/// and never released.
fn conpty_marker(marker: &str) -> String {
    marker
        .chars()
        .map(|c| format!("\x1b[0;0;{};1;0;1_", c as u32))
        .collect()
}

fn paste_through_conpty(payload: &str) {
    let Some(s) = Session::start_with(WIN32, with_three_lines) else {
        return;
    };
    s.wait("text.txt");
    s.text(&win32_key(0x28, 0, 0));
    s.text(&win32_key(0x73, 0, 0));
    s.wait("line_a");
    s.text(&win32_key(0x23, 0, LEFT_CTRL));
    s.wait("Ln 4/4");
    s.text(payload);
    s.wait("pasted_2");
    s.text(&win32_key(0x71, 0, 0));
    s.wait_for("the save", |_| {
        fs::read_to_string(s.p("left/text.txt")).is_ok_and(|t| t.contains("pasted_2"))
    });
    assert_eq!(
        fs::read_to_string(s.p("left/text.txt")).unwrap(),
        "line_a\nline_b\nline_c\npasted_1\npasted_2\n"
    );
}

#[test]
fn a_paste_with_markers_through_conpty_keeps_line_ends() {
    let body = conpty_text("pasted_1\npasted_2\n");
    let start = conpty_marker("\x1b[200~");
    let end = conpty_marker("\x1b[201~");
    paste_through_conpty(&format!("{start}{body}{end}"));
}

#[test]
fn a_paste_without_markers_through_conpty_keeps_line_ends() {
    paste_through_conpty(&conpty_text("pasted_1\npasted_2\n"));
}

/// A paste as the user's Windows Terminal 1.24 sent it: plain markers and text,
/// with each line end as a pair of key events. Taken from what they saw on screen.
#[test]
fn a_paste_with_line_ends_as_key_events_keeps_them() {
    let end = "\x1b[13;28;13;1;0;1_\x1b[13;28;13;0;0;1_";
    paste_through_conpty(&format!("\x1b[200~pasted_1{end}pasted_2{end}\x1b[201~"));
}

#[test]
fn android_logs_are_coloured_in_the_editor() {
    let Some(s) = Session::start_with("COLORTERM=truecolor", |root| {
        fs::write(
            root.join("left/device.log"),
            "x\n11-03 22:44:11.802  1234  5678 E BootTag: failed 42\n",
        )
        .unwrap();
    }) else {
        return;
    };
    s.wait("device.log");
    s.keys(&["Down", "F4"]);
    s.wait("BootTag");
    let raw = |wanted: &str| {
        s.tmux(&["capture-pane", "-p", "-e", "-t", "t"])
            .contains(wanted)
    };
    s.wait_for("the error colour", |_| raw("38;5;196mBootTag"));
    assert!(raw("38;5;196mE"), "level");
    assert!(raw("38;5;46m42"), "number");
}

/// A paste that stalls longer than the give-up time must not run its rest as a
/// command when the line end comes late.
#[test]
fn a_stalled_paste_does_not_run_its_rest_as_a_command() {
    let Some(s) = Session::start_with(WIN32, with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.text("\x1b[200~touch pwned");
    std::thread::sleep(Duration::from_millis(1500));
    s.text("\x1b[13;28;13;1;0;1_\x1b[13;28;13;0;0;1_\x1b[201~");
    s.wait("touch pwned");
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!s.p("left/pwned").exists());
    assert!(s.screen().contains("touch pwned"), "{}", s.screen());
}

#[test]
fn far_about_shows_the_version() {
    let Some(s) = Session::start(with_file) else {
        return;
    };
    s.wait("alpha.txt");
    s.text("far:about");
    s.keys(&["Enter"]);
    s.wait("Version information");
    s.wait(&format!("far_linux {}", env!("CARGO_PKG_VERSION")));
    s.wait("SQLite 3.");
    s.keys(&["Enter"]);
    s.wait_gone("Version information");
    // The banner is on the console, as in Far.
    s.keys(&["C-o"]);
    s.wait(&format!("far_linux {}", env!("CARGO_PKG_VERSION")));
}

#[test]
fn ctrl_a_checkboxes_change_one_permission() {
    use std::os::unix::fs::PermissionsExt;
    let Some(s) = Session::start(|root| {
        fs::write(root.join("left/a.sh"), "x").unwrap();
        fs::set_permissions(root.join("left/a.sh"), fs::Permissions::from_mode(0o644)).unwrap();
    }) else {
        return;
    };
    s.wait("a.sh");
    s.keys(&["Down", "C-a"]);
    s.wait("Set UID");
    s.wait("0644");
    // Down to the owner row, Right twice to Execute, and tick it.
    s.keys(&["Down", "Right", "Right", "Space"]);
    s.wait("0744");
    s.keys(&["Enter"]);
    s.wait_gone("Set UID");
    let start = Instant::now();
    let mode = || fs::metadata(s.p("left/a.sh")).unwrap().permissions().mode() & 0o7777;
    while mode() != 0o744 {
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "mode {:o}\n{}",
            mode(),
            s.screen()
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn alt_f7_in_an_archive_searches_the_archive() {
    use std::io::Write;
    let Some(s) = Session::start(|root| {
        let file = fs::File::create(root.join("left/pack.zip")).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("deep/inner/wanted_entry.log", options)
            .unwrap();
        zip.write_all(b"needle").unwrap();
        zip.start_file("other.txt", options).unwrap();
        zip.write_all(b"hay").unwrap();
        zip.finish().unwrap();
        // Beside the archive, and not to be found from inside it.
        fs::write(root.join("left/outside_entry.log"), "needle").unwrap();
    }) else {
        return;
    };
    s.wait("pack.zip");
    // ".." then outside_entry.log then pack.zip: End is the archive.
    s.keys(&["End"]);
    s.keys(&["Enter"]);
    s.wait("pack.zip:/");
    s.keys(&["M-F7"]);
    s.wait("File mask");
    s.text("*.log");
    s.keys(&["Down"]);
    s.text("needle");
    s.keys(&["Enter"]);
    // Far's results window: a header line for the folder, then the file.
    s.wait("Search done. Found files: 1, folders: 0");
    s.wait("Find file: *.log");
    s.wait("pack.zip:/deep/inner");
    s.wait("│wanted_entry.log");
    s.wait("{ New search } [ Go to ] [ View ] [ Cancel ]");
    assert!(!s.screen().contains("outside_entry"), "{}", s.screen());
    s.keys(&["Enter"]);
    s.wait("pack.zip:/deep/inner");
    s.wait("wanted_entry.log");
}

#[test]
fn an_archive_inside_an_archive_opens_read_only() {
    use std::io::Write;
    let zip_bytes = |name: &str, data: &[u8]| {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(data).unwrap();
        zip.finish().unwrap().into_inner()
    };
    let Some(s) = Session::start(|root| {
        let inner = zip_bytes("nested_note.txt", b"from the inner archive");
        fs::write(root.join("left/outer.zip"), zip_bytes("inner.zip", &inner)).unwrap();
    }) else {
        return;
    };
    s.wait("outer.zip");
    s.keys(&["End", "Enter"]);
    s.wait("outer.zip:/");
    s.wait("inner.zip");
    s.keys(&["End", "Enter"]);
    s.wait("inner.zip:/");
    s.wait("nested_note.txt");
    // Changing it is refused, and the outer archive keeps the inner one.
    s.keys(&["End", "F8"]);
    s.wait("read-only");
    s.keys(&["Enter"]);
    s.wait_gone("read-only");
    s.keys(&["F3"]);
    s.wait("from the inner archive");
    s.keys(&["Escape"]);
    s.wait_gone("from the inner archive");
    // `..` goes back to the outer archive, with the cursor on the inner one.
    s.keys(&["Home", "Enter"]);
    s.wait_gone("nested_note.txt");
    s.wait("outer.zip:/");
    let names: Vec<String> =
        far_linux::archive::list(&s.p("left/outer.zip"), far_linux::archive::Format::Zip)
            .unwrap()
            .into_iter()
            .map(|e| e.path)
            .collect();
    assert_eq!(names, ["inner.zip"]);
}

#[test]
fn alt_f7_results_window_views_and_starts_a_new_search() {
    let Some(s) = Session::start(|root| {
        fs::create_dir(root.join("left/logs")).unwrap();
        fs::write(root.join("left/logs/first.log"), "first log text").unwrap();
        fs::write(root.join("left/logs/second.log"), "second log text").unwrap();
    }) else {
        return;
    };
    s.wait("logs");
    s.keys(&["M-F7"]);
    s.wait("File mask");
    s.text("*.log");
    s.keys(&["Enter"]);
    s.wait("Search done. Found files: 2, folders: 0");
    s.wait("│first.log");
    s.wait("│second.log");
    // F3 views the file under the cursor and comes back to the results.
    s.keys(&["Down", "F3"]);
    s.wait("second log text");
    s.keys(&["Escape"]);
    s.wait_gone("second log text");
    s.wait("Search done. Found files: 2, folders: 0");
    // New search goes back to the form.
    s.keys(&["M-n"]);
    s.wait("File mask");
    s.keys(&["Escape"]);
    s.wait_gone("File mask");
}

#[test]
fn f4_in_find_results_edits_an_archive_entry_and_returns() {
    use std::io::{Read, Write};
    let Some(s) = Session::start(|root| {
        let file = fs::File::create(root.join("left/notes.zip")).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("docs/todo.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"buy milk").unwrap();
        zip.finish().unwrap();
    }) else {
        return;
    };
    s.wait("notes.zip");
    s.keys(&["End", "Enter"]);
    s.wait("notes.zip:/");
    s.keys(&["M-F7"]);
    s.wait("File mask");
    s.text("*.txt");
    s.keys(&["Enter"]);
    s.wait("Search done. Found files: 1, folders: 0");
    s.keys(&["F4"]);
    s.wait("buy milk");
    s.keys(&["End"]);
    s.text(" now");
    s.keys(&["F2"]);
    s.keys(&["Escape"]);
    s.wait("Update it in the archive?");
    s.keys(&["Enter"]);
    // Back in the results window, with the archive changed.
    s.wait("Search done. Found files: 1, folders: 0");
    let start = Instant::now();
    let read = || -> String {
        let mut zip = zip::ZipArchive::new(fs::File::open(s.p("left/notes.zip")).unwrap()).unwrap();
        let mut text = String::new();
        zip.by_name("docs/todo.txt")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        text
    };
    while read() != "buy milk now" {
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "{:?}\n{}",
            read(),
            s.screen()
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    s.keys(&["Escape"]);
    s.wait_gone("Search done");
}

#[test]
fn find_file_mask_shows_matching_history_while_typing() {
    let Some(s) = Session::start(|root| {
        fs::write(root.join("left/app.log"), "x").unwrap();
    }) else {
        return;
    };
    s.wait("app.log");
    s.keys(&["M-F7"]);
    s.wait("File mask");
    s.text("*.log");
    s.keys(&["Enter"]);
    // "Found" is in both the old results menu and Far's results window.
    s.wait("Found");
    s.keys(&["Escape"]);
    s.wait_gone("Found");
    // The mask is prefilled; typing replaces it and lists the earlier mask.
    s.keys(&["M-F7"]);
    s.wait("File mask");
    s.keys(&["C-y"]);
    s.text("*.l");
    s.wait("│*.log");
    s.keys(&["Down", "Enter"]);
    // Enter took the entry and left the dialog open.
    s.wait_gone("│*.log");
    s.wait("File mask");
    assert!(s.screen().contains("*.log"), "{}", s.screen());
    s.keys(&["Enter"]);
    s.wait("Found");
}

#[test]
fn shift_f1_splits_into_volumes_that_open_and_extract() {
    let Some(s) = Session::start(|root| {
        fs::create_dir(root.join("left/data")).unwrap();
        // Data that does not compress, so that the archive needs volumes.
        let mut seed = 11u64;
        let noise: Vec<u8> = (0..6000)
            .map(|_| {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (seed >> 56) as u8
            })
            .collect();
        fs::write(root.join("left/data/noise.bin"), &noise).unwrap();
        fs::write(root.join("left/data/readme.txt"), "volume test").unwrap();
    }) else {
        return;
    };
    s.wait("data");
    s.keys(&["Down", "S-F1"]);
    s.wait("Split into volumes");
    // Path, Append extension, archiver, level, Split into volumes, size.
    s.keys(&["Down", "Down", "Down", "Down", "Space", "Down", "C-y"]);
    s.text("1K");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("left/data.zip.001"), true);
    s.wait("data.zip.004");
    assert!(!s.p("left/data.zip").exists());
    let hidden: Vec<String> = fs::read_dir(s.p("left"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.'))
        .collect();
    assert!(hidden.is_empty(), "{hidden:?}");
    // Enter on the first volume opens the whole archive, read-only.
    s.keys(&["Home", "Down", "Down", "Enter"]);
    s.wait("data.zip.001:/");
    s.keys(&["Down", "Enter"]);
    s.wait("readme.txt");
    s.keys(&["End", "F8"]);
    s.wait("A split archive is read-only");
    s.keys(&["Enter"]);
    s.wait_gone("read-only");
    // Out again, with the cursor on the first volume, and Shift+F2 extracts.
    s.keys(&["Home", "Enter"]);
    s.keys(&["Home", "Enter"]);
    s.wait_gone("data.zip.001:/");
    s.keys(&["S-F2"]);
    s.wait("Extract to");
    s.keys(&["Enter"]);
    s.wait_file(&s.p("right/data/readme.txt"), true);
    let start = Instant::now();
    while fs::read_to_string(s.p("right/data/readme.txt")).unwrap_or_default() != "volume test" {
        assert!(start.elapsed() < Duration::from_secs(8), "{}", s.screen());
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn f4_edits_a_utf16_file_and_saves_it_as_utf16() {
    let Some(s) = Session::start(|root| {
        let mut bytes = vec![0xff, 0xfe];
        for unit in "Привет, Far\r\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        fs::write(root.join("left/wide.txt"), bytes).unwrap();
    }) else {
        return;
    };
    s.wait("wide.txt");
    s.keys(&["Down", "F4"]);
    s.wait("Привет, Far");
    s.wait("UTF-16LE");
    s.text("ok ");
    s.wait("ok Привет");
    s.keys(&["F2"]);
    let mut want = vec![0xff, 0xfe];
    for unit in "ok Привет, Far\r\n".encode_utf16() {
        want.extend_from_slice(&unit.to_le_bytes());
    }
    let start = Instant::now();
    while fs::read(s.p("left/wide.txt")).unwrap() != want {
        assert!(start.elapsed() < Duration::from_secs(8), "{}", s.screen());
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn enter_opens_documents_in_their_application_and_runs_scripts() {
    use std::os::unix::fs::PermissionsExt;
    // Stand-ins for the desktop's openers record what they were asked to open.
    let tools =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("openers_{}", std::process::id()));
    fs::create_dir_all(&tools).unwrap();
    let log = tools.join("opened.log");
    for name in ["wslview", "xdg-open"] {
        let script = format!("#!/bin/sh\nprintf '%s\\n' \"$1\" >> {}\n", log.display());
        let path = tools.join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let env = format!(
        "PATH={}:/usr/bin:/bin DISPLAY=:99 WSL_DISTRO_NAME=test",
        tools.display()
    );
    let Some(s) = Session::start_with(&env, |root| {
        let left = root.join("left");
        // Execute bits alone, as on a Windows drive, do not make a program.
        fs::write(left.join("b_notes.txt"), "notes\n").unwrap();
        fs::set_permissions(left.join("b_notes.txt"), fs::Permissions::from_mode(0o777)).unwrap();
        fs::write(left.join("c_run.sh"), "#!/bin/sh\necho ran > ran.txt\n").unwrap();
        fs::set_permissions(left.join("c_run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    }) else {
        return;
    };
    s.wait("b_notes.txt");
    s.keys(&["Down", "Enter"]);
    s.wait_file(&log, true);
    let opened = || fs::read_to_string(&log).unwrap_or_default();
    let start = Instant::now();
    while !opened().contains("b_notes.txt") {
        assert!(start.elapsed() < Duration::from_secs(5), "{}", opened());
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(opened().ends_with("left/b_notes.txt\n"), "{}", opened());
    // The script runs instead.
    s.keys(&["Down", "Enter"]);
    s.wait_file(&s.p("left/ran.txt"), true);
    let _ = fs::remove_dir_all(&tools);
}
