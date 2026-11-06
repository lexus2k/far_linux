# Changelog

## Unreleased

### Added

- `far:about` and F9, Commands, Version information show the version, the
  architecture and the libraries in use, as Far's `far:about` does. The version
  is also printed on the console at start, where `Ctrl+O` shows it.
- Ubuntu packages for amd64 and arm64, built and published as a GitHub release
  by a workflow started by hand.
- The editor colours source code by the file's name or `#!` line, using the
  `syntect` grammars and the colours of Colorer's default console scheme.
- Text files and logs get the colours of Colorer's text type: numbers, dates,
  times, addresses, strings and symbols. Android logcat lines also show their
  level and tag in a colour for the level.
- The key bar shows Far's Shift, Alt and Ctrl labels while the key is held, in
  panels, the viewer and the editor. This needs Windows Terminal's input mode.
- `Tab` completes file and command names on the `Ctrl+O` console.
- Typing `exit` on the command line or the console ends far.
- Far's default highlight groups: hidden files, archives and temporary files have
  their own colours, also under the cursor. Added groups colour images, media,
  documents and source code.
- Highlight groups are kept in `~/.config/far_linux/highlight.ini`, with Far's masks
  and colours for normal, cursor and selected names. `F9`, Commands, Files
  highlighting opens it.
- `Ctrl+C` clears the command line, starts a new prompt on the console, and cancels
  a running operation.
- Deleting a folder with something in it asks first, with `Delete`, `All`, `Skip`,
  `Skip all` and `Cancel`, as Far does. A permanent delete asks the same about
  read-only files.
- Archives: zip, tar, tar.gz, tar.bz2, tar.xz and 7z. Enter or `Ctrl+PgDn` opens
  one like a folder. `Shift+F1` adds, `Shift+F2` extracts and `Shift+F3` tests.
  Inside an archive, `F3` and `F4` work on a temporary copy, `F5` and `F6`
  extract, and `F7` and `F8` change it. With an archive on the other panel, `F5`
  and `F6` add to it.
- History in a SQLite file, kept between runs: commands, folders, viewed and
  edited files, search and replace text, search options, masks and targets.
  `Ctrl+Up` and `Ctrl+Down` open the list in dialogs that support it, with `Ins`
  to lock, `Shift+Del` to remove and `Del` to clear.
- Editor search and replace as in Far 3: case sensitive, whole words, fuzzy,
  regular expressions, preserve style, Word and Selection buttons, find or
  replace previous, and a list of all matches. `Shift+F7` and `Alt+F7` continue
  the last search or replace.
- Editor keys from Far 3, including bookmarks, `Ctrl+Q`, `Alt+I`, `Alt+U`,
  `Ctrl+L`, `Shift+Enter`, `Ctrl+F` and F1 help. Go to accepts relative,
  percent and hex values.
- Viewer keys from Far 3, including word wrap, dump mode, bytes per row, next
  and previous file, bookmarks and position undo. The search dialog has the same
  options as the editor's, plus hex.
- `far --keys` shows which keys a terminal delivers.
- Two file panels with sorting, column views, selection and quick search.
- File operations with progress, cancel, overwrite and error prompts: copy,
  move, rename, trash, permanent delete, make folder, links, permissions and
  owner, find files and folder size.
- Console on `Ctrl+O`, with `cd` and exported variables kept between commands.
- Viewer for files of any size, with wrap, hex, search and encodings.
- Editor with undo, stream and vertical blocks, search and replace, and
  encoding and line ending preservation.

### Changed

- `F3` on a folder shows its size in the panel instead of a dialog, as in Far. On
  `..` it counts the current folder, and it counts all selected folders.
- Colours no longer depend on the terminal's scheme where 256 colours exist, so
  selected names and column titles are really yellow and executables bright green.
- The progress box of file operations keeps one size.
- `Shift+F8` deletes the item under the cursor to the trash, as in Far 3. Only
  `Shift+Del` skips the trash.
- Editor `F4` quits, and `Shift+Enter` inserts the panel's file name. Viewer
  Left and Right move one column, and Space continues the search.

### Fixed

- Pasting in Windows Terminal keeps line ends. A paste can arrive as key events,
  with a line feed as Ctrl+Enter, or as plain text with its line ends as key
  events. Both lost them, and the second put the key events in the text.
- `far --keys` shows pasted text, as it now turns on bracketed paste.
- `Ctrl+Enter` and `Ctrl+J` start a new line in the editor, as ConPTY can deliver
  a pasted line feed as these keys.
- A paste that stalls is waited for longer, and its late rest is still taken as
  pasted text rather than keys, so its line ends cannot run commands. Control
  characters such as Esc pressed during a paste are left out of it.
- Saving through a dangling symbolic link writes to its target instead of replacing
  the link.
- A failed Save As keeps the original path, so `F2` still saves to the old file.
- `Shift+F5` and `Shift+F6` on a file literally named `~` stay in the folder instead
  of going to the home folder.
- Copying with "contents of symbolic links" stops at a link back to a folder being
  copied.
- A symbolic link copied or moved onto the file it points to destroyed the file.
- A failed in-place save lost the original. The old contents now go back.
- Any failure to create the temporary file, such as a full disk, fell through to
  an in-place save. Only a permission error does now.
- Saving changed the file's group and silently replaced read-only files.
- "Apply to folder contents" applied prefilled values to every item.
- `Shift+F5` and `Shift+F6` with an unedited non-UTF-8 name renamed the file.
- A move across file systems removed the source before syncing the copy.
- Copies and saves kept setuid and setgid bits.
- Folder and command names could send escape codes to the console.
- Crashes: enlarging the terminal with a scrolled menu open, menus on very
  narrow terminals, non-UTF-8 command line arguments, and `F8` in the editor
  after the file was deleted.
- The trash ignored `XDG_DATA_HOME`.
