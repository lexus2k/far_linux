# far_linux

A two-panel file manager for Linux terminals that follows the hot keys and
workflow of [Far Manager 3](https://www.farmanager.com/). It is a new program
written in Rust, not a port, and it does not load Far plugins.

```
╔n═══════════════ ~/projects ════════════════╗╔n═══════════════ ~/backup ═════════════════╗
║        Name         │        Name          ║║        Name         │        Name          ║
║..                   │                      ║║..                   │                      ║
║far_linux            │                      ║║notes.txt            │                      ║
╟─────────────────────┴──────────────────────╢╟─────────────────────┴──────────────────────╢
║..                                        Up║║..                                        Up║
╚═══ 0 bytes, 0 files, 1 folders, 907G free ═╝╚═══ 12 bytes, 1 files, 0 folders, 907G free ╝
~/projects$
1Help   2       3View   4Edit   5Copy   6RenMov 7MkFold 8Delete 9Menu   10Quit  11      12
```

## Install

Requires a recent stable Rust. `bash` is used to run commands.

```bash
make && make install        # to ~/.local/bin
make && sudo make install   # to /usr/local/bin
```

`make install PREFIX=/some/path` chooses another location, and `make uninstall`
removes the program, using the same rules. If the folder is not in your `PATH`,
`make install` says what to add.

```bash
far                # both panels in the current folder
far left_dir right_dir
```

## Hot keys

Press F1 in the program for the full list.

| Area | Keys |
|---|---|
| Panels | `Tab` other panel, `Enter` open or run, `Ctrl+PgUp` parent, `Ctrl+\` root, `Ctrl+U` swap |
| Console | `Ctrl+O` show or hide panels, `Ctrl+P` hide inactive panel, `Ctrl+F1` / `Ctrl+F2` hide left / right |
| File operations | `F5` copy, `F6` rename or move, `Shift+F5` / `Shift+F6` on the file under the cursor, `F7` make folder, `F8` delete to trash, `Shift+Del` delete permanently, `Alt+F6` link, `Ctrl+A` permissions and owner |
| Selection | `Ins`, `Shift+arrows`, `+` / `-` by mask, `*` invert |
| Finding | `Alt+letter` quick search, `Alt+F7` find files |
| Display | `Ctrl+F3`..`Ctrl+F11` sort, `Ctrl+F12` sort menu, `Ctrl+1`..`Ctrl+4` columns, `Ctrl+H` hidden files, `Ctrl+L` info, `Ctrl+Q` quick view, `F9` menu |
| Viewer (`F3`) | `F2` wrap, `F4` hex, `F6` edit, `F7` search, `F8` encoding, `Alt+F8` go to |
| Editor (`F4`) | `F2` save, `Shift+arrows` block, `Alt+Shift+arrows` vertical block, `Ctrl+C/X/V`, `Ctrl+Z`, `F7` search, `Ctrl+F7` replace, `F8` encoding |

## How file operations behave

- **Copying** writes to a temporary file and renames it into place, so
  cancelling or failing never damages an existing destination.
- **Copying** keeps permissions, timestamps and symbolic links. It does not keep
  ownership, hard links between files, sparse regions, extended attributes or ACLs.
- **Moving** uses a rename where possible. Across file systems it copies, then
  removes the source, and keeps the source if anything was skipped.
- **Delete** (`F8`) moves items to the freedesktop.org trash. If the trash is on
  another file system, as for `/mnt/c` under WSL, it asks before deleting permanently.
- **Saving** in the editor is atomic where possible, and files you did not change
  keep their exact bytes, including line endings and encoding.

## Differences from Far 3

- `Ctrl+O` shows the terminal's normal screen and scrollback. With `Ctrl+P`, the
  hidden panel's area is blank rather than showing command output behind it.
- Each command runs in its own `bash`. Working directory and exported variables
  carry over, and `~/.bash_aliases` is loaded, but functions and anything else
  from `~/.bashrc` are not. `Ctrl+Z` is ignored for commands.
- `Enter` on a non-executable file opens the viewer, because there is no file
  association system.

Not implemented yet: user menu (`F2`), plugins (`F11`), screens (`F12`), macros,
tree panel, file descriptions, wipe (`Alt+Del`), saved settings and history, and
editing UTF-16 files (the viewer shows them as bytes).

## Terminal support

Some key combinations depend on the terminal, and some terminals keep certain
keys for themselves, such as `Alt+F4` or `F11` in Windows Terminal. `Ctrl+1`..`4`
and `Ctrl+[` need the kitty keyboard protocol. Every panel command is also
available from the `F9` menu.

## Development

```bash
cargo test     # unit tests, plus end-to-end tests that need tmux
cargo fmt
cargo clippy --all-targets
```

The end-to-end tests in `tests/tui.rs` run the real binary in tmux. They are
skipped if tmux is not installed.
