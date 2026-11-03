//! File operations. This module has no UI: decisions and progress go through
//! the [`OpUi`] trait, so everything here runs on a worker thread and can be
//! tested with a scripted implementation.

use crate::util;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const COPY_BUFFER: usize = 1024 * 1024;
const EXDEV: i32 = libc::EXDEV;

/// Answer to "the destination already exists".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverwriteChoice {
    Overwrite,
    OverwriteAll,
    Skip,
    SkipAll,
    Cancel,
}

/// Answer to a failed step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorChoice {
    Retry,
    Skip,
    SkipAll,
    Cancel,
}

/// Answer to "this cannot go to the trash".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrashChoice {
    Delete,
    DeleteAll,
    Skip,
    Cancel,
}

/// Why a delete needs confirming, as Far asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteAsk {
    /// A selected folder that has something in it. `to_trash` tells whether it
    /// goes to the trash or is removed for good.
    NonEmptyFolder { to_trash: bool },
    /// A file without write permission.
    ReadOnly,
}

/// Answer to a delete confirmation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteChoice {
    Delete,
    All,
    Skip,
    SkipAll,
    Cancel,
}

/// A progress snapshot.
#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub from: PathBuf,
    pub to: PathBuf,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub items_done: u64,
    pub items_total: u64,
}

/// How an operation talks to the user.
pub trait OpUi {
    fn cancelled(&self) -> bool;
    fn progress(&mut self, progress: &Progress);
    fn overwrite(&mut self, src: &Path, dst: &Path) -> OverwriteChoice;
    fn error(&mut self, path: &Path, message: &str) -> ErrorChoice;
    fn trash_fallback(&mut self, path: &Path) -> TrashChoice;
    fn confirm_delete(&mut self, path: &Path, ask: DeleteAsk) -> DeleteChoice;
}

/// Final tally of an operation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpSummary {
    pub done: u64,
    pub skipped: u64,
    pub cancelled: bool,
}

/// Options for copy and move.
#[derive(Clone, Debug, Default)]
pub struct CopyOptions {
    /// Copy what symbolic links point to, instead of the links themselves.
    pub follow_symlinks: bool,
}

pub(crate) struct Cancelled;
pub(crate) type Step<T> = Result<Option<T>, Cancelled>;

pub(crate) struct Ctx<'a, U: OpUi> {
    pub(crate) ui: &'a mut U,
    pub(crate) progress: Progress,
    pub(crate) overwrite_all: Option<bool>,
    skip_errors: bool,
    delete_all: bool,
    /// Remembered "All" or "Skip all" for non-empty folders.
    folders_all: Option<bool>,
    /// Remembered "All" or "Skip all" for read-only files.
    read_only_all: Option<bool>,
    /// Whether a read-only file needs confirming. Only a permanent delete asks.
    ask_read_only: bool,
    pub(crate) skipped: u64,
    pub(crate) done: u64,
    follow: bool,
    /// Folders being copied when following links, to catch a link back to one.
    follow_stack: Vec<(u64, u64)>,
    durable: bool,
}

impl<'a, U: OpUi> Ctx<'a, U> {
    pub(crate) fn new(ui: &'a mut U, follow: bool) -> Self {
        Ctx {
            ui,
            progress: Progress::default(),
            overwrite_all: None,
            skip_errors: false,
            delete_all: false,
            folders_all: None,
            read_only_all: None,
            ask_read_only: false,
            skipped: 0,
            done: 0,
            follow,
            follow_stack: Vec::new(),
            durable: false,
        }
    }

    pub(crate) fn check(&self) -> Result<(), Cancelled> {
        if self.ui.cancelled() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }

    /// Runs `f`, asking the user what to do when it fails. `Ok(None)` means
    /// the step was skipped.
    pub(crate) fn attempt<T>(
        &mut self,
        path: &Path,
        mut f: impl FnMut() -> io::Result<T>,
    ) -> Step<T> {
        loop {
            self.check()?;
            match f() {
                Ok(v) => return Ok(Some(v)),
                Err(e) => {
                    if self.skip_errors {
                        self.skipped += 1;
                        return Ok(None);
                    }
                    match self.ui.error(path, &e.to_string()) {
                        ErrorChoice::Retry => continue,
                        ErrorChoice::Skip => {
                            self.skipped += 1;
                            return Ok(None);
                        }
                        ErrorChoice::SkipAll => {
                            self.skip_errors = true;
                            self.skipped += 1;
                            return Ok(None);
                        }
                        ErrorChoice::Cancel => return Err(Cancelled),
                    }
                }
            }
        }
    }

    /// Reports a failure that already happened. `Ok(true)` means try again.
    pub(crate) fn ask_retry(&mut self, path: &Path, message: &str) -> Result<bool, Cancelled> {
        self.check()?;
        if self.skip_errors {
            self.skipped += 1;
            return Ok(false);
        }
        match self.ui.error(path, message) {
            ErrorChoice::Retry => Ok(true),
            ErrorChoice::Skip => {
                self.skipped += 1;
                Ok(false)
            }
            ErrorChoice::SkipAll => {
                self.skip_errors = true;
                self.skipped += 1;
                Ok(false)
            }
            ErrorChoice::Cancel => Err(Cancelled),
        }
    }

    /// Decides whether an existing destination may be replaced.
    pub(crate) fn may_overwrite(&mut self, src: &Path, dst: &Path) -> Result<bool, Cancelled> {
        if let Some(all) = self.overwrite_all {
            if !all {
                self.skipped += 1;
            }
            return Ok(all);
        }
        match self.ui.overwrite(src, dst) {
            OverwriteChoice::Overwrite => Ok(true),
            OverwriteChoice::OverwriteAll => {
                self.overwrite_all = Some(true);
                Ok(true)
            }
            OverwriteChoice::Skip => {
                self.skipped += 1;
                Ok(false)
            }
            OverwriteChoice::SkipAll => {
                self.overwrite_all = Some(false);
                self.skipped += 1;
                Ok(false)
            }
            OverwriteChoice::Cancel => Err(Cancelled),
        }
    }

    pub(crate) fn report(&mut self) {
        self.ui.progress(&self.progress);
    }

    pub(crate) fn finish(self, result: Result<(), Cancelled>) -> OpSummary {
        OpSummary {
            done: self.done,
            skipped: self.skipped,
            cancelled: result.is_err(),
        }
    }
}

fn is_same_file(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// Returns true when `child` is `parent` or lies inside it.
fn is_inside(child: &Path, parent: &Path) -> bool {
    let parent = fs::canonicalize(parent).unwrap_or_else(|_| util::normalize(parent));
    // The destination may not exist yet: resolve its nearest existing ancestor.
    let mut probe = util::normalize(child);
    let mut tail = Vec::new();
    let resolved = loop {
        match fs::canonicalize(&probe) {
            Ok(p) => break p,
            Err(_) => match (probe.parent().map(Path::to_path_buf), probe.file_name()) {
                (Some(up), Some(name)) => {
                    tail.push(name.to_os_string());
                    probe = up;
                }
                _ => break probe.clone(),
            },
        }
    };
    let mut full = resolved;
    for part in tail.iter().rev() {
        full.push(part);
    }
    full.starts_with(&parent)
}

/// Where one source ends up, following Far's rule: an existing directory,
/// a trailing slash or several sources all mean "put it inside the target".
pub fn resolve_dest(src: &Path, target: &str, base: &Path, multiple: bool) -> PathBuf {
    let dir_hint = target.ends_with('/');
    let target = util::normalize(&util::resolve_path(base, target));
    if dir_hint || multiple || target.is_dir() {
        target.join(src.file_name().unwrap_or_else(|| OsStr::new("")))
    } else {
        target
    }
}

/// Totals for progress reporting and for F3 on a folder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

/// Recursively totals a path, without following symbolic links.
pub fn scan(path: &Path, stop: &dyn Fn() -> bool, totals: &mut Totals) {
    if stop() {
        return;
    }
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.is_dir() {
        totals.dirs += 1;
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                scan(&entry.path(), stop, totals);
            }
        }
    } else {
        totals.files += 1;
        totals.bytes += meta.len();
    }
}

fn prepare<U: OpUi>(ctx: &mut Ctx<U>, sources: &[PathBuf]) {
    let mut totals = Totals::default();
    let ui = &*ctx.ui;
    let stop = || ui.cancelled();
    for src in sources {
        scan(src, &stop, &mut totals);
    }
    ctx.progress.bytes_total = totals.bytes;
    ctx.progress.items_total = totals.files + totals.dirs;
}

/// Copies sources according to `target`.
pub fn copy_items<U: OpUi>(
    sources: &[PathBuf],
    target: &str,
    base: &Path,
    options: &CopyOptions,
    ui: &mut U,
) -> OpSummary {
    let mut ctx = Ctx::new(ui, options.follow_symlinks);
    prepare(&mut ctx, sources);
    let multiple = sources.len() > 1;
    let mut run = || -> Result<(), Cancelled> {
        for src in sources {
            let dst = resolve_dest(src, target, base, multiple);
            copy_one(&mut ctx, src, &dst)?;
        }
        Ok(())
    };
    let result = run();
    ctx.finish(result)
}

pub(crate) fn temp_name(dst: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!(".far_tmp_{}_{n}", std::process::id());
    dst.with_file_name(name)
}

pub(crate) fn set_times(path: &Path, meta: &Metadata) -> io::Result<()> {
    let times = [
        libc::timespec {
            tv_sec: meta.atime() as libc::time_t,
            tv_nsec: meta.atime_nsec() as _,
        },
        libc::timespec {
            tv_sec: meta.mtime() as libc::time_t,
            tv_nsec: meta.mtime_nsec() as _,
        },
    ];
    let c = cstring(path)?;
    // SAFETY: `c` is NUL terminated and `times` has the two required entries.
    let rc = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn copy_one<U: OpUi>(ctx: &mut Ctx<U>, src: &Path, dst: &Path) -> Result<(), Cancelled> {
    ctx.check()?;
    ctx.progress.from = src.to_path_buf();
    ctx.progress.to = dst.to_path_buf();
    ctx.report();
    let follow = ctx.follow;
    let Some(meta) = ctx.attempt(src, || {
        if follow {
            fs::metadata(src)
        } else {
            fs::symlink_metadata(src)
        }
    })?
    else {
        return Ok(());
    };
    let kind = meta.file_type();
    if kind.is_dir() {
        copy_dir(ctx, src, dst, &meta)
    } else if kind.is_symlink() {
        copy_link(ctx, src, dst)
    } else if kind.is_file() {
        copy_file(ctx, src, dst, &meta)
    } else {
        ctx.attempt::<()>(src, || {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "special files cannot be copied",
            ))
        })?;
        Ok(())
    }
}

fn copy_dir<U: OpUi>(
    ctx: &mut Ctx<U>,
    src: &Path,
    dst: &Path,
    meta: &Metadata,
) -> Result<(), Cancelled> {
    if is_inside(dst, src) {
        ctx.attempt::<()>(src, || {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot copy a folder into itself",
            ))
        })?;
        return Ok(());
    }
    let id = (meta.dev(), meta.ino());
    if ctx.follow && ctx.follow_stack.contains(&id) {
        ctx.attempt::<()>(src, || {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "this link leads back to a folder being copied",
            ))
        })?;
        return Ok(());
    }
    ctx.follow_stack.push(id);
    let result = copy_dir_contents(ctx, src, dst, meta);
    ctx.follow_stack.pop();
    result
}

fn copy_dir_contents<U: OpUi>(
    ctx: &mut Ctx<U>,
    src: &Path,
    dst: &Path,
    meta: &Metadata,
) -> Result<(), Cancelled> {
    let existing = fs::symlink_metadata(dst).ok();
    if let Some(existing) = &existing {
        let _ = existing;
        if !fs::metadata(dst).map(|m| m.is_dir()).unwrap_or(false) {
            ctx.attempt::<()>(dst, || {
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "a file with this name already exists",
                ))
            })?;
            return Ok(());
        }
    } else if ctx.attempt(dst, || fs::create_dir(dst))?.is_none() {
        return Ok(());
    }
    // Stay writable while filling the folder; the real mode is applied last.
    let created = existing.is_none();
    if created {
        let _ = fs::set_permissions(dst, fs::Permissions::from_mode(meta.mode() | 0o700));
    }
    let Some(names) = ctx.attempt(src, || read_names(src))? else {
        return Ok(());
    };
    for name in names {
        copy_one(ctx, &src.join(&name), &dst.join(&name))?;
    }
    if created {
        let _ = fs::set_permissions(dst, fs::Permissions::from_mode(meta.mode()));
        let _ = set_times(dst, meta);
    }
    ctx.progress.items_done += 1;
    ctx.done += 1;
    ctx.report();
    Ok(())
}

fn read_names(dir: &Path) -> io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        names.push(entry?.file_name());
    }
    names.sort();
    Ok(names)
}

/// True when `src` is a link to the regular file at `dst`. Replacing that file
/// with the link would destroy the data and leave a link pointing at itself.
fn link_points_at(src: &Path, dst: &Path) -> bool {
    let is_link = |p: &Path| fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
    if !is_link(src) || is_link(dst) {
        return false;
    }
    match (fs::metadata(src), fs::metadata(dst)) {
        (Ok(a), Ok(b)) => is_same_file(&a, &b),
        _ => false,
    }
}

fn refuse_link_onto_target<U: OpUi>(ctx: &mut Ctx<U>, src: &Path) -> Result<(), Cancelled> {
    ctx.attempt::<()>(src, || {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the link points to the destination, which would destroy it",
        ))
    })?;
    Ok(())
}

fn copy_link<U: OpUi>(ctx: &mut Ctx<U>, src: &Path, dst: &Path) -> Result<(), Cancelled> {
    let Some(link) = ctx.attempt(src, || fs::read_link(src))? else {
        return Ok(());
    };
    if link_points_at(src, dst) {
        return refuse_link_onto_target(ctx, src);
    }
    if let Ok(existing) = fs::symlink_metadata(dst) {
        if existing.is_dir() {
            ctx.attempt::<()>(dst, || {
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "a folder with this name already exists",
                ))
            })?;
            return Ok(());
        }
        if !ctx.may_overwrite(src, dst)? {
            return Ok(());
        }
    }
    let temp = temp_name(dst);
    let made = ctx.attempt(dst, || {
        std::os::unix::fs::symlink(&link, &temp)?;
        fs::rename(&temp, dst).inspect_err(|_| {
            let _ = fs::remove_file(&temp);
        })
    })?;
    if made.is_some() {
        ctx.done += 1;
    }
    ctx.progress.items_done += 1;
    ctx.report();
    Ok(())
}

fn copy_file<U: OpUi>(
    ctx: &mut Ctx<U>,
    src: &Path,
    dst: &Path,
    meta: &Metadata,
) -> Result<(), Cancelled> {
    if let Ok(existing) = fs::symlink_metadata(dst) {
        if fs::metadata(dst)
            .map(|m| is_same_file(&m, meta))
            .unwrap_or(false)
        {
            ctx.attempt::<()>(src, || {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "source and destination are the same file",
                ))
            })?;
            return Ok(());
        }
        if existing.is_dir() {
            ctx.attempt::<()>(dst, || {
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "a folder with this name already exists",
                ))
            })?;
            return Ok(());
        }
        if !ctx.may_overwrite(src, dst)? {
            return Ok(());
        }
    }
    // Write to a temporary file first: cancelling or failing midway must
    // leave any existing destination untouched.
    let temp = temp_name(dst);
    let start_bytes = ctx.progress.bytes_done;
    loop {
        ctx.progress.bytes_done = start_bytes;
        match stream_copy(ctx, src, &temp, meta) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = fs::remove_file(&temp);
                if ctx.ask_retry(src, &e.to_string())? {
                    continue;
                }
                ctx.progress.bytes_done = start_bytes + meta.len();
                ctx.progress.items_done += 1;
                ctx.report();
                return Ok(());
            }
            Err(c) => {
                let _ = fs::remove_file(&temp);
                return Err(c);
            }
        }
        break;
    }
    let finished = ctx.attempt(dst, || {
        // Not setuid or setgid: the copy may have another owner than the source.
        let _ = fs::set_permissions(&temp, fs::Permissions::from_mode(meta.mode() & 0o777));
        let _ = set_times(&temp, meta);
        fs::rename(&temp, dst)
    });
    let finished = match finished {
        Ok(finished) => finished,
        Err(cancelled) => {
            let _ = fs::remove_file(&temp);
            return Err(cancelled);
        }
    };
    if finished.is_none() {
        let _ = fs::remove_file(&temp);
    } else {
        ctx.done += 1;
    }
    ctx.progress.items_done += 1;
    ctx.report();
    Ok(())
}

fn stream_copy<U: OpUi>(
    ctx: &mut Ctx<U>,
    src: &Path,
    temp: &Path,
    meta: &Metadata,
) -> Result<io::Result<()>, Cancelled> {
    let mut input = match File::open(src) {
        Ok(f) => f,
        Err(e) => return Ok(Err(e)),
    };
    let mut output = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode_if_unix(meta.mode() & 0o777 | 0o600)
        .open(temp)
    {
        Ok(f) => f,
        Err(e) => return Ok(Err(e)),
    };
    let mut buf = vec![0u8; COPY_BUFFER];
    let mut since_report = 0usize;
    loop {
        ctx.check()?;
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Ok(Err(e)),
        };
        if let Err(e) = output.write_all(&buf[..n]) {
            return Ok(Err(e));
        }
        ctx.progress.bytes_done += n as u64;
        since_report += n;
        if since_report >= COPY_BUFFER {
            since_report = 0;
            ctx.report();
        }
    }
    // A move deletes the only other copy next, so delayed write errors, as on
    // network file systems, must surface now.
    if ctx.durable {
        if let Err(e) = output.sync_all() {
            return Ok(Err(e));
        }
    }
    Ok(Ok(()))
}

trait ModeIfUnix {
    fn mode_if_unix(&mut self, mode: u32) -> &mut Self;
}

impl ModeIfUnix for fs::OpenOptions {
    fn mode_if_unix(&mut self, mode: u32) -> &mut Self {
        use std::os::unix::fs::OpenOptionsExt;
        self.mode(mode)
    }
}

/// Moves or renames sources according to `target`.
pub fn move_items<U: OpUi>(
    sources: &[PathBuf],
    target: &str,
    base: &Path,
    ui: &mut U,
) -> OpSummary {
    let mut ctx = Ctx::new(ui, false);
    prepare(&mut ctx, sources);
    let multiple = sources.len() > 1;
    let mut run = || -> Result<(), Cancelled> {
        for src in sources {
            let dst = resolve_dest(src, target, base, multiple);
            move_one(&mut ctx, src, &dst)?;
        }
        Ok(())
    };
    let result = run();
    ctx.finish(result)
}

pub(crate) fn move_one<U: OpUi>(ctx: &mut Ctx<U>, src: &Path, dst: &Path) -> Result<(), Cancelled> {
    ctx.check()?;
    ctx.progress.from = src.to_path_buf();
    ctx.progress.to = dst.to_path_buf();
    ctx.report();
    if src == dst {
        return Ok(());
    }
    let Some(meta) = ctx.attempt(src, || fs::symlink_metadata(src))? else {
        return Ok(());
    };
    if meta.is_dir() && is_inside(dst, src) {
        ctx.attempt::<()>(src, || {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot move a folder into itself",
            ))
        })?;
        return Ok(());
    }
    if link_points_at(src, dst) {
        return refuse_link_onto_target(ctx, src);
    }
    if let Ok(existing) = fs::symlink_metadata(dst) {
        if !is_same_file(&existing, &meta) {
            if meta.is_dir() && existing.is_dir() {
                return merge_dir(ctx, src, dst);
            }
            if meta.is_dir() != existing.is_dir() {
                ctx.attempt::<()>(dst, || {
                    Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "a file and a folder cannot replace each other",
                    ))
                })?;
                return Ok(());
            }
            if !ctx.may_overwrite(src, dst)? {
                return Ok(());
            }
        }
    }
    // A plain rename is atomic. Only a different file system needs a copy.
    loop {
        match fs::rename(src, dst) {
            Ok(()) => {
                ctx.done += 1;
                ctx.progress.items_done += 1;
                ctx.progress.bytes_done += if meta.is_dir() { 0 } else { meta.len() };
                ctx.report();
                return Ok(());
            }
            Err(e) if e.raw_os_error() == Some(EXDEV) => break,
            Err(e) => {
                if !ctx.ask_retry(src, &e.to_string())? {
                    return Ok(());
                }
            }
        }
    }
    move_across(ctx, src, dst, &meta)
}

fn merge_dir<U: OpUi>(ctx: &mut Ctx<U>, src: &Path, dst: &Path) -> Result<(), Cancelled> {
    let Some(names) = ctx.attempt(src, || read_names(src))? else {
        return Ok(());
    };
    for name in names {
        move_one(ctx, &src.join(&name), &dst.join(&name))?;
    }
    // Anything skipped keeps the source folder non-empty, and so it stays.
    let _ = fs::remove_dir(src);
    Ok(())
}

/// Copies to another file system, then removes the source. The source is
/// kept if anything inside it was skipped, so no data is lost.
fn move_across<U: OpUi>(
    ctx: &mut Ctx<U>,
    src: &Path,
    dst: &Path,
    meta: &Metadata,
) -> Result<(), Cancelled> {
    // The overwrite question was already answered in move_one.
    let saved = ctx.overwrite_all;
    if fs::symlink_metadata(dst).is_ok() && !meta.is_dir() {
        ctx.overwrite_all = Some(true);
    }
    let before = ctx.skipped;
    ctx.durable = true;
    let result = copy_one(ctx, src, dst);
    ctx.durable = false;
    ctx.overwrite_all = saved;
    result?;
    if ctx.skipped != before {
        return Ok(());
    }
    let is_dir = meta.is_dir();
    ctx.attempt(src, || {
        if is_dir {
            fs::remove_dir_all(src)
        } else {
            fs::remove_file(src)
        }
    })?;
    Ok(())
}

/// Deletes sources: into the trash, or permanently.
pub fn delete_items<U: OpUi>(sources: &[PathBuf], permanent: bool, ui: &mut U) -> OpSummary {
    let mut ctx = Ctx::new(ui, false);
    ctx.ask_read_only = permanent;
    prepare(&mut ctx, sources);
    let mut run = || -> Result<(), Cancelled> {
        for src in sources {
            ctx.progress.from = src.clone();
            ctx.report();
            if is_nonempty_folder(src) {
                let ask = DeleteAsk::NonEmptyFolder {
                    to_trash: !permanent,
                };
                if !confirm(&mut ctx, src, ask)? {
                    continue;
                }
            }
            if permanent {
                remove_tree(&mut ctx, src)?;
            } else {
                trash_one(&mut ctx, src)?;
            }
        }
        Ok(())
    };
    let result = run();
    ctx.finish(result)
}

/// A symbolic link to a folder is not a folder here: deleting it removes only
/// the link.
fn is_nonempty_folder(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
        && fs::read_dir(path).is_ok_and(|mut items| items.next().is_some())
}

/// Asks about one item, unless "All" or "Skip all" already answered. Returns
/// whether to delete it, and counts it as skipped if not.
fn confirm<U: OpUi>(ctx: &mut Ctx<U>, path: &Path, ask: DeleteAsk) -> Result<bool, Cancelled> {
    let remembered = match ask {
        DeleteAsk::NonEmptyFolder { .. } => ctx.folders_all,
        DeleteAsk::ReadOnly => ctx.read_only_all,
    };
    let delete = match remembered {
        Some(answer) => answer,
        None => {
            let choice = ctx.ui.confirm_delete(path, ask);
            if choice == DeleteChoice::Cancel {
                return Err(Cancelled);
            }
            let answer = matches!(choice, DeleteChoice::Delete | DeleteChoice::All);
            if matches!(choice, DeleteChoice::All | DeleteChoice::SkipAll) {
                match ask {
                    DeleteAsk::NonEmptyFolder { .. } => ctx.folders_all = Some(answer),
                    DeleteAsk::ReadOnly => ctx.read_only_all = Some(answer),
                }
            }
            answer
        }
    };
    if !delete {
        ctx.skipped += 1;
    }
    Ok(delete)
}

fn trash_one<U: OpUi>(ctx: &mut Ctx<U>, src: &Path) -> Result<(), Cancelled> {
    let Some(home) = util::home_dir() else {
        return trash_refused(ctx, src);
    };
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map_or_else(|| home.join(".local/share"), PathBuf::from);
    let trash = data.join("Trash");
    match move_to_trash(src, &trash) {
        Ok(()) => {
            ctx.done += 1;
            ctx.progress.items_done += 1;
            ctx.report();
            Ok(())
        }
        Err(_) => trash_refused(ctx, src),
    }
}

fn trash_refused<U: OpUi>(ctx: &mut Ctx<U>, src: &Path) -> Result<(), Cancelled> {
    if !ctx.delete_all {
        match ctx.ui.trash_fallback(src) {
            TrashChoice::Delete => {}
            TrashChoice::DeleteAll => ctx.delete_all = true,
            TrashChoice::Skip => {
                ctx.skipped += 1;
                return Ok(());
            }
            TrashChoice::Cancel => return Err(Cancelled),
        }
    }
    remove_tree(ctx, src)
}

/// Moves a path to a freedesktop.org trash directory. This only succeeds
/// when the trash is on the same file system, as the specification expects.
pub fn move_to_trash(src: &Path, trash: &Path) -> io::Result<()> {
    let files = trash.join("files");
    let info = trash.join("info");
    fs::create_dir_all(&files)?;
    fs::create_dir_all(&info)?;
    let base = src
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let absolute = fs::canonicalize(src.parent().unwrap_or(Path::new("/")))?.join(base);
    let t = util::local_time(util::now_secs());
    let record = format!(
        "[Trash Info]\nPath={}\nDeletionDate={:04}-{:02}-{:02}T{:02}:{:02}:{:02}\n",
        percent_encode(absolute.as_os_str().as_bytes()),
        t.year,
        t.month,
        t.day,
        t.hour,
        t.minute,
        t.second
    );
    for n in 0..10_000u32 {
        let mut name = base.to_os_string();
        if n > 0 {
            name.push(format!(".{n}"));
        }
        let mut info_name = name.clone();
        info_name.push(".trashinfo");
        let info_path = info.join(&info_name);
        // Creating the info file exclusively is what reserves the name.
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&info_path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        file.write_all(record.as_bytes())?;
        let dest = files.join(&name);
        if fs::symlink_metadata(&dest).is_ok() {
            let _ = fs::remove_file(&info_path);
            continue;
        }
        return fs::rename(src, &dest).inspect_err(|_| {
            let _ = fs::remove_file(&info_path);
        });
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "too many items with this name in the trash",
    ))
}

fn percent_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn remove_tree<U: OpUi>(ctx: &mut Ctx<U>, path: &Path) -> Result<(), Cancelled> {
    ctx.check()?;
    let Some(meta) = ctx.attempt(path, || fs::symlink_metadata(path))? else {
        return Ok(());
    };
    if meta.is_dir() {
        let Some(names) = ctx.attempt(path, || read_names(path))? else {
            return Ok(());
        };
        let skipped_before = ctx.skipped;
        for name in names {
            remove_tree(ctx, &path.join(name))?;
        }
        // A folder that kept something cannot be removed, and that is no error.
        if ctx.skipped == skipped_before && ctx.attempt(path, || fs::remove_dir(path))?.is_some() {
            ctx.done += 1;
        }
    } else if ctx.ask_read_only
        && !meta.file_type().is_symlink()
        && meta.mode() & 0o200 == 0
        && !confirm(ctx, path, DeleteAsk::ReadOnly)?
    {
        // Counted as skipped by `confirm`.
    } else if ctx.attempt(path, || fs::remove_file(path))?.is_some() {
        ctx.done += 1;
        ctx.progress.bytes_done += meta.len();
    }
    ctx.progress.items_done += 1;
    ctx.progress.from = path.to_path_buf();
    ctx.report();
    Ok(())
}

/// Requested attribute changes.
#[derive(Clone, Debug, Default)]
pub struct AttrChange {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub recursive: bool,
}

/// Applies permission and owner changes.
pub fn set_attributes<U: OpUi>(paths: &[PathBuf], change: &AttrChange, ui: &mut U) -> OpSummary {
    let mut ctx = Ctx::new(ui, false);
    let mut run = || -> Result<(), Cancelled> {
        for path in paths {
            apply_attrs(&mut ctx, path, change)?;
        }
        Ok(())
    };
    let result = run();
    ctx.finish(result)
}

fn apply_attrs<U: OpUi>(
    ctx: &mut Ctx<U>,
    path: &Path,
    change: &AttrChange,
) -> Result<(), Cancelled> {
    let Some(meta) = ctx.attempt(path, || fs::symlink_metadata(path))? else {
        return Ok(());
    };
    ctx.progress.from = path.to_path_buf();
    ctx.report();
    let applied = ctx.attempt(path, || apply_attrs_one(path, &meta, change))?;
    if applied.is_some() {
        ctx.done += 1;
    }
    if change.recursive && meta.is_dir() {
        if let Some(names) = ctx.attempt(path, || read_names(path))? {
            for name in names {
                apply_attrs(ctx, &path.join(name), change)?;
            }
        }
    }
    Ok(())
}

fn apply_attrs_one(path: &Path, meta: &Metadata, change: &AttrChange) -> io::Result<()> {
    if change.uid.is_some() || change.gid.is_some() {
        let c = cstring(path)?;
        let uid = change.uid.unwrap_or(u32::MAX) as libc::uid_t;
        let gid = change.gid.unwrap_or(u32::MAX) as libc::gid_t;
        // SAFETY: `c` is a valid NUL terminated path; -1 leaves a field as is.
        if unsafe { libc::lchown(c.as_ptr(), uid, gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // Permissions on a symbolic link itself do not exist on Linux.
    if let (Some(mode), false) = (change.mode, meta.file_type().is_symlink()) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// Parses an octal mode such as `644` or `0755`.
pub fn parse_mode(text: &str) -> Option<u32> {
    let text = text.trim();
    if text.is_empty() || text.len() > 4 {
        return None;
    }
    u32::from_str_radix(text, 8).ok()
}

/// Looks up a user by name or numeric id.
pub fn lookup_user(text: &str) -> Option<u32> {
    if let Ok(id) = text.parse() {
        return Some(id);
    }
    let c = CString::new(text).ok()?;
    // SAFETY: getpwnam returns null or a pointer to static storage, which is
    // read immediately and only on the UI thread.
    unsafe {
        let p = libc::getpwnam(c.as_ptr());
        if p.is_null() {
            None
        } else {
            Some((*p).pw_uid)
        }
    }
}

/// Looks up a group by name or numeric id.
pub fn lookup_group(text: &str) -> Option<u32> {
    if let Ok(id) = text.parse() {
        return Some(id);
    }
    let c = CString::new(text).ok()?;
    // SAFETY: as for lookup_user.
    unsafe {
        let p = libc::getgrnam(c.as_ptr());
        if p.is_null() {
            None
        } else {
            Some((*p).gr_gid)
        }
    }
}

/// Returns the user name for an id, or the number as text.
pub fn user_name(uid: u32) -> String {
    // SAFETY: as for lookup_user.
    unsafe {
        let p = libc::getpwuid(uid);
        if p.is_null() {
            uid.to_string()
        } else {
            std::ffi::CStr::from_ptr((*p).pw_name)
                .to_string_lossy()
                .into_owned()
        }
    }
}

/// Returns the group name for an id, or the number as text.
pub fn group_name(gid: u32) -> String {
    // SAFETY: as for lookup_user.
    unsafe {
        let p = libc::getgrgid(gid);
        if p.is_null() {
            gid.to_string()
        } else {
            std::ffi::CStr::from_ptr((*p).gr_name)
                .to_string_lossy()
                .into_owned()
        }
    }
}

/// Creates a symbolic or hard link at `link`.
pub fn make_link(target: &Path, link: &Path, symbolic: bool) -> io::Result<()> {
    if symbolic {
        std::os::unix::fs::symlink(target, link)
    } else {
        fs::hard_link(target, link)
    }
}

/// Searches for files by name mask and optional content.
pub fn find_files(
    root: &Path,
    masks: &str,
    text: Option<&[u8]>,
    ignore_case: bool,
    stop: &dyn Fn() -> bool,
    found: &mut dyn FnMut(PathBuf),
    visited: &mut dyn FnMut(&Path),
) {
    if stop() {
        return;
    }
    visited(root);
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if stop() {
            return;
        }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let matches = util::wildcard_list(masks, &name.to_string_lossy());
        if matches {
            match text {
                None => found(path.clone()),
                Some(needle) => {
                    if kind.is_file() && file_contains(&path, needle, ignore_case, stop) {
                        found(path.clone());
                    }
                }
            }
        }
        if kind.is_dir() {
            find_files(&path, masks, text, ignore_case, stop, found, visited);
        }
    }
}

/// Checks whether a file contains the bytes, reading it in overlapping chunks.
pub fn file_contains(
    path: &Path,
    needle: &[u8],
    ignore_case: bool,
    stop: &dyn Fn() -> bool,
) -> bool {
    if needle.is_empty() {
        return true;
    }
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut buf = vec![0u8; COPY_BUFFER + needle.len()];
    let mut keep = 0;
    loop {
        if stop() {
            return false;
        }
        let n = match file.read(&mut buf[keep..]) {
            Ok(0) | Err(_) => return false,
            Ok(n) => n,
        };
        let filled = keep + n;
        if find_bytes(&buf[..filled], needle, ignore_case).is_some() {
            return true;
        }
        keep = (needle.len() - 1).min(filled);
        buf.copy_within(filled - keep..filled, 0);
    }
}

/// Finds a byte pattern, optionally ignoring ASCII case.
pub fn find_bytes(hay: &[u8], needle: &[u8], ignore_case: bool) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| {
        if ignore_case {
            w.eq_ignore_ascii_case(needle)
        } else {
            w == needle
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "far_test_{}_{}_{tag}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            TestDir(path)
        }
        fn p(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
        fn write(&self, rel: &str, text: &str) -> PathBuf {
            let path = self.p(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct Script {
        overwrite: OverwriteChoice,
        error: ErrorChoice,
        trash: TrashChoice,
        errors: Vec<String>,
        prompts: u32,
        cancel_after_progress: Option<u32>,
        reports: std::cell::Cell<u32>,
        confirms: Vec<(PathBuf, DeleteAsk)>,
        delete: Option<DeleteChoice>,
        /// After this many questions, answer with this instead.
        delete_after: Option<(usize, DeleteChoice)>,
    }

    impl Default for Script {
        fn default() -> Self {
            Script {
                overwrite: OverwriteChoice::Overwrite,
                error: ErrorChoice::Skip,
                trash: TrashChoice::Delete,
                errors: Vec::new(),
                prompts: 0,
                cancel_after_progress: None,
                reports: std::cell::Cell::new(0),
                confirms: Vec::new(),
                delete: None,
                delete_after: None,
            }
        }
    }

    impl OpUi for Script {
        fn cancelled(&self) -> bool {
            self.cancel_after_progress
                .is_some_and(|n| self.reports.get() >= n)
        }
        fn progress(&mut self, _: &Progress) {
            self.reports.set(self.reports.get() + 1);
        }
        fn overwrite(&mut self, _: &Path, _: &Path) -> OverwriteChoice {
            self.prompts += 1;
            self.overwrite
        }
        fn error(&mut self, _: &Path, message: &str) -> ErrorChoice {
            self.errors.push(message.to_string());
            self.error
        }
        fn trash_fallback(&mut self, _: &Path) -> TrashChoice {
            self.trash
        }
        fn confirm_delete(&mut self, path: &Path, ask: DeleteAsk) -> DeleteChoice {
            self.confirms.push((path.to_path_buf(), ask));
            match self.delete_after {
                Some((n, choice)) if self.confirms.len() > n => choice,
                _ => self.delete.unwrap_or(DeleteChoice::Delete),
            }
        }
    }

    fn base() -> PathBuf {
        PathBuf::from("/")
    }

    #[test]
    fn copy_file_to_folder_keeps_mode_and_time() {
        let d = TestDir::new("copy");
        let src = d.write("a.txt", "hello");
        fs::set_permissions(&src, fs::Permissions::from_mode(0o640)).unwrap();
        fs::create_dir(d.p("out")).unwrap();
        let mut ui = Script::default();
        let target = d.p("out").display().to_string();
        let s = copy_items(
            std::slice::from_ref(&src),
            &target,
            &base(),
            &CopyOptions::default(),
            &mut ui,
        );
        assert_eq!(s.done, 1);
        let copied = d.p("out/a.txt");
        assert_eq!(fs::read_to_string(&copied).unwrap(), "hello");
        let (a, b) = (fs::metadata(&src).unwrap(), fs::metadata(&copied).unwrap());
        assert_eq!(b.mode() & 0o777, 0o640);
        assert_eq!(a.mtime(), b.mtime());
    }

    #[test]
    fn copy_tree_preserves_symlinks_and_read_only_folders() {
        let d = TestDir::new("tree");
        d.write("src/sub/f.txt", "x");
        symlink("sub/f.txt", d.p("src/link")).unwrap();
        fs::set_permissions(d.p("src/sub"), fs::Permissions::from_mode(0o555)).unwrap();
        let mut ui = Script::default();
        let target = d.p("dst").display().to_string();
        let s = copy_items(
            &[d.p("src")],
            &target,
            &base(),
            &CopyOptions::default(),
            &mut ui,
        );
        assert!(ui.errors.is_empty(), "{:?}", ui.errors);
        assert_eq!(s.skipped, 0);
        assert_eq!(
            fs::read_link(d.p("dst/link")).unwrap(),
            Path::new("sub/f.txt")
        );
        assert_eq!(fs::read_to_string(d.p("dst/sub/f.txt")).unwrap(), "x");
        assert_eq!(fs::metadata(d.p("dst/sub")).unwrap().mode() & 0o777, 0o555);
        for sub in ["src/sub", "dst/sub"] {
            fs::set_permissions(d.p(sub), fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn follow_option_copies_link_contents() {
        let d = TestDir::new("follow");
        d.write("real.txt", "data");
        symlink(d.p("real.txt"), d.p("link")).unwrap();
        let mut ui = Script::default();
        let target = d.p("copy").display().to_string();
        let options = CopyOptions {
            follow_symlinks: true,
        };
        copy_items(&[d.p("link")], &target, &base(), &options, &mut ui);
        let meta = fs::symlink_metadata(d.p("copy")).unwrap();
        assert!(meta.is_file());
        assert_eq!(fs::read_to_string(d.p("copy")).unwrap(), "data");
    }

    #[test]
    fn following_a_link_back_to_its_own_folder_stops() {
        let d = TestDir::new("follow_cycle");
        fs::create_dir(d.p("top")).unwrap();
        d.write("top/file", "data");
        symlink(d.p("top"), d.p("top/back")).unwrap();
        let mut ui = Script::default();
        let options = CopyOptions {
            follow_symlinks: true,
        };
        let target = d.p("copy").display().to_string();
        let s = copy_items(&[d.p("top")], &target, &base(), &options, &mut ui);
        assert_eq!(ui.errors.len(), 1, "{:?}", ui.errors);
        assert_eq!(s.skipped, 1);
        assert_eq!(fs::read_to_string(d.p("copy/file")).unwrap(), "data");
        assert!(!d.p("copy/back").exists());
    }

    #[test]
    fn following_the_same_folder_by_two_links_is_not_a_cycle() {
        let d = TestDir::new("follow_diamond");
        fs::create_dir(d.p("shared")).unwrap();
        d.write("shared/file", "data");
        fs::create_dir(d.p("top")).unwrap();
        symlink(d.p("shared"), d.p("top/one")).unwrap();
        symlink(d.p("shared"), d.p("top/two")).unwrap();
        let mut ui = Script::default();
        let options = CopyOptions {
            follow_symlinks: true,
        };
        let target = d.p("copy").display().to_string();
        copy_items(&[d.p("top")], &target, &base(), &options, &mut ui);
        assert!(ui.errors.is_empty(), "{:?}", ui.errors);
        assert!(d.p("copy/one/file").exists() && d.p("copy/two/file").exists());
    }

    #[test]
    fn copy_onto_itself_never_truncates() {
        let d = TestDir::new("self");
        let src = d.write("a.txt", "keep me");
        let mut ui = Script::default();
        let target = src.display().to_string();
        let s = copy_items(
            std::slice::from_ref(&src),
            &target,
            &base(),
            &CopyOptions::default(),
            &mut ui,
        );
        assert_eq!(s.done, 0);
        assert_eq!(ui.errors.len(), 1);
        assert_eq!(fs::read_to_string(&src).unwrap(), "keep me");
    }

    #[test]
    fn copy_through_a_symlink_to_the_source_never_truncates() {
        let d = TestDir::new("alias");
        let src = d.write("a.txt", "keep me");
        symlink(&src, d.p("alias")).unwrap();
        let mut ui = Script::default();
        let target = d.p("alias").display().to_string();
        copy_items(
            std::slice::from_ref(&src),
            &target,
            &base(),
            &CopyOptions::default(),
            &mut ui,
        );
        assert_eq!(ui.prompts, 0);
        assert_eq!(fs::read_to_string(&src).unwrap(), "keep me");
    }

    #[test]
    fn folder_cannot_be_copied_or_moved_into_itself() {
        let d = TestDir::new("into");
        d.write("dir/f", "x");
        let mut ui = Script::default();
        let target = d.p("dir/inner/deeper").display().to_string();
        copy_items(
            &[d.p("dir")],
            &target,
            &base(),
            &CopyOptions::default(),
            &mut ui,
        );
        assert_eq!(ui.errors.len(), 1);
        assert!(!d.p("dir/inner").exists());
        let mut ui = Script::default();
        move_items(&[d.p("dir")], &target, &base(), &mut ui);
        assert_eq!(ui.errors.len(), 1);
        assert!(d.p("dir/f").exists());
    }

    #[test]
    fn overwrite_choices_are_respected() {
        let d = TestDir::new("over");
        let a = d.write("a/x", "new");
        let b = d.write("a/y", "new");
        d.write("b/x", "old");
        d.write("b/y", "old");
        let target = format!("{}/", d.p("b").display());
        let sources = [a, b];

        let mut ui = Script {
            overwrite: OverwriteChoice::Skip,
            ..Default::default()
        };
        let s = copy_items(&sources, &target, &base(), &CopyOptions::default(), &mut ui);
        assert_eq!((s.done, s.skipped, ui.prompts), (0, 2, 2));
        assert_eq!(fs::read_to_string(d.p("b/x")).unwrap(), "old");

        let mut ui = Script {
            overwrite: OverwriteChoice::SkipAll,
            ..Default::default()
        };
        let s = copy_items(&sources, &target, &base(), &CopyOptions::default(), &mut ui);
        assert_eq!((s.skipped, ui.prompts), (2, 1));

        let mut ui = Script {
            overwrite: OverwriteChoice::OverwriteAll,
            ..Default::default()
        };
        let s = copy_items(&sources, &target, &base(), &CopyOptions::default(), &mut ui);
        assert_eq!((s.done, ui.prompts), (2, 1));
        assert_eq!(fs::read_to_string(d.p("b/y")).unwrap(), "new");
    }

    #[test]
    fn cancelling_a_copy_leaves_the_old_file_and_no_temp_file() {
        let d = TestDir::new("cancel");
        let src = d.write("a/big", &"z".repeat(5 * COPY_BUFFER));
        d.write("b/big", "old");
        let mut ui = Script {
            cancel_after_progress: Some(2),
            ..Default::default()
        };
        let target = d.p("b/big").display().to_string();
        let s = copy_items(&[src], &target, &base(), &CopyOptions::default(), &mut ui);
        assert!(s.cancelled);
        assert_eq!(fs::read_to_string(d.p("b/big")).unwrap(), "old");
        assert_eq!(fs::read_dir(d.p("b")).unwrap().count(), 1);
    }

    #[test]
    fn retry_runs_the_failed_copy_again() {
        struct FixThenRetry(PathBuf, u32);
        impl OpUi for FixThenRetry {
            fn confirm_delete(&mut self, _: &Path, _: DeleteAsk) -> DeleteChoice {
                DeleteChoice::Delete
            }
            fn cancelled(&self) -> bool {
                false
            }
            fn progress(&mut self, _: &Progress) {}
            fn overwrite(&mut self, _: &Path, _: &Path) -> OverwriteChoice {
                OverwriteChoice::Cancel
            }
            fn error(&mut self, _: &Path, _: &str) -> ErrorChoice {
                self.1 += 1;
                fs::create_dir_all(&self.0).unwrap();
                ErrorChoice::Retry
            }
            fn trash_fallback(&mut self, _: &Path) -> TrashChoice {
                TrashChoice::Cancel
            }
        }
        let d = TestDir::new("retry");
        let src = d.write("a", "data");
        let mut ui = FixThenRetry(d.p("missing"), 0);
        let target = d.p("missing/a").display().to_string();
        let s = copy_items(&[src], &target, &base(), &CopyOptions::default(), &mut ui);
        assert_eq!((s.done, ui.1), (1, 1));
        assert_eq!(fs::read_to_string(d.p("missing/a")).unwrap(), "data");
    }

    #[test]
    fn rename_and_move_into_folder() {
        let d = TestDir::new("move");
        let a = d.write("a.txt", "1");
        let mut ui = Script::default();
        move_items(std::slice::from_ref(&a), "b.txt", &d.0, &mut ui);
        assert!(!a.exists());
        assert_eq!(fs::read_to_string(d.p("b.txt")).unwrap(), "1");
        fs::create_dir(d.p("dir")).unwrap();
        move_items(&[d.p("b.txt")], "dir", &d.0, &mut ui);
        assert_eq!(fs::read_to_string(d.p("dir/b.txt")).unwrap(), "1");
    }

    #[test]
    fn move_merges_folders_and_keeps_what_was_skipped() {
        let d = TestDir::new("merge");
        d.write("a/d/one", "new");
        d.write("a/d/two", "new");
        d.write("b/d/one", "old");
        let mut ui = Script {
            overwrite: OverwriteChoice::Skip,
            ..Default::default()
        };
        let target = d.p("b").display().to_string();
        move_items(&[d.p("a/d")], &target, &base(), &mut ui);
        assert_eq!(fs::read_to_string(d.p("b/d/one")).unwrap(), "old");
        assert_eq!(fs::read_to_string(d.p("b/d/two")).unwrap(), "new");
        assert_eq!(fs::read_to_string(d.p("a/d/one")).unwrap(), "new");
        assert!(!d.p("a/d/two").exists());
    }

    #[test]
    fn move_to_same_path_is_a_no_op() {
        let d = TestDir::new("same");
        let a = d.write("a.txt", "1");
        let mut ui = Script::default();
        let target = a.display().to_string();
        move_items(std::slice::from_ref(&a), &target, &base(), &mut ui);
        assert_eq!(fs::read_to_string(&a).unwrap(), "1");
        assert!(ui.errors.is_empty());
    }

    #[test]
    fn a_link_never_replaces_the_file_it_points_to() {
        let d = TestDir::new("linktarget");
        d.write("b/file", "precious");
        fs::create_dir(d.p("a")).unwrap();
        symlink(d.p("b/file"), d.p("a/file")).unwrap();
        for moving in [false, true] {
            let mut ui = Script::default();
            let sources = [d.p("a/file")];
            let target = d.p("b").display().to_string();
            let summary = if moving {
                move_items(&sources, &target, &d.0, &mut ui)
            } else {
                let options = CopyOptions::default();
                copy_items(&sources, &target, &d.0, &options, &mut ui)
            };
            assert_eq!(summary.done, 0);
            assert_eq!(fs::read_to_string(d.p("b/file")).unwrap(), "precious");
            let kept = fs::symlink_metadata(d.p("a/file")).unwrap();
            assert!(kept.file_type().is_symlink());
        }
    }

    #[test]
    fn a_link_to_another_file_can_still_replace_a_file() {
        let d = TestDir::new("linkother");
        d.write("other", "x");
        d.write("b/file", "old");
        fs::create_dir(d.p("a")).unwrap();
        symlink(d.p("other"), d.p("a/file")).unwrap();
        let mut ui = Script::default();
        let target = d.p("b").display().to_string();
        let options = CopyOptions::default();
        copy_items(&[d.p("a/file")], &target, &d.0, &options, &mut ui);
        assert!(fs::symlink_metadata(d.p("b/file"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn copies_do_not_keep_setuid() {
        let d = TestDir::new("setuid");
        let file = d.write("prog", "x");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o6755)).unwrap();
        let mut ui = Script::default();
        let options = CopyOptions::default();
        copy_items(&[file], "copy", &d.0, &options, &mut ui);
        let mode = fs::metadata(d.p("copy")).unwrap().mode() & 0o7777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn move_across_keeps_source_when_copy_was_skipped() {
        let d = TestDir::new("across");
        d.write("a/x", "new");
        d.write("b/x", "old");
        let meta = fs::symlink_metadata(d.p("a")).unwrap();
        let mut ui = Script {
            overwrite: OverwriteChoice::Skip,
            ..Default::default()
        };
        let mut ctx = Ctx::new(&mut ui, false);
        let _ = move_across(&mut ctx, &d.p("a"), &d.p("b"), &meta);
        assert_eq!(ctx.skipped, 1);
        assert_eq!(fs::read_to_string(d.p("a/x")).unwrap(), "new");
        assert_eq!(fs::read_to_string(d.p("b/x")).unwrap(), "old");
    }

    #[test]
    fn move_across_removes_source_after_success() {
        let d = TestDir::new("across2");
        d.write("a/x", "data");
        let meta = fs::symlink_metadata(d.p("a")).unwrap();
        let mut ui = Script::default();
        let mut ctx = Ctx::new(&mut ui, false);
        let _ = move_across(&mut ctx, &d.p("a"), &d.p("c"), &meta);
        assert!(!d.p("a").exists());
        assert_eq!(fs::read_to_string(d.p("c/x")).unwrap(), "data");
    }

    fn folder_with_file(d: &TestDir, name: &str) -> PathBuf {
        fs::create_dir(d.p(name)).unwrap();
        d.write(&format!("{name}/inner"), "x");
        d.p(name)
    }

    fn read_only_file(d: &TestDir, name: &str) -> PathBuf {
        let path = d.write(name, "x");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        path
    }

    #[test]
    fn non_empty_folder_asks_and_all_stops_asking() {
        let d = TestDir::new("del_folder_all");
        let items = [folder_with_file(&d, "a"), folder_with_file(&d, "b")];
        let mut ui = Script {
            delete: Some(DeleteChoice::All),
            ..Default::default()
        };
        let s = delete_items(&items, true, &mut ui);
        assert_eq!(ui.confirms.len(), 1);
        assert_eq!(
            ui.confirms[0].1,
            DeleteAsk::NonEmptyFolder { to_trash: false }
        );
        assert_eq!(s.skipped, 0);
        assert!(!items[0].exists() && !items[1].exists());
    }

    #[test]
    fn empty_folder_and_file_are_not_asked_about() {
        let d = TestDir::new("del_no_ask");
        fs::create_dir(d.p("empty")).unwrap();
        let items = [d.p("empty"), d.write("file", "x")];
        let mut ui = Script::default();
        delete_items(&items, true, &mut ui);
        assert!(ui.confirms.is_empty());
        assert!(!items[0].exists() && !items[1].exists());
    }

    #[test]
    fn link_to_a_full_folder_is_not_asked_about_and_target_survives() {
        let d = TestDir::new("del_link_ask");
        let target = folder_with_file(&d, "target");
        std::os::unix::fs::symlink(&target, d.p("link")).unwrap();
        let mut ui = Script::default();
        delete_items(&[d.p("link")], true, &mut ui);
        assert!(ui.confirms.is_empty());
        assert!(fs::symlink_metadata(d.p("link")).is_err());
        assert!(target.join("inner").exists());
    }

    #[test]
    fn skipped_folder_is_kept_and_the_rest_still_go() {
        let d = TestDir::new("del_folder_skip");
        let folder = folder_with_file(&d, "keep");
        let file = d.write("gone", "x");
        let mut ui = Script {
            delete: Some(DeleteChoice::Skip),
            ..Default::default()
        };
        let s = delete_items(&[folder.clone(), file.clone()], true, &mut ui);
        assert_eq!(s.skipped, 1);
        assert!(folder.join("inner").exists());
        assert!(!file.exists());
    }

    #[test]
    fn skip_all_stops_asking_and_keeps_every_folder() {
        let d = TestDir::new("del_folder_skipall");
        let items = [folder_with_file(&d, "a"), folder_with_file(&d, "b")];
        let mut ui = Script {
            delete: Some(DeleteChoice::SkipAll),
            ..Default::default()
        };
        let s = delete_items(&items, true, &mut ui);
        assert_eq!((ui.confirms.len(), s.skipped), (1, 2));
        assert!(items[0].exists() && items[1].exists());
    }

    #[test]
    fn cancel_at_the_folder_prompt_deletes_nothing_more() {
        let d = TestDir::new("del_folder_cancel");
        let items = [folder_with_file(&d, "a"), d.write("later", "x")];
        let mut ui = Script {
            delete: Some(DeleteChoice::Cancel),
            ..Default::default()
        };
        let s = delete_items(&items, true, &mut ui);
        assert!(s.cancelled);
        assert!(items[0].join("inner").exists() && items[1].exists());
    }

    #[test]
    fn trash_of_non_empty_folder_asks_too() {
        let d = TestDir::new("del_folder_trash");
        let folder = folder_with_file(&d, "a");
        let mut ui = Script {
            delete: Some(DeleteChoice::Skip),
            ..Default::default()
        };
        delete_items(std::slice::from_ref(&folder), false, &mut ui);
        assert_eq!(
            ui.confirms[0].1,
            DeleteAsk::NonEmptyFolder { to_trash: true }
        );
        assert!(folder.join("inner").exists());
    }

    #[test]
    fn read_only_file_asks_and_skip_keeps_its_folder_without_an_error() {
        let d = TestDir::new("del_ro_skip");
        let folder = folder_with_file(&d, "dir");
        let locked = read_only_file(&d, "dir/locked");
        // The folder prompt takes the first answer, the read-only one the next.
        let mut ui = Script {
            delete_after: Some((1, DeleteChoice::Skip)),
            ..Default::default()
        };
        let s = delete_items(std::slice::from_ref(&folder), true, &mut ui);
        assert_eq!(ui.confirms.len(), 2);
        assert_eq!(ui.confirms[1], (locked.clone(), DeleteAsk::ReadOnly));
        assert!(ui.errors.is_empty(), "{:?}", ui.errors);
        assert_eq!(s.skipped, 1);
        assert!(locked.exists());
        assert!(!folder.join("inner").exists());
    }

    #[test]
    fn read_only_all_answers_only_read_only_files() {
        let d = TestDir::new("del_ro_all");
        let items = [read_only_file(&d, "a"), read_only_file(&d, "b")];
        let mut ui = Script {
            delete: Some(DeleteChoice::All),
            ..Default::default()
        };
        delete_items(&items, true, &mut ui);
        assert_eq!(ui.confirms.len(), 1);
        assert!(!items[0].exists() && !items[1].exists());
    }

    #[test]
    fn link_to_read_only_file_is_not_asked_about() {
        let d = TestDir::new("del_ro_link");
        let target = read_only_file(&d, "target");
        std::os::unix::fs::symlink(&target, d.p("link")).unwrap();
        let mut ui = Script::default();
        delete_items(&[d.p("link")], true, &mut ui);
        assert!(ui.confirms.is_empty());
        assert!(target.exists());
    }

    #[test]
    fn permanent_delete_removes_links_not_their_targets() {
        let d = TestDir::new("del");
        d.write("keep/file", "safe");
        d.write("victim/f", "x");
        symlink(d.p("keep"), d.p("victim/link")).unwrap();
        let mut ui = Script::default();
        let s = delete_items(&[d.p("victim")], true, &mut ui);
        assert!(!s.cancelled);
        assert!(!d.p("victim").exists());
        assert_eq!(fs::read_to_string(d.p("keep/file")).unwrap(), "safe");
    }

    #[test]
    fn trash_keeps_file_and_records_where_it_came_from() {
        let d = TestDir::new("trash");
        let f = d.write("my file.txt", "x");
        let g = d.write("sub/my file.txt", "y");
        let trash = d.p("Trash");
        move_to_trash(&f, &trash).unwrap();
        move_to_trash(&g, &trash).unwrap();
        assert!(!f.exists());
        assert_eq!(
            fs::read_to_string(trash.join("files/my file.txt")).unwrap(),
            "x"
        );
        assert_eq!(
            fs::read_to_string(trash.join("files/my file.txt.1")).unwrap(),
            "y"
        );
        let info = fs::read_to_string(trash.join("info/my file.txt.trashinfo")).unwrap();
        assert!(info.contains("my%20file.txt"), "{info}");
        assert!(info.contains("DeletionDate="));
    }

    #[test]
    fn attributes_change_mode_recursively_and_skip_symlinks() {
        let d = TestDir::new("attr");
        let f = d.write("dir/f", "x");
        let outside = d.write("outside", "y");
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&outside, d.p("dir/link")).unwrap();
        let change = AttrChange {
            mode: Some(0o700),
            recursive: true,
            ..Default::default()
        };
        let mut ui = Script::default();
        set_attributes(&[d.p("dir")], &change, &mut ui);
        assert_eq!(fs::metadata(&f).unwrap().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&outside).unwrap().mode() & 0o777, 0o644);
    }

    #[test]
    fn mode_parsing() {
        assert_eq!(parse_mode("644"), Some(0o644));
        assert_eq!(parse_mode("0755"), Some(0o755));
        assert_eq!(parse_mode("9"), None);
        assert_eq!(parse_mode(""), None);
    }

    #[test]
    fn dest_resolution() {
        let d = TestDir::new("dest");
        fs::create_dir(d.p("dir")).unwrap();
        let src = Path::new("/x/a.txt");
        assert_eq!(resolve_dest(src, "dir", &d.0, false), d.p("dir/a.txt"));
        assert_eq!(resolve_dest(src, "new", &d.0, false), d.p("new"));
        assert_eq!(resolve_dest(src, "new/", &d.0, false), d.p("new/a.txt"));
        assert_eq!(resolve_dest(src, "new", &d.0, true), d.p("new/a.txt"));
    }

    #[test]
    fn find_by_name_and_content() {
        let d = TestDir::new("find");
        d.write("a/one.txt", "needle here");
        d.write("a/two.txt", "nothing");
        d.write("b/three.log", "NEEDLE");
        let stop = || false;
        let mut hits = Vec::new();
        find_files(
            &d.0,
            "*.txt",
            None,
            false,
            &stop,
            &mut |p| hits.push(p),
            &mut |_| {},
        );
        assert_eq!(hits.len(), 2);
        hits.clear();
        find_files(
            &d.0,
            "*",
            Some(b"needle"),
            true,
            &stop,
            &mut |p| hits.push(p),
            &mut |_| {},
        );
        assert_eq!(hits.len(), 2);
        hits.clear();
        find_files(
            &d.0,
            "*",
            Some(b"needle"),
            false,
            &stop,
            &mut |p| hits.push(p),
            &mut |_| {},
        );
        assert_eq!(hits, vec![d.p("a/one.txt")]);
    }

    #[test]
    fn content_search_finds_matches_across_chunk_boundaries() {
        let d = TestDir::new("chunk");
        let mut text = "x".repeat(COPY_BUFFER - 2);
        text.push_str("needle");
        let f = d.write("big", &text);
        assert!(file_contains(&f, b"needle", false, &|| false));
        assert!(!file_contains(&f, b"absent", false, &|| false));
    }
}
