# AGENTS.md

Guidance for coding agents working on far_linux, a Far Manager 3 style file
manager for Linux terminals. `README.md` describes user-facing behaviour.

## Commands

```bash
make                # release build, target/release/far
make install        # ~/.local/bin, or /usr/local/bin as root
cargo test          # unit tests, plus end-to-end tests that need tmux
cargo fmt
cargo clippy --all-targets
```

Run the tests and `cargo fmt` before handing work back. The end-to-end tests
pass trivially if tmux is missing, because they skip themselves.

## Layout

| File | Responsibility |
|---|---|
| `src/app.rs` | Main loop, hot keys, `Action` dispatch, file operation dialogs, F9 menu |
| `src/fsops.rs` | Copy, move, delete, trash, attributes, find. No UI code |
| `src/archive.rs` | List, extract, create and change archives. No UI code |
| `src/highlight.rs` | Far's highlighting groups: masks, attributes, colours and their settings file |
| `src/history.rs` | SQLite history and remembered options |
| `src/search.rs` | Search engine shared by editor and viewer: regex, whole words, fuzzy, preserve style |
| `src/panel.rs` | Directory and archive listing, sorting, selection, panel drawing |
| `src/viewer.rs` | F3 viewer. Reads on demand and never loads the whole file |
| `src/editor.rs` | F4 editor: buffer, undo, blocks, search, saving |
| `src/syntax.rs` | Syntax colours for the editor, from `syntect` grammars with Colorer's default colours |
| `src/shell.rs` | Runs commands in bash and carries cwd and exports between them |
| `src/screen.rs` | Cell buffer, terminal setup, key normalization, theme |
| `src/input.rs` | Windows Terminal's input mode, which reports modifier keys alone |
| `src/dialog.rs` | Message boxes, forms, menus |
| `src/lineedit.rs` | One-line text editing for the command line and dialogs |
| `src/encoding.rs` | Encodings cycled with F8 |
| `src/util.rs` | Wildcards, formatting, path helpers |
| `tests/tui.rs` | End-to-end tests that drive the real binary in tmux |

## Rules that protect user data

- Never write directly over a copy destination. `copy_file` writes a temporary
  file and renames it, so a cancelled or failed copy leaves the old file intact.
- A move across file systems removes the source only if nothing was skipped.
- Delete must not follow symbolic links.
- The editor must save unchanged text back byte for byte. Extend
  `untouched_files_save_back_byte_for_byte` when changing `decode_file` or
  `encode_file`.
- Refuse to save text an encoding cannot represent. Do not substitute characters.
- Use `OsString` and `Path` for file operations. `Entry::name` is lossy and is
  for display only.

- Archive contents are untrusted. Entry names go through `safe_name`, and
  extraction must not write through a symbolic link or create setuid files. The
  hostile archive tests in `archive.rs` cover this.
- Changing an archive writes a new file and renames it, and keeps entries that
  this program refuses to extract.
- Move into an archive removes the sources only after nothing was skipped and
  the archive tests clean.
- A symbolic link must never replace the file it points to.

Add a test for any change to these paths. In `fsops.rs`, use the scripted
`OpUi` implementation.

## Design constraints

- `fsops.rs` stays free of UI code. It reports through the `OpUi` trait and runs
  on a worker thread. `run_op` in `app.rs` connects it to the dialogs.
- Keep logic in methods that don't need a terminal, as `Viewer` and `Editor` do,
  so it can be unit tested. Only the `run` functions touch `Tui`.
- Everything drawn goes through `Screen::put`, which replaces control characters.
  This stops a file name from sending escape codes to the terminal.
- Add commands as an `Action` and bind them in `key_action`. Add them to the F9
  menu and the `HELP` table too, because terminals may not deliver every key.
- Match the keys of Far Manager 3. Record intentional differences in `README.md`.
  Check keys against Far's own help (`FarEng.hlf`), not from memory. Far's help
  on this machine is at `/mnt/c/Program Files/Far Manager/FarEng.hlf`.
- See `docs/DESIGN.md` for the design, plan and known issues. Keep it current.

## Testing notes

- Keys pressed while `run_op` is active are discarded, except Esc. In an
  end-to-end test, wait until the dialogs are gone before sending more keys.
  File names also appear in dialogs, so seeing a name does not prove that the
  panels are back.
- The key bar is on screen in panels, so `wait("Copy ")` matches `5Copy   `.
  Wait for text only the dialog has.
- Tests set `XDG_DATA_HOME` so that they never touch the real history database.
- Terminal key handling differs between terminals. Passing in tmux does not prove
  a key works in another terminal.

## Releases

`.github/workflows/release.yml` runs only when started by hand, from Actions,
Release, Run workflow, or with `gh workflow run release.yml -f version=0.2.0`.
It runs the tests and builds `far-linux` packages for amd64 and arm64 with
`cargo deb`, cross-compiling arm64 on Ubuntu 22.04 and checking it under qemu.
Then it publishes release `v<version>` with the packages and `SHA256SUMS`.
`-f publish=false` builds and checks without publishing.

The version input overrides `Cargo.toml` only for the build. Bump `Cargo.toml`
and the `CHANGELOG.md` heading in a PR first, so that `far --version` and the
repository agree. Package metadata is under `[package.metadata.deb]`.

## Git conventions

- Commit or push only when asked.
- Use Conventional Commits, such as `fix(fsops): ...`, with a body that explains
  what changed and why, wrapped at about 72 columns.
- Do not add `Co-Authored-By` trailers or other AI attribution to commits or PRs.
- Branch from `main` as `feature/<name>` or `fix/<name>`.
- Stage files by name, not with `git add -A`.
- Update `README.md` and `CHANGELOG.md` when behaviour changes.
