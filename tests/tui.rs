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
    s.wait("Archive name");
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
    s.wait("Archive name");
    s.keys(&["M-f"]);
    s.wait("Format");
    s.keys(&["Down", "Enter"]);
    s.wait("note.txt.tar.gz");
    s.keys(&["Tab", "Tab", "Enter"]);
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
    s.wait("Archive name");
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
    s.wait("22 345 bytes");
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
    s.wait("4 321 bytes");
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
        let shell = s.tmux(&["display", "-p", "-t", "t", "#{pane_pid}"]);
        let far = Command::new("pgrep")
            .args(["-P", shell.trim(), "far"])
            .output()
            .expect("pgrep runs");
        assert!(far.stdout.is_empty(), "far is still running");
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
