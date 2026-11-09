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

Ubuntu packages for amd64 and arm64 are on the
[releases page](https://github.com/lexus2k/far_linux/releases). They need
Ubuntu 22.04 or later, or another system with glibc 2.34 or later.

```bash
sudo apt install ./far-linux_0.2.0-1_amd64.deb   # the file you downloaded
```

To build from source instead, you need a recent stable Rust. `bash` is used to
run commands.

```bash
make && make install        # to ~/.local/bin
make && sudo make install   # to /usr/local/bin
```

`make install PREFIX=/some/path` chooses another location, and `make uninstall`
removes the program, using the same rules. If the folder is not in your `PATH`,
`make install` says what to add.

```bash
far --version      # also far:about on the command line
far                # both panels in the current folder
far left_dir right_dir
```

## Hot keys

Press F1 in the program for the full list.

| Area | Keys |
|---|---|
| Panels | `Tab` other panel, `Enter` open or run, `Ctrl+PgUp` parent, `Ctrl+\` root, `Ctrl+U` swap |
| Console | `Ctrl+O` show or hide panels, `Ctrl+P` hide inactive panel, `Ctrl+F1` / `Ctrl+F2` hide left / right |
| File operations | `F5` copy, `F6` rename or move, `Shift+F5` / `Shift+F6` on the file under the cursor, `F7` make folder, `F8` delete to trash, `Shift+F8` delete the item under the cursor to trash, `Shift+Del` delete permanently, `Alt+F6` link, `Ctrl+A` permissions, with octal or checkboxes, and owner |
| Selection | `Ins`, `Shift+arrows`, `+` / `-` by mask, `*` invert |
| Finding | `Alt+letter` quick search, `Alt+F7` find files, inside the archive when one is open. The results window fills while searching: `Enter` goes to a file, `F3` / `F4` view or edit it, `Alt+N` new search |
| Display | `Ctrl+F3`..`Ctrl+F11` sort, `Ctrl+F12` sort menu, `Ctrl+1`..`Ctrl+4` columns, `Ctrl+H` hidden files, `Ctrl+L` info, `Ctrl+Q` quick view, `F9` menu |
| Viewer (`F3`) | `F2` wrap, `Shift+F2` word wrap, `F4` hex, `Shift+F4` mode, `F6` edit, `F7` search, `Shift+F7` or `Space` next, `Alt+F7` previous, `F8` encoding, `Alt+F8` go to, `+` / `-` next or previous file. `F1` lists all keys |
| Editor (`F4`) | `F2` save, `Shift+arrows` block, `Alt+arrows` vertical block, `Ctrl+C/X/V`, `Ctrl+Z`, `F7` search, `Ctrl+F7` replace, `Shift+F7` next, `Alt+F7` previous, `Alt+F8` go to, `F8` encoding, `Shift+F8` choose encoding, also UTF-16, `Ctrl+Shift+0..9` / `Ctrl+0..9` bookmarks. `F1` lists all keys. Source code is coloured by file type |

## Archives

Zip, tar, tar.gz, tar.bz2, tar.xz and 7z work. Tar.bz2 and tar.xz use the `bzip2`
and `xz` programs, and 7z uses `7z`. Zip, tar and tar.gz need nothing installed.

| Key | Action |
|---|---|
| `Enter`, `Ctrl+PgDn` | Open the archive under the cursor, also one inside an open archive |
| `Ctrl+PgUp` | Go up, leaving the archive from its top folder |
| `Shift+F1` | Add the selection to a new or existing archive, or split a new one into volumes |
| `Shift+F2` | Extract the chosen archives |
| `Shift+F3` | Archive commands: test the archive |
| `F3`, `F4` | Inside an archive: view or edit a temporary copy |
| `F5` | Inside an archive: extract. Otherwise, with an archive on the other panel: add to it |
| `F6` | The same, then remove the originals |
| `F7`, `F8` | Inside an archive: make a folder, delete |

`Shift+F1` opens Create archive, laid out as ArcLite's. The format comes from the
name's extension, or from the archiver chosen, which also changes the extension.
Compression level runs from Store to Ultra: bzip2 and xz cannot store, so Store is
their fastest level, and zip and gzip stop at 9. Changing an archive writes a new file and renames it into place, so a
failure leaves the original intact. Names that would leave the destination are
refused, and extraction does not follow links or create setuid files.

Split into volumes in `Shift+F1` cuts a new archive into `name.zip.001`, `.002`
and so on, of the size given, such as `700M`. It works for every format, as
7-Zip's volumes do. `Enter` on the first volume opens the joined archive,
read-only, and `Shift+F2` extracts it. Making volumes needs room for the whole
archive once more while it is cut.

An archive inside an archive opens from a temporary copy, shown as
`outer.tar:/lib/inner.zip:/`, and `..` at its top returns to the outer one. It is
read-only: view, extract and search it, or extract it to change it.

Limits: password protected archives, rar and renaming inside an archive are not
supported. Listing and testing run in the foreground and cannot be cancelled, so
a large compressed tar can pause the program. Esc does not stop a 7z command once
it has started.

## History

Commands, folders, viewed and edited files, search and replace text, search
options, masks and copy targets are kept in
`$XDG_DATA_HOME/far_linux/history.db`, or `~/.local/share/far_linux/history.db`.
Only you can read the file, because commands can contain secrets. Delete it to
clear everything. If it cannot be opened, history lasts until you quit.

In search, find, mask and copy target boxes, `Ctrl+Up` or `Ctrl+Down` opens the
list. `Enter` picks an item, `Ins` locks it, `Shift+Del` removes it and `Del`
clears everything that is not locked.

While you type in such a box, earlier entries that start with the typed text
are listed under it, as Far's AutoComplete does. `Down` and `Up` choose one,
`Enter` puts it in the box, `Esc` closes the list, `Shift+Del` removes the
entry from the history and `Ctrl+End` completes from the first match.

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

- Syntax colours come from `syntect` grammars, painted with Colorer's default
  console colours, so some details differ. Code in a Markdown fence keeps its
  language's colours, and logcat levels and tags are coloured, neither of which
  Colorer does.
- The Alt+F7 results window shows the Unix mode where Far shows Windows
  attributes, and has no Panel button for a temporary panel of the results.
- Create archive (`Shift+F1`) has no profiles, encryption, self-extracting
  archives, method, solid or advanced parameters, filter, or choice for files
  already in the archive.
- `Ctrl+A` shows Unix permission bits rather than Far's Windows attributes,
  and has no file times. Its checkboxes work as Far's do: `[?]` where the
  items differ, and only the changed bits are applied.
- `Ctrl+Enter` and `Ctrl+J` start a new line in the editor, where Far does nothing. Windows'
  ConPTY can deliver a line feed in pasted text as that key.
- `Ctrl+O` shows the terminal's normal screen and scrollback. With `Ctrl+P`, the
  hidden panel's area is blank rather than showing command output behind it.
- Each command runs in its own `bash`. Working directory and exported variables
  carry over, and `~/.bash_aliases` is loaded, but functions and anything else
  from `~/.bashrc` are not. `Ctrl+Z` is ignored for commands.
- `Enter` runs a file only when it has an execute bit and is a program or a
  `#!` script. Any other file opens in the application registered for its type,
  as Windows' associations open it in Far: on WSL through `wslview` or Explorer,
  elsewhere through `xdg-open` when a desktop is running. Without either it opens
  in the viewer. far has no association menu of its own. `Shift+Enter` opens the
  file or folder the same way, as Far opens a folder in Explorer.
- `exit` ends far without asking, and `exit 3` ignores the code: far itself exits
  with 0.
- `Tab` completes file and command names on the console (`Ctrl+O`), as bash does.
  With panels on screen it changes panel, as in Far.
- Files highlighting is a text file, `~/.config/far_linux/highlight.ini`, not Far's
  dialog, and `F9`, Commands, Files highlighting opens it. It has Far's masks,
  including `<name>` and `|` to exclude, and its four colours, but no regular
  expression masks, marking character or "continue processing". Deleting the file restores the defaults.
- The default groups are Far's: hidden (names starting with a dot), folders, `<arc>`,
  `<temp>` and executables, plus links, images, media, documents and source. Linux
  uses the execute permission rather than Far's `<exec>` masks, and it comes after
  archives, temporary files, images, media and documents, because Windows drives
  mark every file executable.
- `Ctrl+C` clears the command line and cancels a running operation, as in a shell.
  Far's panels have no such key.
- `F3` on a folder shows the size in the Size column and the panel footer. Any
  re-read of the panel clears it, so that it never shows a stale number. It does
  not work inside an archive yet.
- A read-only file asks before a permanent delete only, because the trash can
  bring it back.

Not implemented yet: user menu (`F2`), plugins (`F11`), screens (`F12`), macros,
tree panel, file descriptions, wipe (`Alt+Del`), saved settings, the editor's
persistent blocks (`Ctrl+P`, `Ctrl+M`), and
showing UTF-16 files as text in the viewer, which shows them as bytes.

## Terminal support

Run `far --keys` to see what your terminal sends for a key, such as `Shift+F7`.

The key bar changes while `Shift`, `Alt` or `Ctrl` is held only where the terminal
reports those keys alone. far uses Windows Terminal's win32-input-mode for this,
if the terminal confirms it at startup. `far --keys` says whether it is on and
prints `Held:` lines. Set `FAR_WIN32_INPUT=0` to turn it off, or `1` to skip the
check. Inside tmux or screen, or in other terminals, the key bar stays as it is.

Colours do not use the terminal's colour scheme where it has 256 colours, because
a scheme can redefine bright yellow to look brown.

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
skipped if tmux is not installed. See [AGENTS.md](AGENTS.md) for the code layout
and project rules.

## Licence

MIT, see [LICENSE](LICENSE). The syntax colours follow the default console
scheme and text rules of the [Colorer](https://github.com/colorer/Colorer-library)
library, which Far's Colorer plugin uses, and the grammars come from
[syntect](https://github.com/trishume/syntect).
