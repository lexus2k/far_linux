//! Archives: listing, extracting, creating and changing them. Zip and tar
//! formats are handled in-process. Bzip2 and xz use the system tools for the
//! compression layer, and 7z uses the `7z` program.
//!
//! Archive contents are untrusted. Entry names can never place a file outside
//! the destination, and nothing is written through a symbolic link. Changing an
//! archive writes a new file and renames it into place, so that a failure or a
//! cancel leaves the original untouched.

use crate::fsops::{self, Cancelled, Ctx, OpSummary, OpUi, Totals};
use crate::util;
use flate2::{read::MultiGzDecoder, write::GzEncoder, Compression};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipArchive, ZipWriter};

const BUFFER: usize = 256 * 1024;
const S_IFMT: u32 = 0o170000;
const S_IFLNK: u32 = 0o120000;

/// A supported archive format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Zip,
    Tar,
    TarGz,
    TarBz2,
    TarXz,
    SevenZ,
}

/// All formats, in the order offered when creating an archive.
pub const FORMATS: [Format; 6] = [
    Format::Zip,
    Format::TarGz,
    Format::Tar,
    Format::TarXz,
    Format::TarBz2,
    Format::SevenZ,
];

impl Format {
    /// The usual file name extension.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Zip => "zip",
            Format::Tar => "tar",
            Format::TarGz => "tar.gz",
            Format::TarBz2 => "tar.bz2",
            Format::TarXz => "tar.xz",
            Format::SevenZ => "7z",
        }
    }

    /// The program this format needs, if any.
    pub fn tool(self) -> Option<&'static str> {
        match self {
            Format::TarBz2 => Some("bzip2"),
            Format::TarXz => Some("xz"),
            Format::SevenZ => Some("7z"),
            _ => None,
        }
    }

    /// True when the program this format needs is installed.
    pub fn is_available(self) -> bool {
        self.tool().is_none_or(program_exists)
    }

    fn compressor(self) -> Option<&'static str> {
        match self {
            Format::TarBz2 => Some("bzip2"),
            Format::TarXz => Some("xz"),
            _ => None,
        }
    }
}

fn program_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
}

/// Compression level, as ArcLite's Create archive dialog offers it. Normal
/// keeps each format's own default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Level {
    Store,
    Fastest,
    #[default]
    Normal,
    Maximum,
    Ultra,
}

pub const LEVELS: [Level; 5] = [
    Level::Store,
    Level::Fastest,
    Level::Normal,
    Level::Maximum,
    Level::Ultra,
];

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Level::Store => "Store",
            Level::Fastest => "Fastest",
            Level::Normal => "Normal",
            Level::Maximum => "Maximum",
            Level::Ultra => "Ultra",
        }
    }

    /// The option for an external compressor, bzip2 or xz. Neither can
    /// store, so Store is their fastest level.
    fn tool_arg(self, tool: &str) -> Option<&'static str> {
        match (self, tool) {
            (Level::Normal, _) => None,
            (Level::Store, "xz") => Some("-0"),
            (Level::Store | Level::Fastest, _) => Some("-1"),
            (Level::Ultra, "xz") => Some("-9e"),
            (Level::Maximum | Level::Ultra, _) => Some("-9"),
        }
    }

    /// The deflate level for zip and gzip; `None` is the default.
    fn deflate(self) -> Option<u32> {
        match self {
            Level::Store => Some(0),
            Level::Fastest => Some(1),
            Level::Normal => None,
            Level::Maximum | Level::Ultra => Some(9),
        }
    }

    /// 7z's `-mx` switch.
    fn seven_z(self) -> Option<&'static str> {
        match self {
            Level::Store => Some("-mx0"),
            Level::Fastest => Some("-mx1"),
            Level::Normal => None,
            Level::Maximum => Some("-mx7"),
            Level::Ultra => Some("-mx9"),
        }
    }
}

/// Recognizes an archive by its file name.
pub fn detect(path: &Path) -> Option<Format> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    let table: [(&str, Format); 12] = [
        (".tar.gz", Format::TarGz),
        (".tgz", Format::TarGz),
        (".tar.bz2", Format::TarBz2),
        (".tbz2", Format::TarBz2),
        (".tbz", Format::TarBz2),
        (".tar.xz", Format::TarXz),
        (".txz", Format::TarXz),
        (".tar", Format::Tar),
        (".zip", Format::Zip),
        (".jar", Format::Zip),
        (".apk", Format::Zip),
        (".7z", Format::SevenZ),
    ];
    table
        .iter()
        .find(|(suffix, _)| name.ends_with(suffix) && name.len() > suffix.len())
        .map(|(_, format)| *format)
}

/// One item stored in an archive.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArcEntry {
    pub path: String,
    pub is_dir: bool,
    pub is_link: bool,
    pub size: u64,
    pub mtime: i64,
    pub mode: u32,
}

/// Cleans an entry name, refusing any that could leave the destination.
pub fn safe_name(name: &str) -> Option<String> {
    if name.contains('\0') {
        return None;
    }
    let mut parts = Vec::new();
    for part in Path::new(name).components() {
        match part {
            Component::Normal(text) => parts.push(text.to_str()?),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// True when `path` is `item` or lies inside it.
fn is_under(path: &str, item: &str) -> bool {
    path == item
        || path
            .strip_prefix(item)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The direct children of a folder in an archive, including folders that
/// exist only as part of a deeper entry's name.
pub fn children(all: &[ArcEntry], folder: &str) -> Vec<ArcEntry> {
    let mut found: BTreeMap<String, ArcEntry> = BTreeMap::new();
    for entry in all {
        let rest = if folder.is_empty() {
            entry.path.as_str()
        } else {
            match entry
                .path
                .strip_prefix(folder)
                .and_then(|r| r.strip_prefix('/'))
            {
                Some(rest) => rest,
                None => continue,
            }
        };
        match rest.split_once('/') {
            None => {
                found.insert(rest.to_string(), entry.clone());
            }
            Some((dir, _)) => {
                let path = if folder.is_empty() {
                    dir.to_string()
                } else {
                    format!("{folder}/{dir}")
                };
                found.entry(dir.to_string()).or_insert(ArcEntry {
                    path,
                    is_dir: true,
                    mode: 0o755,
                    mtime: entry.mtime,
                    ..Default::default()
                });
            }
        }
    }
    found.into_values().collect()
}

fn invalid(message: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

/// Converts a zip timestamp, which has no time zone, to a Unix time.
fn zip_time(time: Option<zip::DateTime>) -> i64 {
    let Some(t) = time else {
        return 0;
    };
    // SAFETY: `tm` is plain data and mktime only reads and normalizes it.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        tm.tm_year = t.year() as i32 - 1900;
        tm.tm_mon = t.month() as i32 - 1;
        tm.tm_mday = t.day() as i32;
        tm.tm_hour = t.hour() as i32;
        tm.tm_min = t.minute() as i32;
        tm.tm_sec = t.second() as i32;
        tm.tm_isdst = -1;
        libc::mktime(&mut tm) as i64
    }
}

fn zip_stamp(secs: i64) -> zip::DateTime {
    let t = util::local_time(secs);
    zip::DateTime::from_date_and_time(
        t.year.clamp(1980, 2107) as u16,
        t.month as u8,
        t.day as u8,
        t.hour as u8,
        t.minute as u8,
        t.second.min(58) as u8,
    )
    .unwrap_or_default()
}

/// A tar stream being read, with any decompression under it.
enum TarIn {
    Plain(File),
    Gz(Box<MultiGzDecoder<File>>),
    Pipe(Child, ChildStdout),
}

impl Read for TarIn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            TarIn::Plain(f) => f.read(buf),
            TarIn::Gz(f) => f.read(buf),
            TarIn::Pipe(_, out) => out.read(buf),
        }
    }
}

impl TarIn {
    fn open(path: &Path, format: Format) -> io::Result<Self> {
        let file = File::open(path)?;
        match format.compressor() {
            Some(tool) => {
                let mut child = Command::new(tool)
                    .arg("-dc")
                    .stdin(file)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()?;
                let out = child.stdout.take().ok_or_else(|| invalid("no output"))?;
                Ok(TarIn::Pipe(child, out))
            }
            None if format == Format::TarGz => Ok(TarIn::Gz(Box::new(MultiGzDecoder::new(file)))),
            None => Ok(TarIn::Plain(file)),
        }
    }

    /// Reads to the end and reports whether the data was complete.
    fn finish(&mut self) -> io::Result<()> {
        io::copy(self, &mut io::sink())?;
        if let TarIn::Pipe(child, _) = self {
            let status = child.wait()?;
            if !status.success() {
                return Err(invalid("the compressed data is damaged"));
            }
        }
        Ok(())
    }
}

impl Drop for TarIn {
    fn drop(&mut self) {
        if let TarIn::Pipe(child, _) = self {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A tar stream being written, with any compression under it.
enum TarOut {
    Plain(File),
    Gz(GzEncoder<File>),
    Pipe(Child, Option<ChildStdin>),
}

impl Write for TarOut {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            TarOut::Plain(f) => f.write(buf),
            TarOut::Gz(f) => f.write(buf),
            TarOut::Pipe(_, stdin) => stdin
                .as_mut()
                .ok_or_else(|| invalid("the stream is closed"))?
                .write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            TarOut::Plain(f) => f.flush(),
            TarOut::Gz(f) => f.flush(),
            TarOut::Pipe(_, stdin) => stdin.as_mut().map_or(Ok(()), Write::flush),
        }
    }
}

impl TarOut {
    fn create(file: File, format: Format, level: Level) -> io::Result<Self> {
        match format.compressor() {
            Some(tool) => {
                let mut child = Command::new(tool)
                    .arg("-c")
                    .args(level.tool_arg(tool))
                    .stdin(Stdio::piped())
                    .stdout(file)
                    .stderr(Stdio::null())
                    .spawn()?;
                let stdin = child.stdin.take();
                Ok(TarOut::Pipe(child, stdin))
            }
            None if format == Format::TarGz => {
                let compression = level
                    .deflate()
                    .map_or(Compression::default(), Compression::new);
                Ok(TarOut::Gz(GzEncoder::new(file, compression)))
            }
            None => Ok(TarOut::Plain(file)),
        }
    }

    /// Completes the stream. A failure here means the archive is not usable.
    fn finish(self) -> io::Result<()> {
        match self {
            TarOut::Plain(f) => f.sync_all(),
            TarOut::Gz(encoder) => encoder.finish()?.sync_all(),
            TarOut::Pipe(mut child, stdin) => {
                drop(stdin);
                let status = child.wait()?;
                if status.success() {
                    Ok(())
                } else {
                    Err(invalid("the compressor failed"))
                }
            }
        }
    }
}

fn tar_entry_name<R: Read>(entry: &tar::Entry<R>) -> Option<String> {
    let bytes = entry.path_bytes();
    safe_name(std::str::from_utf8(&bytes).ok()?)
}

fn list_tar(path: &Path, format: Format) -> io::Result<Vec<ArcEntry>> {
    let mut archive = tar::Archive::new(TarIn::open(path, format)?);
    let mut out = Vec::new();
    for entry in archive.entries()? {
        let entry = entry?;
        let Some(name) = tar_entry_name(&entry) else {
            continue;
        };
        let header = entry.header();
        let kind = header.entry_type();
        out.push(ArcEntry {
            path: name,
            is_dir: kind.is_dir(),
            is_link: kind.is_symlink(),
            size: entry.size(),
            mtime: header.mtime().unwrap_or(0) as i64,
            mode: header.mode().unwrap_or(0o644) & 0o777,
        });
    }
    drop(archive);
    Ok(out)
}

fn zip_is_link(file: &zip::read::ZipFile<'_, File>) -> bool {
    file.unix_mode().is_some_and(|m| m & S_IFMT == S_IFLNK)
}

fn zip_entry_name(file: &zip::read::ZipFile<'_, File>) -> Option<String> {
    safe_name(&file.name().ok()?.replace('\\', "/"))
}

fn list_zip(path: &Path) -> io::Result<Vec<ArcEntry>> {
    let mut archive = ZipArchive::new(File::open(path)?).map_err(invalid)?;
    let mut out = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let file = archive.by_index_raw(i).map_err(invalid)?;
        let Some(name) = zip_entry_name(&file) else {
            continue;
        };
        out.push(ArcEntry {
            path: name,
            is_dir: file.is_dir(),
            is_link: zip_is_link(&file),
            size: file.size(),
            mtime: zip_time(file.last_modified()),
            mode: file
                .unix_mode()
                .unwrap_or(if file.is_dir() { 0o755 } else { 0o644 })
                & 0o777,
        });
    }
    Ok(out)
}

fn run_7z(args: &[&std::ffi::OsStr], cwd: Option<&Path>) -> io::Result<String> {
    let mut command = Command::new("7z");
    command.args(args).stdin(Stdio::null());
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let output = command.output()?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        return Ok(text);
    }
    let detail = text
        .lines()
        .chain(String::from_utf8_lossy(&output.stderr).lines())
        .find(|l| l.contains("ERROR") || l.contains("Error"))
        .unwrap_or("7z failed")
        .to_string();
    Err(invalid(detail))
}

fn parse_7z_time(text: &str) -> i64 {
    let digits: Vec<i32> = text
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|p| p.parse().ok())
        .collect();
    let [year, month, day, hour, minute, second] = digits[..] else {
        return 0;
    };
    // SAFETY: as for zip_time.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        tm.tm_year = year - 1900;
        tm.tm_mon = month - 1;
        tm.tm_mday = day;
        tm.tm_hour = hour;
        tm.tm_min = minute;
        tm.tm_sec = second;
        tm.tm_isdst = -1;
        libc::mktime(&mut tm) as i64
    }
}

fn os(text: &str) -> &std::ffi::OsStr {
    std::ffi::OsStr::new(text)
}

fn list_7z(path: &Path) -> io::Result<Vec<ArcEntry>> {
    let text = run_7z(
        &[os("l"), os("-slt"), os("-spd"), os("--"), path.as_os_str()],
        None,
    )?;
    let mut out = Vec::new();
    // Technical output is `Key = Value` blocks. Entries follow the `----` line.
    let body = text.split_once("----------").map_or("", |(_, rest)| rest);
    for block in body.split("\n\n") {
        let mut entry = ArcEntry {
            mode: 0o644,
            ..Default::default()
        };
        let mut name = None;
        for line in block.lines() {
            let Some((key, value)) = line.split_once(" = ") else {
                continue;
            };
            match key {
                "Path" => name = safe_name(value),
                "Size" => entry.size = value.parse().unwrap_or(0),
                "Modified" => entry.mtime = parse_7z_time(value),
                "Folder" => entry.is_dir = value == "+",
                "Attributes" => {
                    entry.is_dir |= value.starts_with('D');
                    entry.is_link |= value.contains(" lrwx");
                }
                _ => {}
            }
        }
        if let Some(path) = name {
            entry.path = path;
            out.push(entry);
        }
    }
    Ok(out)
}

/// Lists an archive's contents. Entries with unsafe names are left out.
pub fn list(path: &Path, format: Format) -> io::Result<Vec<ArcEntry>> {
    match format {
        Format::Zip => list_zip(path),
        Format::SevenZ => list_7z(path),
        _ => list_tar(path, format),
    }
}

/// What to extract and where.
#[derive(Clone, Debug, Default)]
pub struct ExtractRequest {
    /// Archive paths to extract. Empty means everything.
    pub items: Vec<String>,
    /// The archive folder that maps to the destination.
    pub base: String,
    pub dest: PathBuf,
}

impl ExtractRequest {
    fn wanted(&self, path: &str) -> bool {
        self.items.is_empty() || self.items.iter().any(|item| is_under(path, item))
    }

    /// The destination of an entry, if it lies under `base`.
    fn target(&self, path: &str) -> Option<PathBuf> {
        if self.base.is_empty() {
            return Some(self.dest.join(path));
        }
        let rest = path.strip_prefix(&self.base)?.strip_prefix('/')?;
        Some(self.dest.join(rest))
    }
}

/// Creates the folders above `path`, refusing to pass through a symbolic link.
/// This is what stops an archive from writing outside the destination using a
/// link it created earlier, or one that was already there.
fn prepare_parents(dest: &Path, path: &Path) -> io::Result<()> {
    fs::create_dir_all(dest)?;
    let parent = path.parent().unwrap_or(dest);
    let inner = parent
        .strip_prefix(dest)
        .map_err(|_| invalid("unsafe path"))?;
    let mut current = dest.to_path_buf();
    for part in inner.components() {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(invalid("the path passes through a symbolic link"));
            }
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(invalid("a file is in the way of a folder")),
            Err(_) => fs::create_dir(&current)?,
        }
    }
    Ok(())
}

fn temp_beside(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    path.with_file_name(format!(
        ".far_arc_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn set_mtime(path: &Path, mtime: i64) {
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    let times = [
        libc::timespec {
            tv_sec: mtime as libc::time_t,
            tv_nsec: 0,
        },
        libc::timespec {
            tv_sec: mtime as libc::time_t,
            tv_nsec: 0,
        },
    ];
    // SAFETY: `c` is NUL terminated and `times` has the two required entries.
    unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        );
    }
}

/// Tracks deferred work during extraction.
#[derive(Default)]
struct Pending {
    dirs: Vec<(PathBuf, u32, i64)>,
    links: Vec<(PathBuf, String)>,
    hard_links: Vec<(PathBuf, String)>,
}

fn stream_to_file<U: OpUi, R: Read>(
    ctx: &mut Ctx<U>,
    reader: &mut R,
    temp: &Path,
    mode: u32,
) -> Result<io::Result<()>, Cancelled> {
    let mut output = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode | 0o600)
        .open(temp)
    {
        Ok(file) => file,
        Err(e) => return Ok(Err(e)),
    };
    let mut buf = vec![0u8; BUFFER];
    loop {
        ctx.check()?;
        let n = match reader.read(&mut buf) {
            Ok(0) => return Ok(Ok(())),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Ok(Err(e)),
        };
        if let Err(e) = output.write_all(&buf[..n]) {
            return Ok(Err(e));
        }
        ctx.progress.bytes_done += n as u64;
        ctx.report();
    }
}

/// Extracts one file. The data can only be read once, so a failed read cannot
/// be retried from here: it is reported and the file is skipped.
fn write_file<U: OpUi, R: Read>(
    ctx: &mut Ctx<U>,
    reader: &mut R,
    dest: &Path,
    target: &Path,
    entry: &ArcEntry,
) -> Result<(), Cancelled> {
    ctx.progress.to = target.to_path_buf();
    ctx.report();
    if let Err(e) = prepare_parents(dest, target) {
        fail(ctx, target, &e)?;
        return Ok(());
    }
    if let Ok(existing) = fs::symlink_metadata(target) {
        if existing.is_dir() {
            fail(
                ctx,
                target,
                &invalid("a folder with this name already exists"),
            )?;
            return Ok(());
        }
        let source = Path::new(&entry.path);
        if !ctx.may_overwrite(source, target)? {
            return Ok(());
        }
    }
    let temp = temp_beside(target);
    let written = stream_to_file(ctx, reader, &temp, entry.mode & 0o777);
    let result = match written {
        Ok(result) => result,
        Err(cancelled) => {
            let _ = fs::remove_file(&temp);
            return Err(cancelled);
        }
    };
    let result = result.and_then(|()| {
        // Only permission bits: an archive must not create setuid programs.
        fs::set_permissions(&temp, fs::Permissions::from_mode(entry.mode & 0o777))?;
        set_mtime(&temp, entry.mtime);
        fs::rename(&temp, target)
    });
    match result {
        Ok(()) => ctx.done += 1,
        Err(e) => {
            let _ = fs::remove_file(&temp);
            fail(ctx, target, &e)?;
        }
    }
    ctx.progress.items_done += 1;
    Ok(())
}

/// Reports an error for a step that cannot be retried.
fn fail<U: OpUi>(ctx: &mut Ctx<U>, path: &Path, error: &io::Error) -> Result<(), Cancelled> {
    let message = error.to_string();
    let reported = ctx.attempt::<()>(path, || Err(io::Error::other(message.clone())));
    reported.map(|_| ())
}

fn finish_extract<U: OpUi>(
    ctx: &mut Ctx<U>,
    dest: &Path,
    pending: Pending,
) -> Result<(), Cancelled> {
    for (target, link) in pending.hard_links {
        let result = prepare_parents(dest, &target).and_then(|()| {
            let name = safe_name(&link).ok_or_else(|| invalid("unsafe link"))?;
            let source = dest.join(name);
            // Linking to a symbolic link would hand out a path outside `dest`.
            if fs::symlink_metadata(&source)?.file_type().is_symlink() {
                return Err(invalid("the link points to a symbolic link"));
            }
            let _ = fs::remove_file(&target);
            fs::hard_link(source, &target)
        });
        match result {
            Ok(()) => ctx.done += 1,
            Err(e) => fail(ctx, &target, &e)?,
        }
    }
    // Links come after every file, so that no file is written through one.
    for (target, link) in pending.links {
        if let Err(e) = prepare_parents(dest, &target) {
            fail(ctx, &target, &e)?;
            continue;
        }
        if let Ok(existing) = fs::symlink_metadata(&target) {
            if existing.is_dir() {
                fail(
                    ctx,
                    &target,
                    &invalid("a folder with this name already exists"),
                )?;
                continue;
            }
            if !ctx.may_overwrite(Path::new(&link), &target)? {
                continue;
            }
            let _ = fs::remove_file(&target);
        }
        match std::os::unix::fs::symlink(&link, &target) {
            Ok(()) => ctx.done += 1,
            Err(e) => fail(ctx, &target, &e)?,
        }
    }
    // Deepest first, so that read-only folders do not block anything.
    let mut dirs = pending.dirs;
    dirs.sort_by_key(|(path, _, _)| std::cmp::Reverse(path.components().count()));
    for (path, mode, mtime) in dirs {
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(mode & 0o777));
        set_mtime(&path, mtime);
    }
    Ok(())
}

fn make_dir<U: OpUi>(
    ctx: &mut Ctx<U>,
    dest: &Path,
    target: PathBuf,
    entry: &ArcEntry,
    pending: &mut Pending,
) -> Result<(), Cancelled> {
    let inner = target.join("x");
    match prepare_parents(dest, &inner) {
        Ok(()) => {
            pending.dirs.push((target, entry.mode | 0o700, entry.mtime));
            ctx.done += 1;
        }
        Err(e) => fail(ctx, &target, &e)?,
    }
    Ok(())
}

fn extract_zip<U: OpUi>(
    ctx: &mut Ctx<U>,
    path: &Path,
    request: &ExtractRequest,
) -> Result<io::Result<()>, Cancelled> {
    let mut archive = match File::open(path).and_then(|f| ZipArchive::new(f).map_err(invalid)) {
        Ok(archive) => archive,
        Err(e) => return Ok(Err(e)),
    };
    let mut pending = Pending::default();
    for i in 0..archive.len() {
        ctx.check()?;
        let mut file = match archive.by_index(i) {
            Ok(file) => file,
            Err(e) => {
                fail(ctx, path, &invalid(e))?;
                continue;
            }
        };
        let raw_name = file
            .name()
            .map_or_else(|_| "?".to_string(), |n| n.into_owned());
        let Some(name) = zip_entry_name(&file) else {
            fail(
                ctx,
                Path::new(&raw_name),
                &invalid("unsafe name in the archive"),
            )?;
            continue;
        };
        if !request.wanted(&name) {
            continue;
        }
        let Some(target) = request.target(&name) else {
            continue;
        };
        ctx.progress.from = PathBuf::from(&name);
        let entry = ArcEntry {
            path: name,
            is_dir: file.is_dir(),
            size: file.size(),
            mtime: zip_time(file.last_modified()),
            mode: file
                .unix_mode()
                .unwrap_or(if file.is_dir() { 0o755 } else { 0o644 }),
            ..Default::default()
        };
        if entry.is_dir {
            make_dir(ctx, &request.dest, target, &entry, &mut pending)?;
        } else if zip_is_link(&file) {
            let mut link = String::new();
            match file.read_to_string(&mut link) {
                Ok(_) => pending.links.push((target, link)),
                Err(e) => fail(ctx, &target, &e)?,
            }
        } else {
            write_file(ctx, &mut file, &request.dest, &target, &entry)?;
        }
    }
    finish_extract(ctx, &request.dest, pending)?;
    Ok(Ok(()))
}

fn extract_tar<U: OpUi>(
    ctx: &mut Ctx<U>,
    path: &Path,
    format: Format,
    request: &ExtractRequest,
) -> Result<io::Result<()>, Cancelled> {
    let input = match TarIn::open(path, format) {
        Ok(input) => input,
        Err(e) => return Ok(Err(e)),
    };
    let mut archive = tar::Archive::new(input);
    let entries = match archive.entries() {
        Ok(entries) => entries,
        Err(e) => return Ok(Err(e)),
    };
    let mut pending = Pending::default();
    for entry in entries {
        ctx.check()?;
        let mut file = match entry {
            Ok(file) => file,
            Err(e) => return Ok(Err(e)),
        };
        let Some(name) = tar_entry_name(&file) else {
            let raw = String::from_utf8_lossy(&file.path_bytes()).into_owned();
            fail(ctx, Path::new(&raw), &invalid("unsafe name in the archive"))?;
            continue;
        };
        if !request.wanted(&name) {
            continue;
        }
        let Some(target) = request.target(&name) else {
            continue;
        };
        ctx.progress.from = PathBuf::from(&name);
        let kind = file.header().entry_type();
        let info = ArcEntry {
            path: name,
            is_dir: kind.is_dir(),
            size: file.size(),
            mtime: file.header().mtime().unwrap_or(0) as i64,
            mode: file.header().mode().unwrap_or(0o644) & 0o777,
            ..Default::default()
        };
        let link = file
            .link_name_bytes()
            .map(|b| String::from_utf8_lossy(&b).into_owned());
        if kind.is_dir() {
            make_dir(ctx, &request.dest, target, &info, &mut pending)?;
        } else if kind.is_symlink() {
            if let Some(link) = link {
                pending.links.push((target, link));
            }
        } else if kind.is_hard_link() {
            if let Some(link) = link {
                pending.hard_links.push((target, link));
            }
        } else if kind.is_file() || kind.is_contiguous() || kind.is_gnu_sparse() {
            write_file(ctx, &mut file, &request.dest, &target, &info)?;
        } else {
            fail(ctx, &target, &invalid("special files are not extracted"))?;
        }
    }
    finish_extract(ctx, &request.dest, pending)?;
    Ok(Ok(()))
}

fn extract_7z<U: OpUi>(
    ctx: &mut Ctx<U>,
    path: &Path,
    request: &ExtractRequest,
) -> Result<io::Result<()>, Cancelled> {
    if let Err(e) = fs::create_dir_all(&request.dest) {
        return Ok(Err(e));
    }
    let temp = temp_beside(&request.dest.join("x"));
    if let Err(e) = fs::create_dir(&temp) {
        return Ok(Err(e));
    }
    let result = (|| -> Result<io::Result<()>, Cancelled> {
        let out = format!("-o{}", temp.display());
        let mut args: Vec<&std::ffi::OsStr> = vec![
            os("x"),
            os("-y"),
            os("-spd"),
            os(&out),
            os("--"),
            path.as_os_str(),
        ];
        args.extend(request.items.iter().map(|item| os(item)));
        if let Err(e) = run_7z(&args, None) {
            return Ok(Err(e));
        }
        // Moving with the shared code gives the usual overwrite questions.
        let from = temp.join(&request.base);
        let names = match fs::read_dir(&from) {
            Ok(names) => names,
            Err(e) => return Ok(Err(e)),
        };
        for name in names.flatten() {
            fsops::move_one(ctx, &name.path(), &request.dest.join(name.file_name()))?;
        }
        Ok(Ok(()))
    })();
    let _ = fs::remove_dir_all(&temp);
    result
}

/// Extracts from an archive, asking about existing files through `ui`.
pub fn extract<U: OpUi>(
    path: &Path,
    format: Format,
    request: &ExtractRequest,
    ui: &mut U,
) -> OpSummary {
    let mut ctx = Ctx::new(ui, false);
    if let Ok(all) = list(path, format) {
        let wanted = all.iter().filter(|e| request.wanted(&e.path));
        for entry in wanted {
            ctx.progress.bytes_total += entry.size;
            ctx.progress.items_total += 1;
        }
    }
    let result = match format {
        Format::Zip => extract_zip(&mut ctx, path, request),
        Format::SevenZ => extract_7z(&mut ctx, path, request),
        _ => extract_tar(&mut ctx, path, format, request),
    };
    let result = report_outcome(&mut ctx, path, result);
    ctx.finish(result)
}

fn report_outcome<U: OpUi>(
    ctx: &mut Ctx<U>,
    path: &Path,
    result: Result<io::Result<()>, Cancelled>,
) -> Result<(), Cancelled> {
    match result? {
        Ok(()) => Ok(()),
        Err(e) => fail(ctx, path, &e),
    }
}

/// Files a content search extracts at once. The scratch copies are removed
/// before the next batch, so that a large archive does not fill the disk.
const FIND_BATCH_BYTES: u64 = 64 * 1024 * 1024;

/// Alt+F7 in an archive: finds entries from `folder` down whose name matches
/// `masks`, and with `text`, files that contain it. Calls `found` with each
/// entry; a folder that exists only in deeper names has just its path set. Content is read from scratch copies made by `extract`, so the
/// same safety rules apply as to any extraction.
#[allow(clippy::too_many_arguments)]
pub fn find<U: OpUi>(
    path: &Path,
    format: Format,
    folder: &str,
    masks: &str,
    text: Option<&[u8]>,
    ignore_case: bool,
    ui: &mut U,
    found: &mut dyn FnMut(ArcEntry),
) -> io::Result<()> {
    let all = list(path, format)?;
    // Folders that exist only as part of a deeper name are found too, as in a
    // real folder.
    let mut items: BTreeMap<String, &ArcEntry> = BTreeMap::new();
    let mut implied: HashSet<String> = HashSet::new();
    for entry in &all {
        if !folder.is_empty() && !entry.path.starts_with(&format!("{folder}/")) {
            continue;
        }
        items.insert(entry.path.clone(), entry);
        let mut parent = entry.path.as_str();
        while let Some((up, _)) = parent.rsplit_once('/') {
            if up == folder || up.len() <= folder.len() {
                break;
            }
            implied.insert(up.to_string());
            parent = up;
        }
    }
    let name_matches = |p: &str| util::wildcard_list(masks, p.rsplit('/').next().unwrap_or(p));
    let Some(needle) = text.filter(|t| !t.is_empty()) else {
        let mut names: Vec<&String> = items.keys().chain(implied.iter()).collect();
        names.sort();
        names.dedup();
        for name in names.into_iter().filter(|n| name_matches(n)) {
            if ui.cancelled() {
                break;
            }
            let entry = items.get(name).map_or_else(
                || ArcEntry {
                    path: name.clone(),
                    is_dir: true,
                    ..Default::default()
                },
                |e| (*e).clone(),
            );
            found(entry);
        }
        return Ok(());
    };
    let files: Vec<&ArcEntry> = items
        .values()
        .copied()
        .filter(|e| !e.is_dir && !e.is_link && name_matches(&e.path))
        .collect();
    let mut start = 0;
    while start < files.len() && !ui.cancelled() {
        let mut end = start;
        let mut bytes = 0;
        while end < files.len() && (end == start || bytes + files[end].size <= FIND_BATCH_BYTES) {
            bytes += files[end].size;
            end += 1;
        }
        let batch = &files[start..end];
        start = end;
        let scratch = ScratchDir::new()?;
        let request = ExtractRequest {
            items: batch.iter().map(|e| e.path.clone()).collect(),
            base: String::new(),
            dest: scratch.path().to_path_buf(),
        };
        extract(path, format, &request, ui);
        let stop = || ui.cancelled();
        for entry in batch {
            let copy = scratch.path().join(&entry.path);
            // A file `extract` refused, such as a hostile name, is not there.
            let is_file = fs::symlink_metadata(&copy).is_ok_and(|m| m.is_file());
            if is_file && fsops::file_contains(&copy, needle, ignore_case, &stop) {
                found((*entry).clone());
            }
        }
    }
    Ok(())
}

/// Parses a volume size as ArcLite's Volume size box takes it: a number of
/// bytes, or of K, M or G (powers of 1024), such as `700M`.
pub fn parse_volume_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let (number, unit) = match text.char_indices().last()? {
        (at, c) if c.is_ascii_alphabetic() => (&text[..at], c.to_ascii_uppercase()),
        _ => (text, 'B'),
    };
    let scale: u64 = match unit {
        'B' => 1,
        'K' => 1 << 10,
        'M' => 1 << 20,
        'G' => 1 << 30,
        _ => return None,
    };
    let value: u64 = number.trim().parse().ok()?;
    value.checked_mul(scale).filter(|&size| size > 0)
}

/// The path of volume `index`, counting from 1, of a split `archive`:
/// `name.zip.001`, as 7-Zip names them.
pub fn volume_path(archive: &Path, index: usize) -> PathBuf {
    let mut name = archive.as_os_str().to_os_string();
    name.push(format!(".{index:03}"));
    PathBuf::from(name)
}

/// For the first volume of a split archive, `name.zip.001`, the format of the
/// archive it holds.
pub fn detect_volumes(first: &Path) -> Option<Format> {
    let name = first.file_name()?.to_str()?;
    detect(Path::new(name.strip_suffix(".001")?))
}

/// The volumes of a split archive, from its first one, in order.
pub fn volumes(first: &Path) -> Vec<PathBuf> {
    let Some(archive) = first
        .to_str()
        .and_then(|p| p.strip_suffix(".001"))
        .map(PathBuf::from)
    else {
        return Vec::new();
    };
    (1..)
        .map(|i| volume_path(&archive, i))
        .take_while(|p| p.is_file())
        .collect()
}

/// Joins volumes into one file at `dest`.
pub fn join_volumes(parts: &[PathBuf], dest: &Path, stop: &dyn Fn() -> bool) -> io::Result<()> {
    let mut out = OpenOptions::new().write(true).create_new(true).open(dest)?;
    for part in parts {
        if stop() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        io::copy(&mut File::open(part)?, &mut out)?;
    }
    out.sync_all()
}

/// For the first volume of a split archive, joins all its volumes into a
/// scratch copy named after the archive, and returns it with its path. For
/// any other file, returns `None`.
pub fn join_if_split(
    file: &Path,
    stop: &dyn Fn() -> bool,
) -> io::Result<Option<(ScratchDir, PathBuf)>> {
    if detect_volumes(file).is_none() {
        return Ok(None);
    }
    let parts = volumes(file);
    if parts.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no volumes found"));
    }
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".001"))
        .unwrap_or("joined")
        .to_string();
    let scratch = ScratchDir::new()?;
    let path = scratch.path().join(name);
    join_volumes(&parts, &path, stop)?;
    Ok(Some((scratch, path)))
}

/// Cuts `src` into volumes of `size` bytes named after `archive`. Each volume
/// is written beside its place and renamed into it, and none replaces an
/// existing file. On failure the volumes made are removed.
fn split_into_volumes(
    src: &Path,
    archive: &Path,
    size: u64,
    stop: &dyn Fn() -> bool,
) -> io::Result<usize> {
    let mut made: Vec<PathBuf> = Vec::new();
    let result = (|| -> io::Result<()> {
        let mut input = File::open(src)?;
        let total = input.metadata()?.len();
        let count = total.div_ceil(size).max(1);
        for index in 1..=count as usize {
            if stop() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            let path = volume_path(archive, index);
            let temp = temp_beside(&path);
            let written = (|| -> io::Result<()> {
                let mut out = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temp)?;
                io::copy(&mut (&mut input).take(size), &mut out)?;
                out.sync_all()?;
                // A hard link fails if the name is taken, so an existing file
                // is never replaced, even one that appeared meanwhile.
                fs::hard_link(&temp, &path)
            })();
            let _ = fs::remove_file(&temp);
            written?;
            made.push(path);
        }
        Ok(())
    })();
    match result {
        Ok(()) => Ok(made.len()),
        Err(e) => {
            for path in &made {
                let _ = fs::remove_file(path);
            }
            Err(e)
        }
    }
}

/// Shift+F1 with Split into volumes: makes a new archive of `sources` and cuts
/// it into volumes of `size` bytes, `name.zip.001` and on. With `verify`, the
/// archive is read back before it is cut, for a move. Refuses when the archive
/// or its first volume exists.
pub fn add_split<U: OpUi>(
    file: &Path,
    format: Format,
    sources: &[PathBuf],
    size: u64,
    verify: bool,
    level: Level,
    ui: &mut U,
) -> OpSummary {
    let failed = |ui: &mut U, path: &Path, text: &str| {
        ui.error(path, text);
        OpSummary {
            skipped: 1,
            ..Default::default()
        }
    };
    let first = volume_path(file, 1);
    if file.exists() || first.exists() {
        let taken = if file.exists() { file } else { &first };
        return failed(
            ui,
            taken,
            "Volumes are made only for a new archive, and this file exists",
        );
    }
    // The name keeps the extension, as 7z adds one to a name without it.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let temp = file.with_file_name(format!(
        ".far_vol_{}_{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
        format.extension()
    ));
    let summary = add_with(&temp, format, "", sources, level, ui);
    let result = if summary.cancelled || summary.skipped > 0 {
        Ok(())
    } else if let Err(e) = verify.then(|| test(&temp, format)).transpose() {
        Err(e)
    } else {
        let stop = || ui.cancelled();
        split_into_volumes(&temp, file, size, &stop).map(|_| ())
    };
    let _ = fs::remove_file(&temp);
    match result {
        Ok(()) => summary,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => OpSummary {
            cancelled: true,
            ..summary
        },
        Err(e) => {
            let mut out = failed(ui, file, &e.to_string());
            out.done = summary.done;
            out
        }
    }
}

/// Reads the whole archive and returns how many items it holds.
pub fn test(path: &Path, format: Format) -> io::Result<u64> {
    match format {
        Format::Zip => {
            let mut archive = ZipArchive::new(File::open(path)?).map_err(invalid)?;
            let count = archive.len() as u64;
            for i in 0..archive.len() {
                let mut file = archive.by_index(i).map_err(invalid)?;
                // Reading to the end is what checks the CRC.
                io::copy(&mut file, &mut io::sink())?;
            }
            Ok(count)
        }
        Format::SevenZ => {
            run_7z(&[os("t"), os("-spd"), os("--"), path.as_os_str()], None)?;
            Ok(list_7z(path)?.len() as u64)
        }
        _ => {
            let mut archive = tar::Archive::new(TarIn::open(path, format)?);
            let mut count = 0;
            for entry in archive.entries()? {
                let mut entry = entry?;
                let size = entry.size();
                if io::copy(&mut entry, &mut io::sink())? != size {
                    return Err(invalid("the archive is truncated"));
                }
                count += 1;
            }
            archive.into_inner().finish()?;
            Ok(count)
        }
    }
}

/// One file or folder to put in an archive.
struct NewItem {
    /// `None` for a folder that exists only inside the archive.
    source: Option<PathBuf>,
    name: String,
}

/// Expands sources into items, not following symbolic links.
fn collect_items(
    source: &Path,
    name: &str,
    skip: &Path,
    out: &mut Vec<NewItem>,
    stop: &dyn Fn() -> bool,
) {
    if stop() || source == skip {
        return;
    }
    out.push(NewItem {
        source: Some(source.to_path_buf()),
        name: name.to_string(),
    });
    let is_dir = fs::symlink_metadata(source).is_ok_and(|m| m.is_dir());
    if !is_dir {
        return;
    }
    let Ok(entries) = fs::read_dir(source) else {
        return;
    };
    let mut names: Vec<_> = entries.flatten().map(|e| e.file_name()).collect();
    names.sort();
    for child in names {
        let Some(text) = child.to_str() else {
            continue;
        };
        collect_items(
            &source.join(&child),
            &format!("{name}/{text}"),
            skip,
            out,
            stop,
        );
    }
}

enum Writer {
    Zip(Box<ZipWriter<File>>, Level),
    Tar(Box<tar::Builder<TarOut>>),
}

impl Writer {
    fn create(file: File, format: Format, level: Level) -> io::Result<Self> {
        Ok(match format {
            Format::Zip => Writer::Zip(Box::new(ZipWriter::new(file)), level),
            _ => {
                let mut builder = tar::Builder::new(TarOut::create(file, format, level)?);
                builder.mode(tar::HeaderMode::Complete);
                Writer::Tar(Box::new(builder))
            }
        })
    }

    fn finish(self) -> io::Result<()> {
        match self {
            Writer::Zip(writer, _) => {
                let file = writer.finish().map_err(invalid)?;
                file.sync_all()
            }
            Writer::Tar(builder) => builder.into_inner()?.finish(),
        }
    }

    fn add_dir(&mut self, name: &str, mode: u32, mtime: i64) -> io::Result<()> {
        match self {
            Writer::Zip(zip, _) => {
                let options = SimpleFileOptions::default()
                    .unix_permissions(mode & 0o777)
                    .last_modified_time(zip_stamp(mtime));
                zip.add_directory(format!("{name}/"), options)
                    .map_err(invalid)
            }
            Writer::Tar(builder) => {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(mode & 0o777);
                header.set_mtime(mtime.max(0) as u64);
                header.set_size(0);
                builder.append_data(&mut header, format!("{name}/"), io::empty())
            }
        }
    }

    fn add_link(&mut self, name: &str, target: &Path, mtime: i64) -> io::Result<()> {
        match self {
            Writer::Zip(zip, _) => {
                let options = SimpleFileOptions::default()
                    .unix_permissions(0o777)
                    .last_modified_time(zip_stamp(mtime));
                zip.add_symlink(name, target.to_string_lossy(), options)
                    .map_err(invalid)
            }
            Writer::Tar(builder) => {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_mode(0o777);
                header.set_mtime(mtime.max(0) as u64);
                header.set_size(0);
                builder.append_link(&mut header, name, target)
            }
        }
    }

    fn add_file<U: OpUi>(
        &mut self,
        ctx: &mut Ctx<U>,
        name: &str,
        source: &Path,
        meta: &fs::Metadata,
    ) -> Result<io::Result<()>, Cancelled> {
        let mut input = match File::open(source) {
            Ok(file) => file,
            Err(e) => return Ok(Err(e)),
        };
        match self {
            Writer::Zip(zip, level) => {
                let method = if *level == Level::Store {
                    CompressionMethod::Stored
                } else {
                    CompressionMethod::Deflated
                };
                let options = SimpleFileOptions::default()
                    .compression_method(method)
                    .compression_level(
                        level
                            .deflate()
                            .filter(|_| method != CompressionMethod::Stored)
                            .map(i64::from),
                    )
                    .unix_permissions(meta.mode() & 0o777)
                    .last_modified_time(zip_stamp(meta.mtime()))
                    .large_file(meta.len() >= u32::MAX as u64);
                if let Err(e) = zip.start_file(name, options) {
                    return Ok(Err(invalid(e)));
                }
                let mut buf = vec![0u8; BUFFER];
                loop {
                    ctx.check()?;
                    let n = match input.read(&mut buf) {
                        Ok(0) => return Ok(Ok(())),
                        Ok(n) => n,
                        Err(e) => return Ok(Err(e)),
                    };
                    if let Err(e) = zip.write_all(&buf[..n]) {
                        return Ok(Err(e));
                    }
                    ctx.progress.bytes_done += n as u64;
                    ctx.report();
                }
            }
            Writer::Tar(builder) => {
                let mut header = tar::Header::new_gnu();
                header.set_mode(meta.mode() & 0o777);
                header.set_mtime(meta.mtime().max(0) as u64);
                header.set_size(meta.len());
                // The size is fixed in the header, so the data must match it.
                let mut limited = (&mut input).take(meta.len());
                let result = builder.append_data(&mut header, name, &mut limited);
                ctx.progress.bytes_done += meta.len();
                ctx.report();
                Ok(result)
            }
        }
    }
}

/// Copies the kept entries of an existing archive to the new one.
fn copy_existing<U: OpUi>(
    ctx: &mut Ctx<U>,
    path: &Path,
    format: Format,
    writer: &mut Writer,
    keep: &dyn Fn(&str) -> bool,
) -> Result<io::Result<()>, Cancelled> {
    match writer {
        Writer::Zip(zip, _) => {
            let mut archive =
                match File::open(path).and_then(|f| ZipArchive::new(f).map_err(invalid)) {
                    Ok(archive) => archive,
                    Err(e) => return Ok(Err(e)),
                };
            for i in 0..archive.len() {
                ctx.check()?;
                let file = match archive.by_index_raw(i) {
                    Ok(file) => file,
                    Err(e) => return Ok(Err(invalid(e))),
                };
                let name = zip_entry_name(&file);
                // Unsafe names stay as they are: changing an archive must not
                // lose entries that this program merely refuses to extract.
                if name.as_deref().is_some_and(|n| !keep(n)) {
                    continue;
                }
                if let Err(e) = zip.raw_copy_file(file) {
                    return Ok(Err(invalid(e)));
                }
            }
            Ok(Ok(()))
        }
        Writer::Tar(builder) => {
            let input = match TarIn::open(path, format) {
                Ok(input) => input,
                Err(e) => return Ok(Err(e)),
            };
            let mut archive = tar::Archive::new(input);
            let entries = match archive.entries() {
                Ok(entries) => entries,
                Err(e) => return Ok(Err(e)),
            };
            for entry in entries {
                ctx.check()?;
                let mut entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => return Ok(Err(e)),
                };
                if tar_entry_name(&entry).is_some_and(|n| !keep(&n)) {
                    continue;
                }
                let mut header = entry.header().clone();
                let raw = entry.path_bytes().into_owned();
                if tar_entry_name(&entry).is_none() {
                    // The builder refuses names like `../x`, so these go in as
                    // they came. The header alone holds at most a short name.
                    if header.path_bytes().as_ref() != raw.as_slice() {
                        return Ok(Err(invalid("an unsafe entry has too long a name to keep")));
                    }
                    if let Err(e) = builder.append(&header.clone(), &mut entry) {
                        return Ok(Err(e));
                    }
                    continue;
                }
                let name = PathBuf::from(std::ffi::OsStr::from_bytes(&raw));
                let kind = header.entry_type();
                let result = if kind.is_symlink() || kind.is_hard_link() {
                    match entry.link_name() {
                        Ok(Some(link)) => {
                            let link = link.into_owned();
                            builder.append_link(&mut header, &name, link)
                        }
                        Ok(None) => Ok(()),
                        Err(e) => Err(e),
                    }
                } else {
                    builder.append_data(&mut header, &name, &mut entry)
                };
                if let Err(e) = result {
                    return Ok(Err(e));
                }
            }
            Ok(Ok(()))
        }
    }
}

fn write_item<U: OpUi>(
    ctx: &mut Ctx<U>,
    writer: &mut Writer,
    item: &NewItem,
) -> Result<(), Cancelled> {
    ctx.progress.to = PathBuf::from(&item.name);
    let Some(source) = &item.source else {
        if let Err(e) = writer.add_dir(&item.name, 0o755, util::now_secs()) {
            fail(ctx, Path::new(&item.name), &e)?;
        } else {
            ctx.done += 1;
        }
        return Ok(());
    };
    ctx.progress.from = source.clone();
    ctx.report();
    let Some(meta) = ctx.attempt(source, || fs::symlink_metadata(source))? else {
        return Ok(());
    };
    let kind = meta.file_type();
    let result = if kind.is_dir() {
        writer.add_dir(&item.name, meta.mode(), meta.mtime())
    } else if kind.is_symlink() {
        fs::read_link(source).and_then(|link| writer.add_link(&item.name, &link, meta.mtime()))
    } else if kind.is_file() {
        writer.add_file(ctx, &item.name, source, &meta)?
    } else {
        Err(invalid("special files cannot be archived"))
    };
    match result {
        Ok(()) => ctx.done += 1,
        Err(e) => fail(ctx, source, &e)?,
    }
    ctx.progress.items_done += 1;
    Ok(())
}

/// Writes a replacement archive and swaps it in only after full success.
fn rewrite<U: OpUi>(
    ctx: &mut Ctx<U>,
    path: &Path,
    format: Format,
    keep: &dyn Fn(&str) -> bool,
    items: &[NewItem],
    level: Level,
) -> Result<io::Result<()>, Cancelled> {
    let existing = fs::metadata(path).ok();
    let temp = temp_beside(path);
    let mode = existing.as_ref().map_or(0o644, |m| m.mode() & 0o777);
    let file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temp)
    {
        Ok(file) => file,
        Err(e) => return Ok(Err(e)),
    };
    let result = (|| -> Result<io::Result<()>, Cancelled> {
        let mut writer = match Writer::create(file, format, level) {
            Ok(writer) => writer,
            Err(e) => return Ok(Err(e)),
        };
        if existing.is_some() {
            if let Err(e) = copy_existing(ctx, path, format, &mut writer, keep)? {
                return Ok(Err(e));
            }
        }
        for item in items {
            write_item(ctx, &mut writer, item)?;
        }
        Ok(writer.finish().and_then(|()| {
            if existing.is_some() {
                fs::set_permissions(&temp, fs::Permissions::from_mode(mode))?;
            }
            fs::rename(&temp, path)
        }))
    })();
    if !matches!(result, Ok(Ok(()))) {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Asks about items that already exist in the archive. Returns the items to
/// write and the names whose old entries they replace.
fn resolve_conflicts<U: OpUi>(
    ctx: &mut Ctx<U>,
    known: &[ArcEntry],
    items: Vec<NewItem>,
) -> Result<(Vec<NewItem>, HashSet<String>), Cancelled> {
    let existing: HashSet<&str> = known.iter().map(|e| e.path.as_str()).collect();
    let folders: HashSet<&str> = known
        .iter()
        .filter(|e| e.is_dir)
        .map(|e| e.path.as_str())
        .collect();
    let mut chosen = Vec::with_capacity(items.len());
    let mut replaced = HashSet::new();
    for item in items {
        let is_dir = item
            .source
            .as_ref()
            .is_none_or(|s| fs::symlink_metadata(s).is_ok_and(|m| m.is_dir()));
        if is_dir && folders.contains(item.name.as_str()) {
            continue;
        }
        if existing.contains(item.name.as_str()) {
            let source = item.source.clone().unwrap_or_default();
            if !ctx.may_overwrite(&source, Path::new(&item.name))? {
                continue;
            }
            replaced.insert(item.name.clone());
        }
        chosen.push(item);
    }
    Ok((chosen, replaced))
}

pub fn join_inner(folder: &str, name: &str) -> String {
    if folder.is_empty() {
        name.to_string()
    } else {
        format!("{folder}/{name}")
    }
}

/// Adds files to an archive under `folder`, creating the archive if needed.
pub fn add<U: OpUi>(
    path: &Path,
    format: Format,
    folder: &str,
    sources: &[PathBuf],
    ui: &mut U,
) -> OpSummary {
    add_with(path, format, folder, sources, Level::Normal, ui)
}

/// `add` with a compression level.
pub fn add_with<U: OpUi>(
    path: &Path,
    format: Format,
    folder: &str,
    sources: &[PathBuf],
    level: Level,
    ui: &mut U,
) -> OpSummary {
    let mut ctx = Ctx::new(ui, false);
    let mut totals = Totals::default();
    {
        let ui = &*ctx.ui;
        let stop = || ui.cancelled();
        for source in sources {
            fsops::scan(source, &stop, &mut totals);
        }
    }
    ctx.progress.bytes_total = totals.bytes;
    ctx.progress.items_total = totals.files + totals.dirs;
    let result = add_inner(&mut ctx, path, format, folder, sources, level);
    let result = report_outcome(&mut ctx, path, result);
    ctx.finish(result)
}

fn add_inner<U: OpUi>(
    ctx: &mut Ctx<U>,
    path: &Path,
    format: Format,
    folder: &str,
    sources: &[PathBuf],
    level: Level,
) -> Result<io::Result<()>, Cancelled> {
    let known = if path.exists() {
        match list(path, format) {
            Ok(known) => known,
            Err(e) => return Ok(Err(e)),
        }
    } else {
        Vec::new()
    };
    let mut items = Vec::new();
    {
        let ui = &*ctx.ui;
        let stop = || ui.cancelled();
        for source in sources {
            let Some(name) = source.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let inner = join_inner(folder, name);
            collect_items(source, &inner, path, &mut items, &stop);
        }
    }
    ctx.check()?;
    let (items, replaced) = resolve_conflicts(ctx, &known, items)?;
    if items.is_empty() {
        return Ok(Ok(()));
    }
    if format == Format::SevenZ {
        return add_7z(ctx, path, folder, sources, &items, level);
    }
    rewrite(
        ctx,
        path,
        format,
        &|name| !replaced.contains(name),
        &items,
        level,
    )
}

fn add_7z<U: OpUi>(
    ctx: &mut Ctx<U>,
    path: &Path,
    folder: &str,
    sources: &[PathBuf],
    items: &[NewItem],
    level: Level,
) -> Result<io::Result<()>, Cancelled> {
    let absolute = match fs::canonicalize(path.parent().unwrap_or(Path::new("."))) {
        Ok(dir) => dir.join(path.file_name().unwrap_or_default()),
        Err(e) => return Ok(Err(e)),
    };
    // 7z stores paths as given, so lay the items out in a scratch folder.
    let stage = temp_beside(&absolute);
    let result = (|| -> io::Result<()> {
        fs::create_dir(&stage)?;
        for item in items {
            let target = stage.join(&item.name);
            match &item.source {
                None => fs::create_dir_all(&target)?,
                Some(source) => {
                    let meta = fs::symlink_metadata(source)?;
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    if meta.is_dir() {
                        fs::create_dir_all(&target)?;
                    } else if meta.file_type().is_symlink() {
                        std::os::unix::fs::symlink(fs::read_link(source)?, &target)?;
                    } else {
                        fs::copy(source, &target)?;
                    }
                }
            }
        }
        let top = if folder.is_empty() {
            let mut names = Vec::new();
            for source in sources {
                names.push(source.file_name().unwrap_or_default().to_os_string());
            }
            names
        } else {
            let first = folder.split('/').next().unwrap_or(folder);
            vec![std::ffi::OsString::from(first)]
        };
        let mut args: Vec<&std::ffi::OsStr> = vec![os("a"), os("-y"), os("-spd"), os("-snl")];
        args.extend(level.seven_z().map(os));
        args.extend([os("--"), absolute.as_os_str()]);
        args.extend(top.iter().map(|n| n.as_os_str()));
        run_7z(&args, Some(&stage)).map(|_| ())
    })();
    let _ = fs::remove_dir_all(&stage);
    if result.is_ok() {
        ctx.done += items.len() as u64;
        ctx.progress.items_done += items.len() as u64;
    }
    Ok(result)
}

/// Replaces one file in an archive, after the user has already agreed to it.
pub fn update_file<U: OpUi>(
    path: &Path,
    format: Format,
    folder: &str,
    source: &Path,
    ui: &mut U,
) -> OpSummary {
    let mut ctx = Ctx::new(ui, false);
    ctx.overwrite_all = Some(true);
    let result = add_inner(
        &mut ctx,
        path,
        format,
        folder,
        &[source.to_path_buf()],
        Level::Normal,
    );
    let result = report_outcome(&mut ctx, path, result);
    ctx.finish(result)
}

/// A private temporary folder, removed when dropped.
pub struct ScratchDir(PathBuf);

impl ScratchDir {
    pub fn new() -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..100 {
            let path = std::env::temp_dir().join(format!(
                "far_linux_scratch_{}_{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut builder = fs::DirBuilder::new();
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            match builder.create(&path) {
                Ok(()) => return Ok(ScratchDir(path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(invalid("cannot create a temporary folder"))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The name of an archive without its format's extension.
pub fn stem(name: &str) -> &str {
    let lower = name.to_lowercase();
    for suffix in [
        ".tar.gz", ".tar.bz2", ".tar.xz", ".tgz", ".tbz2", ".tbz", ".txz", ".tar", ".zip", ".jar",
        ".apk", ".7z",
    ] {
        if lower.ends_with(suffix)
            && name.len() > suffix.len()
            && name.is_char_boundary(name.len() - suffix.len())
        {
            return &name[..name.len() - suffix.len()];
        }
    }
    name
}

/// Creates a folder inside an archive.
pub fn make_folder<U: OpUi>(path: &Path, format: Format, name: &str, ui: &mut U) -> OpSummary {
    let mut ctx = Ctx::new(ui, false);
    let items = vec![NewItem {
        source: None,
        name: name.to_string(),
    }];
    let result = if format == Format::SevenZ {
        add_7z(&mut ctx, path, name, &[], &items, Level::Normal)
    } else {
        rewrite(&mut ctx, path, format, &|_| true, &items, Level::Normal)
    };
    let result = report_outcome(&mut ctx, path, result);
    ctx.finish(result)
}

/// Removes items, and everything under them, from an archive.
pub fn delete<U: OpUi>(path: &Path, format: Format, names: &[String], ui: &mut U) -> OpSummary {
    let mut ctx = Ctx::new(ui, false);
    let result = if format == Format::SevenZ {
        let mut args: Vec<&std::ffi::OsStr> =
            vec![os("d"), os("-y"), os("-spd"), os("--"), path.as_os_str()];
        args.extend(names.iter().map(|n| os(n)));
        Ok(run_7z(&args, None).map(|_| ()))
    } else {
        rewrite(
            &mut ctx,
            path,
            format,
            &|entry| !names.iter().any(|n| is_under(entry, n)),
            &[],
            Level::Normal,
        )
    };
    if matches!(result, Ok(Ok(()))) {
        ctx.done = names.len() as u64;
    }
    let result = report_outcome(&mut ctx, path, result);
    ctx.finish(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsops::{ErrorChoice, OverwriteChoice, Progress, TrashChoice};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "far_arc_test_{}_{}_{tag}",
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

    struct Ui {
        overwrite: OverwriteChoice,
        errors: Vec<String>,
        prompts: u32,
        cancel: bool,
    }

    impl Default for Ui {
        fn default() -> Self {
            Ui {
                overwrite: OverwriteChoice::Overwrite,
                errors: Vec::new(),
                prompts: 0,
                cancel: false,
            }
        }
    }

    impl OpUi for Ui {
        fn confirm_delete(
            &mut self,
            _: &Path,
            _: crate::fsops::DeleteAsk,
        ) -> crate::fsops::DeleteChoice {
            crate::fsops::DeleteChoice::Delete
        }
        fn cancelled(&self) -> bool {
            self.cancel
        }
        fn progress(&mut self, _: &Progress) {}
        fn overwrite(&mut self, _: &Path, _: &Path) -> OverwriteChoice {
            self.prompts += 1;
            self.overwrite
        }
        fn error(&mut self, _: &Path, message: &str) -> ErrorChoice {
            self.errors.push(message.to_string());
            ErrorChoice::Skip
        }
        fn trash_fallback(&mut self, _: &Path) -> TrashChoice {
            TrashChoice::Cancel
        }
    }

    /// Every format that can run on this machine.
    fn formats() -> Vec<Format> {
        FORMATS.into_iter().filter(|f| f.is_available()).collect()
    }

    fn all_to(dest: &Path) -> ExtractRequest {
        ExtractRequest {
            dest: dest.to_path_buf(),
            ..Default::default()
        }
    }

    fn paths(archive: &Path, format: Format) -> Vec<String> {
        let mut names: Vec<String> = list(archive, format)
            .unwrap()
            .into_iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.path)
            .collect();
        names.sort();
        names
    }

    #[test]
    fn every_format_round_trips_contents_modes_and_links() {
        for format in formats() {
            let d = TestDir::new("round");
            d.write("src/top.txt", "top");
            d.write("src/sub/deep.txt", "deep");
            d.write("src/имя файла.txt", "unicode");
            fs::create_dir(d.p("src/empty")).unwrap();
            std::os::unix::fs::symlink("top.txt", d.p("src/link")).unwrap();
            fs::set_permissions(d.p("src/top.txt"), fs::Permissions::from_mode(0o750)).unwrap();
            let archive = d.p(&format!("out.{}", format.extension()));
            let mut ui = Ui::default();
            let s = add(&archive, format, "", &[d.p("src")], &mut ui);
            assert!(ui.errors.is_empty(), "{format:?}: {:?}", ui.errors);
            assert_eq!(s.skipped, 0, "{format:?}");
            assert!(test(&archive, format).is_ok(), "{format:?}");
            assert!(paths(&archive, format).contains(&"src/sub/deep.txt".to_string()));

            let mut ui = Ui::default();
            let dest = d.p("dest");
            let s = extract(&archive, format, &all_to(&dest), &mut ui);
            assert!(ui.errors.is_empty(), "{format:?}: {:?}", ui.errors);
            assert_eq!(s.skipped, 0, "{format:?}");
            assert_eq!(
                fs::read_to_string(dest.join("src/sub/deep.txt")).unwrap(),
                "deep"
            );
            assert_eq!(
                fs::read_to_string(dest.join("src/имя файла.txt")).unwrap(),
                "unicode"
            );
            assert!(dest.join("src/empty").is_dir(), "{format:?}");
            let link = fs::symlink_metadata(dest.join("src/link")).unwrap();
            assert!(link.file_type().is_symlink(), "{format:?}");
            let mode = fs::metadata(dest.join("src/top.txt")).unwrap().mode() & 0o777;
            assert_eq!(mode, 0o750, "{format:?}");
        }
    }

    fn found(
        archive: &Path,
        format: Format,
        folder: &str,
        masks: &str,
        text: Option<&str>,
    ) -> Vec<String> {
        let mut hits = Vec::new();
        let mut ui = Ui::default();
        find(
            archive,
            format,
            folder,
            masks,
            text.map(str::as_bytes),
            true,
            &mut ui,
            &mut |e| hits.push(e.path),
        )
        .unwrap();
        assert!(ui.errors.is_empty(), "{format:?}: {:?}", ui.errors);
        hits
    }

    #[test]
    fn find_searches_names_and_contents_inside_the_archive() {
        for format in formats() {
            let d = TestDir::new("find");
            d.write("a/b/one.txt", "needle here");
            d.write("a/b/two.log", "nothing");
            d.write("a/three.txt", "NEEDLE too");
            let archive = d.p(&format!("x.{}", format.extension()));
            add(&archive, format, "", &[d.p("a")], &mut Ui::default());
            // A file of the same name beside the archive is not found.
            d.write("one.txt", "needle");
            let names = found(&archive, format, "", "*.txt", None);
            assert_eq!(names, ["a/b/one.txt", "a/three.txt"], "{format:?}");
            assert_eq!(
                found(&archive, format, "a/b", "*", None),
                ["a/b/one.txt", "a/b/two.log"],
                "{format:?}"
            );
            assert_eq!(
                found(&archive, format, "", "b", None),
                ["a/b"],
                "{format:?}"
            );
            let text = found(&archive, format, "", "*", Some("needle"));
            assert_eq!(text, ["a/b/one.txt", "a/three.txt"], "{format:?}");
            assert!(
                found(&archive, format, "a/b", "*", Some("too")).is_empty(),
                "{format:?}"
            );
        }
    }

    #[test]
    fn find_in_a_hostile_archive_reads_nothing_outside_the_scratch_copy() {
        let d = TestDir::new("findhostile");
        d.write("secret", "needle");
        let archive = d.p("evil.tar");
        hostile_tar(
            &archive,
            &[
                ("link", tar::EntryType::Symlink, "../secret", b""),
                ("../up", tar::EntryType::Regular, "", b"needle"),
                ("plain", tar::EntryType::Regular, "", b"needle"),
            ],
        );
        let mut hits = Vec::new();
        let mut ui = Ui::default();
        find(
            &archive,
            Format::Tar,
            "",
            "*",
            Some(b"needle"),
            true,
            &mut ui,
            &mut |e| hits.push(e.path),
        )
        .unwrap();
        assert_eq!(hits, ["plain"]);
    }

    #[test]
    fn compression_levels_reach_every_format() {
        for format in formats() {
            let d = TestDir::new("level");
            d.write("src/text.txt", &"far manager level test ".repeat(4000));
            let mut sizes = Vec::new();
            for level in LEVELS {
                let file = d.p(&format!("{}.{}", level.name(), format.extension()));
                let mut ui = Ui::default();
                let s = add_with(&file, format, "", &[d.p("src")], level, &mut ui);
                assert_eq!(s.skipped, 0, "{format:?} {level:?}: {:?}", ui.errors);
                assert!(paths(&file, format).contains(&"src/text.txt".to_string()));
                sizes.push(fs::metadata(&file).unwrap().len());
            }
            let (store, ultra) = (sizes[0], sizes[4]);
            match format {
                Format::Tar => assert_eq!(store, ultra, "tar does not compress"),
                // bzip2 and xz cannot store; Store is their fastest level.
                Format::TarBz2 | Format::TarXz => {}
                _ => assert!(
                    store > ultra * 2,
                    "{format:?}: store {store}, ultra {ultra}"
                ),
            }
        }
    }

    #[test]
    fn volume_sizes_take_k_m_and_g() {
        assert_eq!(parse_volume_size("1000"), Some(1000));
        assert_eq!(parse_volume_size("64k"), Some(64 << 10));
        assert_eq!(parse_volume_size(" 700M "), Some(700 << 20));
        assert_eq!(parse_volume_size("2G"), Some(2 << 30));
        for bad in ["", "0", "M", "1.5M", "10X", "-1", "99999999999999999999G"] {
            assert_eq!(parse_volume_size(bad), None, "{bad}");
        }
        let a = Path::new("/x/data.tar.gz");
        assert_eq!(volume_path(a, 1), Path::new("/x/data.tar.gz.001"));
        assert_eq!(volume_path(a, 1234), Path::new("/x/data.tar.gz.1234"));
        assert_eq!(
            detect_volumes(Path::new("/x/data.tar.gz.001")),
            Some(Format::TarGz)
        );
        assert_eq!(detect_volumes(Path::new("/x/data.tar.gz.002")), None);
        assert_eq!(detect_volumes(Path::new("/x/data.001")), None);
    }

    /// Files beside the archive, other than the volumes: a temporary file
    /// left behind would show here.
    fn leftovers(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn every_format_splits_into_volumes_that_join_back() {
        for format in formats() {
            let d = TestDir::new("vol");
            // Random-looking data, so that compression leaves several volumes.
            let mut seed = 7u64;
            let noise: String = (0..20_000)
                .map(|_| {
                    seed = seed
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    char::from(b'a' + (seed >> 59) as u8)
                })
                .collect();
            d.write("src/a.txt", &noise);
            d.write("src/sub/b.txt", "bbb");
            fs::create_dir(d.p("out")).unwrap();
            let file = d.p(&format!("out/x.{}", format.extension()));
            let mut ui = Ui::default();
            let s = add_split(
                &file,
                format,
                &[d.p("src")],
                4096,
                true,
                Level::Normal,
                &mut ui,
            );
            assert_eq!(
                (s.skipped, s.cancelled),
                (0, false),
                "{format:?}: {:?}",
                ui.errors
            );
            assert!(!file.exists(), "{format:?}: only volumes are left");
            let first = volume_path(&file, 1);
            let parts = volumes(&first);
            assert!(parts.len() >= 2, "{format:?}: {parts:?}");
            for part in &parts[..parts.len() - 1] {
                assert_eq!(fs::metadata(part).unwrap().len(), 4096, "{format:?}");
            }
            assert_eq!(leftovers(&d.p("out")), Vec::<String>::new(), "{format:?}");
            let (_scratch, joined) = join_if_split(&first, &|| false).unwrap().unwrap();
            assert!(join_if_split(&d.p("src/a.txt"), &|| false)
                .unwrap()
                .is_none());
            // The same entries as the archive made whole.
            let whole = d.p(&format!("whole.{}", format.extension()));
            add(&whole, format, "", &[d.p("src")], &mut Ui::default());
            assert_eq!(paths(&joined, format), paths(&whole, format), "{format:?}");
            assert!(paths(&joined, format).contains(&"src/sub/b.txt".to_string()));
        }
    }

    #[test]
    fn volumes_are_refused_over_an_existing_archive_or_volume() {
        let d = TestDir::new("volexist");
        d.write("a.txt", "a");
        let file = d.p("x.zip");
        fs::write(&file, "old").unwrap();
        let mut ui = Ui::default();
        let s = add_split(
            &file,
            Format::Zip,
            &[d.p("a.txt")],
            1024,
            false,
            Level::Normal,
            &mut ui,
        );
        assert_eq!((s.skipped, ui.errors.len()), (1, 1));
        assert_eq!(fs::read_to_string(&file).unwrap(), "old");
        fs::remove_file(&file).unwrap();
        fs::write(volume_path(&file, 1), "old volume").unwrap();
        let s = add_split(
            &file,
            Format::Zip,
            &[d.p("a.txt")],
            1024,
            false,
            Level::Normal,
            &mut ui,
        );
        assert_eq!(s.skipped, 1);
        assert_eq!(
            fs::read_to_string(volume_path(&file, 1)).unwrap(),
            "old volume"
        );
        assert!(!volume_path(&file, 2).exists());
        assert!(!file.exists());
    }

    #[test]
    fn a_cancelled_split_leaves_nothing_behind() {
        let d = TestDir::new("volcancel");
        d.write("a.txt", &"x".repeat(10_000));
        fs::create_dir(d.p("out")).unwrap();
        let file = d.p("out/x.tar");
        let mut ui = Ui {
            cancel: true,
            ..Default::default()
        };
        let s = add_split(
            &file,
            Format::Tar,
            &[d.p("a.txt")],
            1024,
            false,
            Level::Normal,
            &mut ui,
        );
        assert!(s.cancelled);
        let names: Vec<_> = fs::read_dir(d.p("out")).unwrap().flatten().collect();
        assert!(names.is_empty(), "{names:?}");
    }

    #[test]
    fn selected_items_extract_relative_to_the_open_folder() {
        for format in formats() {
            let d = TestDir::new("sel");
            d.write("a/b/one.txt", "1");
            d.write("a/b/two.txt", "2");
            d.write("a/other.txt", "3");
            let archive = d.p(&format!("x.{}", format.extension()));
            add(&archive, format, "", &[d.p("a")], &mut Ui::default());
            let dest = d.p("dest");
            let request = ExtractRequest {
                items: vec!["a/b/one.txt".into()],
                base: "a/b".into(),
                dest: dest.clone(),
            };
            let mut ui = Ui::default();
            extract(&archive, format, &request, &mut ui);
            assert!(ui.errors.is_empty(), "{format:?}: {:?}", ui.errors);
            assert_eq!(
                fs::read_to_string(dest.join("one.txt")).unwrap(),
                "1",
                "{format:?}"
            );
            assert!(!dest.join("two.txt").exists(), "{format:?}");
            assert!(!dest.join("a").exists(), "{format:?}");
        }
    }

    #[test]
    fn extract_asks_before_overwriting_and_skip_keeps_the_file() {
        for format in formats() {
            let d = TestDir::new("over");
            d.write("f.txt", "new");
            let archive = d.p(&format!("x.{}", format.extension()));
            add(&archive, format, "", &[d.p("f.txt")], &mut Ui::default());
            let dest = d.p("dest");
            fs::create_dir(&dest).unwrap();
            fs::write(dest.join("f.txt"), "old").unwrap();
            let mut ui = Ui {
                overwrite: OverwriteChoice::Skip,
                ..Default::default()
            };
            extract(&archive, format, &all_to(&dest), &mut ui);
            assert_eq!(ui.prompts, 1, "{format:?}");
            assert_eq!(
                fs::read_to_string(dest.join("f.txt")).unwrap(),
                "old",
                "{format:?}"
            );
            let mut ui = Ui::default();
            extract(&archive, format, &all_to(&dest), &mut ui);
            assert_eq!(
                fs::read_to_string(dest.join("f.txt")).unwrap(),
                "new",
                "{format:?}"
            );
        }
    }

    #[test]
    fn adding_keeps_existing_entries_and_asks_about_conflicts() {
        for format in formats() {
            let d = TestDir::new("add");
            let first = d.write("one.txt", "1");
            let second = d.write("two.txt", "2");
            let archive = d.p(&format!("x.{}", format.extension()));
            add(
                &archive,
                format,
                "",
                std::slice::from_ref(&first),
                &mut Ui::default(),
            );
            let mut ui = Ui::default();
            add(&archive, format, "folder", &[second], &mut ui);
            assert!(ui.errors.is_empty(), "{format:?}: {:?}", ui.errors);
            assert_eq!(
                paths(&archive, format),
                ["folder/two.txt", "one.txt"],
                "{format:?}"
            );

            fs::write(&first, "changed").unwrap();
            let mut ui = Ui {
                overwrite: OverwriteChoice::Skip,
                ..Default::default()
            };
            add(&archive, format, "", std::slice::from_ref(&first), &mut ui);
            assert_eq!(ui.prompts, 1, "{format:?}");
            let dest = d.p("skip");
            extract(&archive, format, &all_to(&dest), &mut Ui::default());
            assert_eq!(
                fs::read_to_string(dest.join("one.txt")).unwrap(),
                "1",
                "{format:?}"
            );

            add(&archive, format, "", &[first], &mut Ui::default());
            let dest = d.p("over");
            extract(&archive, format, &all_to(&dest), &mut Ui::default());
            assert_eq!(
                fs::read_to_string(dest.join("one.txt")).unwrap(),
                "changed",
                "{format:?}"
            );
            assert_eq!(
                fs::read_to_string(dest.join("folder/two.txt")).unwrap(),
                "2",
                "{format:?}"
            );
        }
    }

    #[test]
    fn delete_removes_items_and_folder_contents_only() {
        for format in formats() {
            let d = TestDir::new("del");
            d.write("t/keep.txt", "k");
            d.write("t/dir/gone.txt", "g");
            d.write("t/dir2/stay.txt", "s");
            let archive = d.p(&format!("x.{}", format.extension()));
            add(&archive, format, "", &[d.p("t")], &mut Ui::default());
            let mut ui = Ui::default();
            delete(&archive, format, &["t/dir".to_string()], &mut ui);
            assert!(ui.errors.is_empty(), "{format:?}: {:?}", ui.errors);
            assert_eq!(
                paths(&archive, format),
                ["t/dir2/stay.txt", "t/keep.txt"],
                "{format:?}"
            );
            assert!(test(&archive, format).is_ok(), "{format:?}");
        }
    }

    #[test]
    fn make_folder_adds_an_empty_folder() {
        for format in formats() {
            let d = TestDir::new("mk");
            let f = d.write("f.txt", "x");
            let archive = d.p(&format!("x.{}", format.extension()));
            add(&archive, format, "", &[f], &mut Ui::default());
            let mut ui = Ui::default();
            make_folder(&archive, format, "new/inner", &mut ui);
            assert!(ui.errors.is_empty(), "{format:?}: {:?}", ui.errors);
            let all = list(&archive, format).unwrap();
            assert!(
                all.iter().any(|e| e.path == "new/inner" && e.is_dir),
                "{format:?}: {all:?}"
            );
            assert!(all.iter().any(|e| e.path == "f.txt"), "{format:?}");
        }
    }

    #[test]
    fn cancelling_or_failing_leaves_the_original_archive_alone() {
        for format in [Format::Zip, Format::TarGz, Format::Tar] {
            let d = TestDir::new("safe");
            let f = d.write("f.txt", "x");
            let g = d.write("g.txt", "y");
            let archive = d.p(&format!("x.{}", format.extension()));
            add(&archive, format, "", &[f], &mut Ui::default());
            let before = fs::read(&archive).unwrap();
            let mut ui = Ui {
                cancel: true,
                ..Default::default()
            };
            let s = add(&archive, format, "", &[g], &mut ui);
            assert!(s.cancelled, "{format:?}");
            assert_eq!(fs::read(&archive).unwrap(), before, "{format:?}");
            let leftovers = fs::read_dir(&d.0)
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(".far_arc"))
                .count();
            assert_eq!(leftovers, 0, "{format:?}");
        }
    }

    #[test]
    fn archiving_a_folder_holding_the_archive_skips_the_archive() {
        let d = TestDir::new("self");
        d.write("dir/f.txt", "x");
        let archive = d.p("dir/own.zip");
        add(&archive, Format::Zip, "", &[d.p("dir")], &mut Ui::default());
        add(&archive, Format::Zip, "", &[d.p("dir")], &mut Ui::default());
        assert!(!paths(&archive, Format::Zip)
            .iter()
            .any(|p| p.ends_with("own.zip")));
    }

    fn hostile_tar(path: &Path, entries: &[(&str, tar::EntryType, &str, &[u8])]) {
        let mut builder = tar::Builder::new(File::create(path).unwrap());
        for (name, kind, link, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_mode(0o644);
            header.set_size(data.len() as u64);
            // Written by hand: the tar crate refuses to build these names.
            let bytes = name.as_bytes();
            header.as_old_mut().name[..bytes.len()].copy_from_slice(bytes);
            if !link.is_empty() {
                let bytes = link.as_bytes();
                header.as_old_mut().linkname[..bytes.len()].copy_from_slice(bytes);
            }
            header.set_cksum();
            builder.append(&header, *data).unwrap();
        }
        builder.finish().unwrap();
    }

    #[test]
    fn hostile_tar_names_cannot_leave_the_destination() {
        let d = TestDir::new("slip");
        let archive = d.p("evil.tar");
        let reg = tar::EntryType::Regular;
        hostile_tar(
            &archive,
            &[
                ("../escaped.txt", reg, "", b"bad"),
                ("a/../../escaped2.txt", reg, "", b"bad"),
                ("/tmp/far_absolute_escape.txt", reg, "", b"bad"),
                ("good.txt", reg, "", b"ok"),
            ],
        );
        let dest = d.p("x/dest");
        let mut ui = Ui::default();
        extract(&archive, Format::Tar, &all_to(&dest), &mut ui);
        assert_eq!(ui.errors.len(), 3, "{:?}", ui.errors);
        assert_eq!(fs::read_to_string(dest.join("good.txt")).unwrap(), "ok");
        assert!(!d.p("x/escaped.txt").exists());
        assert!(!d.p("escaped2.txt").exists());
        assert!(!Path::new("/tmp/far_absolute_escape.txt").exists());
        assert_eq!(paths(&archive, Format::Tar), ["good.txt"]);
    }

    #[test]
    fn hostile_tar_symlink_cannot_redirect_a_later_file() {
        let d = TestDir::new("link");
        let outside = d.p("outside");
        fs::create_dir(&outside).unwrap();
        let archive = d.p("evil.tar");
        let target = outside.display().to_string();
        hostile_tar(
            &archive,
            &[
                ("door", tar::EntryType::Symlink, &target, b""),
                ("door/pwned.txt", tar::EntryType::Regular, "", b"bad"),
            ],
        );
        let dest = d.p("dest");
        let mut ui = Ui::default();
        extract(&archive, Format::Tar, &all_to(&dest), &mut ui);
        assert!(!outside.join("pwned.txt").exists());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn existing_symlink_in_the_destination_is_not_followed() {
        let d = TestDir::new("exist");
        let outside = d.p("outside");
        fs::create_dir(&outside).unwrap();
        let dest = d.p("dest");
        fs::create_dir(&dest).unwrap();
        std::os::unix::fs::symlink(&outside, dest.join("door")).unwrap();
        let archive = d.p("x.tar");
        hostile_tar(
            &archive,
            &[("door/pwned.txt", tar::EntryType::Regular, "", b"bad")],
        );
        let mut ui = Ui::default();
        extract(&archive, Format::Tar, &all_to(&dest), &mut ui);
        assert_eq!(ui.errors.len(), 1);
        assert!(!outside.join("pwned.txt").exists());
    }

    #[test]
    fn hostile_hard_link_cannot_reach_outside_files() {
        let d = TestDir::new("hard");
        let secret = d.write("secret.txt", "secret");
        let archive = d.p("x.tar");
        let absolute = secret.display().to_string();
        hostile_tar(
            &archive,
            &[
                ("copy1", tar::EntryType::Link, &absolute, b""),
                ("copy2", tar::EntryType::Link, "../secret.txt", b""),
            ],
        );
        let dest = d.p("dest");
        let mut ui = Ui::default();
        extract(&archive, Format::Tar, &all_to(&dest), &mut ui);
        assert_eq!(ui.errors.len(), 2, "{:?}", ui.errors);
        assert!(!dest.join("copy1").exists());
        assert!(!dest.join("copy2").exists());
    }

    #[test]
    fn extracted_files_never_get_setuid() {
        let d = TestDir::new("suid");
        let archive = d.p("x.tar");
        let mut builder = tar::Builder::new(File::create(&archive).unwrap());
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o4755);
        header.set_size(1);
        header.set_cksum();
        builder.append_data(&mut header, "prog", &b"x"[..]).unwrap();
        builder.finish().unwrap();
        drop(builder);
        let dest = d.p("dest");
        extract(&archive, Format::Tar, &all_to(&dest), &mut Ui::default());
        assert_eq!(
            fs::metadata(dest.join("prog")).unwrap().mode() & 0o7777,
            0o755
        );
    }

    #[test]
    fn hostile_zip_names_cannot_leave_the_destination() {
        let d = TestDir::new("zipslip");
        let archive = d.p("evil.zip");
        let mut zip = ZipWriter::new(File::create(&archive).unwrap());
        let options = SimpleFileOptions::default();
        for name in [
            "../escaped.txt",
            "..\\escaped3.txt",
            "/tmp/far_zip_abs.txt",
            "ok.txt",
        ] {
            zip.start_file(name, options).unwrap();
            zip.write_all(b"x").unwrap();
        }
        zip.finish().unwrap();
        let dest = d.p("x/dest");
        let mut ui = Ui::default();
        extract(&archive, Format::Zip, &all_to(&dest), &mut ui);
        assert_eq!(ui.errors.len(), 3, "{:?}", ui.errors);
        assert!(dest.join("ok.txt").exists());
        assert!(!d.p("x/escaped.txt").exists());
        assert!(!d.p("x/escaped3.txt").exists());
        assert!(!Path::new("/tmp/far_zip_abs.txt").exists());
    }

    #[test]
    fn changing_an_archive_keeps_entries_it_will_not_extract() {
        let d = TestDir::new("keep");
        let archive = d.p("x.tar");
        hostile_tar(
            &archive,
            &[
                ("../odd.txt", tar::EntryType::Regular, "", b"odd"),
                ("normal.txt", tar::EntryType::Regular, "", b"n"),
            ],
        );
        delete(
            &archive,
            Format::Tar,
            &["normal.txt".to_string()],
            &mut Ui::default(),
        );
        let mut read = tar::Archive::new(File::open(&archive).unwrap());
        let names: Vec<String> = read
            .entries()
            .unwrap()
            .map(|e| String::from_utf8_lossy(&e.unwrap().path_bytes()).into_owned())
            .collect();
        assert_eq!(names, ["../odd.txt"]);
    }

    #[test]
    fn damaged_archives_are_errors_not_panics() {
        for format in formats() {
            let d = TestDir::new("bad");
            let f = d.write("big.txt", &"data ".repeat(20000));
            let archive = d.p(&format!("x.{}", format.extension()));
            add(&archive, format, "", &[f], &mut Ui::default());
            let bytes = fs::read(&archive).unwrap();
            fs::write(&archive, &bytes[..bytes.len() / 2]).unwrap();
            assert!(test(&archive, format).is_err(), "{format:?}");
            let garbage = d.p(&format!("g.{}", format.extension()));
            fs::write(&garbage, "not an archive").unwrap();
            assert!(test(&garbage, format).is_err(), "{format:?}");
            let mut ui = Ui::default();
            extract(&garbage, format, &all_to(&d.p("out")), &mut ui);
        }
    }

    #[test]
    fn formats_are_detected_by_name() {
        assert_eq!(detect(Path::new("a.zip")), Some(Format::Zip));
        assert_eq!(detect(Path::new("A.TAR.GZ")), Some(Format::TarGz));
        assert_eq!(detect(Path::new("a.tgz")), Some(Format::TarGz));
        assert_eq!(detect(Path::new("a.tar")), Some(Format::Tar));
        assert_eq!(detect(Path::new("a.7z")), Some(Format::SevenZ));
        assert_eq!(detect(Path::new("a.txt")), None);
        assert_eq!(detect(Path::new(".zip")), None);
    }

    #[test]
    fn stems_drop_only_a_known_extension() {
        assert_eq!(stem("a.tar.gz"), "a");
        assert_eq!(stem("a.b.ZIP"), "a.b");
        assert_eq!(stem("a.txt"), "a.txt");
        assert_eq!(stem(".zip"), ".zip");
    }

    #[test]
    fn update_replaces_without_asking() {
        let d = TestDir::new("upd");
        let f = d.write("f.txt", "one");
        let archive = d.p("x.zip");
        add(
            &archive,
            Format::Zip,
            "dir",
            std::slice::from_ref(&f),
            &mut Ui::default(),
        );
        fs::write(&f, "two").unwrap();
        let mut ui = Ui::default();
        update_file(&archive, Format::Zip, "dir", &f, &mut ui);
        assert_eq!(ui.prompts, 0);
        let dest = d.p("dest");
        extract(&archive, Format::Zip, &all_to(&dest), &mut Ui::default());
        assert_eq!(fs::read_to_string(dest.join("dir/f.txt")).unwrap(), "two");
    }

    #[test]
    fn scratch_folders_are_private_and_removed() {
        let scratch = ScratchDir::new().unwrap();
        let path = scratch.path().to_path_buf();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
        drop(scratch);
        assert!(!path.exists());
    }

    #[test]
    fn name_cleaning() {
        assert_eq!(safe_name("./a//b/").as_deref(), Some("a/b"));
        assert_eq!(safe_name("a/../b"), None);
        assert_eq!(safe_name("../a"), None);
        assert_eq!(safe_name("/a"), None);
        assert_eq!(safe_name(""), None);
        assert_eq!(safe_name("."), None);
        assert_eq!(safe_name("a\0b"), None);
    }

    #[test]
    fn children_include_implied_folders() {
        let entry = |path: &str, is_dir| ArcEntry {
            path: path.into(),
            is_dir,
            ..Default::default()
        };
        let all = vec![
            entry("a/b/c.txt", false),
            entry("a/d.txt", false),
            entry("e", true),
            entry("abc.txt", false),
        ];
        let names = |folder| -> Vec<(String, bool)> {
            children(&all, folder)
                .into_iter()
                .map(|e| (e.path, e.is_dir))
                .collect()
        };
        assert_eq!(
            names(""),
            [
                ("a".to_string(), true),
                ("abc.txt".to_string(), false),
                ("e".to_string(), true)
            ]
        );
        assert_eq!(
            names("a"),
            [("a/b".to_string(), true), ("a/d.txt".to_string(), false)]
        );
        assert!(names("zzz").is_empty());
    }

    #[test]
    fn membership_respects_path_boundaries() {
        assert!(is_under("a/b", "a"));
        assert!(is_under("a", "a"));
        assert!(!is_under("ab", "a"));
    }
}
