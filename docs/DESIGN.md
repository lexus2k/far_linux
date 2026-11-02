# Design

far_linux follows Far Manager 3's keys and behaviour on Linux terminals. Far's
own help is the reference for keys, not memory: an early version of the editor
was built from memory and missed many of them.

## Structure

```
main loop (app.rs) ── panels (panel.rs) ── directory or archive listing
    │
    ├─ viewer.rs, editor.rs ── search.rs     text search with Far's options
    ├─ dialog.rs ── history.rs               forms with history lists
    └─ run_op (worker thread)
           ├─ fsops.rs                       file operations
           └─ archive.rs                     archive operations
```

`fsops.rs` and `archive.rs` have no UI code. They report through `OpUi`, which
`run_op` connects to dialogs over channels. This is also what makes them testable
with a scripted `OpUi`.

## Decisions

**Archives are a panel mode.** `Panel::archive` holds the open file, the folder
inside it and its listing. `Panel::path` stays the folder holding the archive,
because shell commands need a real working directory. Commands that make no sense
inside an archive are refused in `App::perform`.

**Zip, tar and gzip run in-process; other formats use tools.** The Rust crates
need no installed programs for the common formats. Bzip2 and xz are piped through
the system tools, and 7z is driven through its command line, because no pure Rust
writer was needed badly enough to add one. A missing tool gives a message, not a
failure at build time.

**Archive names are untrusted.** `safe_name` rejects absolute paths and `..`.
Symbolic links are created after all files, parents are checked for links before
each write, hard links must stay inside the destination, and modes are masked to
`0o777`.

**Changing an archive rewrites it.** Entries are copied to a temporary file and
renamed over the original only after full success. Zip entries are copied without
recompressing. Entries with unsafe names are kept, because refusing to extract
something is no reason to delete it from the user's archive.

**History is SQLite, not plain files.** The user asked for a database file rather
than something like Far's registry settings. `rusqlite` is bundled, so there is no
system dependency, at the cost of a C compiler at build time. The file is mode
0600 because commands can contain secrets. Any failure falls back to an in-memory
database, because history must never stop the file manager.

**Search lives in one place.** `Matcher` gives the editor and viewer the same
options. Both search line by line, so a pattern cannot match across lines. The
viewer uses a faster byte search for exact and hex text.

## Plan

Done:

1. Panels, file operations, console, viewer and editor.
2. Far 3 keys and search dialogs in the editor and viewer.
3. Archives and history.
4. Data loss and crash findings from the first independent review.

Next:

1. Audit the panel keys against Far's help, as was done for the editor and viewer.
2. Fix the known issues below.
3. Run listing and testing of archives on the worker thread so they can be
   cancelled.
4. Test keys in Windows Terminal, which is the user's terminal. Tests run in tmux
   only.

## Known issues

From the first review, not yet fixed:

- Non-UTF-8 names: a command built from one uses lossy text, `Ctrl+Enter` on a
  name with a newline can name another file, `F5` into a non-UTF-8 folder fails,
  and `cd` into one is lost between commands.
- SIGTERM and SIGHUP leave the terminal unrestored and the state folder behind,
  and that folder holds exported variables.
- Saving through a dangling symbolic link replaces the link with a file.
- Save As changes the file's path before the save has succeeded.
- `Shift+F6` on a file literally named `~` moves it to the home folder.
- `is_inside` compares paths, so it can miss on case-insensitive file systems.
- Copying with "contents of symbolic links" has no cycle detection.
- Restoring the screen after a resize during some dialogs draws at the old size.
- Wrapping a very long line is slow, and blocking files like `/proc/kmsg` can
  hang the viewer.

Other:

- Retry in an archive error dialog repeats the prompt, not the failed step,
  because archive data can be read only once.
- Keeping a file's group on save and syncing during a move have no test. The first
  needs a second group and the second needs a failing file system.
