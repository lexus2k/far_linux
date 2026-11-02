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
| `src/panel.rs` | Directory listing, sorting, selection, panel drawing |
| `src/viewer.rs` | F3 viewer. Reads on demand and never loads the whole file |
| `src/editor.rs` | F4 editor: buffer, undo, blocks, search, saving |
| `src/shell.rs` | Runs commands in bash and carries cwd and exports between them |
| `src/screen.rs` | Cell buffer, terminal setup, key normalization, theme |
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

## Testing notes

- Keys pressed while `run_op` is active are discarded, except Esc. In an
  end-to-end test, wait until the dialogs are gone before sending more keys.
  File names also appear in dialogs, so seeing a name does not prove that the
  panels are back.
- Terminal key handling differs between terminals. Passing in tmux does not prove
  a key works in another terminal.

## Git conventions

- Commit or push only when asked.
- Use Conventional Commits, such as `fix(fsops): ...`, with a body that explains
  what changed and why, wrapped at about 72 columns.
- Do not add `Co-Authored-By` trailers or other AI attribution to commits or PRs.
- Branch from `main` as `feature/<name>` or `fix/<name>`.
- Stage files by name, not with `git add -A`.
- Update `README.md` and `CHANGELOG.md` when behaviour changes.
