//! Files copied to the clipboard, transferred when control moves (desktop).
//!
//! A shell that sees files on the system clipboard reports them as
//! `text/uri-list` (`file://` URLs). The core replaces that with a manifest
//! (`application/x-linkcursor-files`: relative paths and sizes, never local
//! paths), which travels with the clipboard. A receiver that accepts it pulls
//! the files entry by entry over the data link (`FileRequest` / `FileChunk`),
//! writes each to a `.partial` file next to its destination and renames it
//! once the length matches. When all are in, the receiver's clipboard becomes
//! a `text/uri-list` of what it now has, so pasting gives the files.
//!
//! Receivers trust nothing in a manifest: every path is cleaned again
//! ([`sanitize`]), top-level names that already exist get a number, sizes are
//! checked while writing, and nothing ever lands outside the download
//! directory.

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

pub const MIME_URI_LIST: &str = "text/uri-list";
pub const MIME_FILES: &str = "application/x-linkcursor-files";
/// Bytes per `FileChunk`.
pub const FILE_CHUNK_SIZE: usize = 64 * 1024;
/// Entries (files and folders) one transfer may hold.
pub const MAX_ENTRIES: usize = 10_000;
const MAX_DEPTH: usize = 32;
const MAX_PATH_LENGTH: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub transfer: u32,
    pub total_size: u64,
    pub entries: Vec<Entry>,
}

/// A file or folder, by its path relative to the transfer (`/` separated).
/// Folders come before what they contain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dir: bool,
}

/// Files this device offers: the manifest and where each entry really is.
#[derive(Debug, Clone)]
pub struct Offer {
    pub manifest: Manifest,
    /// The paths the shell reported, to recognise the same copy again.
    pub roots: Vec<PathBuf>,
    sources: Vec<Source>,
}

#[derive(Debug, Clone)]
struct Source {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

impl Offer {
    /// Lists `roots` (files and folders, recursively; symbolic links are
    /// left out) as transfer `transfer`.
    pub fn build(transfer: u32, roots: &[PathBuf]) -> Result<Self, String> {
        let mut offer = Self {
            manifest: Manifest {
                transfer,
                total_size: 0,
                entries: Vec::new(),
            },
            roots: roots.to_vec(),
            sources: Vec::new(),
        };
        for root in roots {
            let name = root
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| format!("unsupported file name: {}", root.display()))?;
            offer.add(root, name.to_owned(), 0)?;
        }
        if offer.manifest.entries.is_empty() {
            return Err("no files to offer".to_owned());
        }
        Ok(offer)
    }

    fn add(&mut self, path: &Path, relative: String, depth: usize) -> Result<(), String> {
        if self.manifest.entries.len() >= MAX_ENTRIES || depth > MAX_DEPTH {
            return Err("too many files".to_owned());
        }
        let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() {
            return Ok(());
        }
        if metadata.is_dir() {
            self.push(path, relative.clone(), 0, true, None);
            let mut children: Vec<_> = fs::read_dir(path)
                .map_err(|error| error.to_string())?
                .filter_map(Result::ok)
                .collect();
            children.sort_by_key(|child| child.file_name());
            for child in children {
                if let Some(name) = child.file_name().to_str() {
                    self.add(&child.path(), format!("{relative}/{name}"), depth + 1)?;
                }
            }
        } else if metadata.is_file() {
            self.manifest.total_size += metadata.len();
            self.push(
                path,
                relative,
                metadata.len(),
                false,
                metadata.modified().ok(),
            );
        }
        Ok(())
    }

    fn push(
        &mut self,
        path: &Path,
        relative: String,
        size: u64,
        dir: bool,
        modified: Option<SystemTime>,
    ) {
        self.manifest.entries.push(Entry {
            path: relative,
            size,
            dir,
        });
        self.sources.push(Source {
            path: path.to_owned(),
            size,
            modified,
        });
    }

    /// Opens entry `entry` for sending, failing if it is a folder or changed
    /// since it was offered (a half-new, half-old file must never arrive).
    pub fn open(&self, entry: usize) -> Result<(PathBuf, u64), String> {
        let source = self.sources.get(entry).ok_or("no such entry")?;
        if self.manifest.entries[entry].dir {
            return Err("entry is a folder".to_owned());
        }
        let metadata = fs::metadata(&source.path).map_err(|error| error.to_string())?;
        if metadata.len() != source.size || metadata.modified().ok() != source.modified {
            return Err("file changed since it was copied".to_owned());
        }
        Ok((source.path.clone(), source.size))
    }
}

impl Manifest {
    /// Checks what a receiver relies on before planning anything.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.entries.is_empty() || self.entries.len() > MAX_ENTRIES {
            return Err("invalid number of files");
        }
        let mut total: u64 = 0;
        for entry in &self.entries {
            if sanitize(&entry.path).is_none() {
                return Err("unsafe file name");
            }
            if entry.dir && entry.size != 0 {
                return Err("folder with a size");
            }
            total = total.checked_add(entry.size).ok_or("sizes overflow")?;
        }
        if total != self.total_size {
            return Err("sizes do not add up");
        }
        Ok(())
    }
}

/// A relative manifest path made safe for this device's file system, or
/// `None` when it cannot be. Rejects absolute paths, drive or UNC prefixes,
/// `.` and `..`; strips control characters; replaces characters Windows does
/// not allow (everywhere, so names stay portable); renames reserved device
/// names (`CON`, `NUL`, `COM1`…) and drops trailing dots and spaces.
pub fn sanitize(path: &str) -> Option<PathBuf> {
    if path.is_empty() || path.len() > MAX_PATH_LENGTH || path.contains('\\') {
        return None;
    }
    let mut clean = PathBuf::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains(':') {
            return None;
        }
        let mut name: String = part
            .chars()
            .filter(|c| !c.is_control())
            .map(|c| if "<>\"|?*".contains(c) { '_' } else { c })
            .collect();
        while name.ends_with('.') || name.ends_with(' ') {
            name.pop();
        }
        if name.is_empty() {
            return None;
        }
        let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit());
        if reserved {
            name.insert(0, '_');
        }
        clean.push(name);
    }
    // Belt and braces: the result must be plain relative components.
    clean
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
        .then_some(clean)
}

/// `dir/name`, or `dir/name (1).ext`, `(2)`… when that already exists.
pub fn unique_child(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if fs::symlink_metadata(&candidate).is_err() {
        return candidate;
    }
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){extension}")))
        .find(|path| fs::symlink_metadata(path).is_err())
        .expect("some number is free")
}

/// Where each entry of an accepted transfer goes: top-level names that are
/// taken get a number; everything below follows its top-level folder.
pub fn plan(
    manifest: &Manifest,
    root: &Path,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>), &'static str> {
    let mut renamed: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut targets = Vec::with_capacity(manifest.entries.len());
    let mut tops = Vec::new();
    for entry in &manifest.entries {
        let clean = sanitize(&entry.path).ok_or("unsafe file name")?;
        let mut parts = clean.components();
        let first = PathBuf::from(parts.next().ok_or("empty path")?.as_os_str());
        let rest: PathBuf = parts.collect();
        let top = match renamed.iter().find(|(from, _)| *from == first) {
            Some((_, to)) => to.clone(),
            None => {
                let to = unique_child(root, first.to_str().ok_or("unsupported name")?);
                renamed.push((first, to.clone()));
                tops.push(to.clone());
                to
            }
        };
        let target = if rest.as_os_str().is_empty() {
            top
        } else {
            top.join(rest)
        };
        if !target.starts_with(root) {
            return Err("path escapes the download directory");
        }
        targets.push(target);
    }
    Ok((targets, tops))
}

/// Receives one file: writes to a hidden `.partial` next to `target` and
/// renames it into place once exactly `size` bytes arrived.
pub struct FileWriter {
    target: PathBuf,
    partial: PathBuf,
    file: Option<fs::File>,
    size: u64,
    written: u64,
}

impl FileWriter {
    pub fn create(target: PathBuf, size: u64, salt: u64) -> io::Result<Self> {
        let parent = target.parent().map(Path::to_owned).unwrap_or_default();
        fs::create_dir_all(&parent)?;
        let mut name = OsString::from(".");
        name.push(target.file_name().unwrap_or_default());
        name.push(format!(".{salt:x}.partial"));
        let partial = parent.join(name);
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)?;
        Ok(Self {
            target,
            partial,
            file: Some(file),
            size,
            written: 0,
        })
    }

    /// Appends `data` at `offset`; `Ok(true)` once the file is complete and
    /// in place.
    pub fn write(&mut self, offset: u64, data: &[u8]) -> Result<bool, String> {
        if offset != self.written {
            return Err("file chunk out of order".to_owned());
        }
        if self.written + data.len() as u64 > self.size {
            return Err("file larger than announced".to_owned());
        }
        let file = self.file.as_mut().ok_or("file already finished")?;
        file.write_all(data).map_err(|error| error.to_string())?;
        self.written += data.len() as u64;
        if self.written < self.size {
            return Ok(false);
        }
        self.finish().map(|()| true)
    }

    /// Completes a file of the announced length (also an empty one).
    pub fn finish(&mut self) -> Result<(), String> {
        let file = self.file.take().ok_or("file already finished")?;
        let result = self.complete(file);
        if result.is_err() {
            let _ = fs::remove_file(&self.partial);
        }
        result
    }

    fn complete(&mut self, file: fs::File) -> Result<(), String> {
        if self.written != self.size {
            return Err("file shorter than announced".to_owned());
        }
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        // Something may have appeared at the target meanwhile: never overwrite.
        let target = if fs::symlink_metadata(&self.target).is_ok() {
            let parent = self.target.parent().unwrap_or(Path::new(""));
            let name = self
                .target
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file");
            unique_child(parent, name)
        } else {
            self.target.clone()
        };
        fs::rename(&self.partial, &target).map_err(|error| error.to_string())?;
        self.target = target;
        Ok(())
    }
}

impl Drop for FileWriter {
    fn drop(&mut self) {
        // Unfinished: the partial file goes.
        if self.file.take().is_some() {
            let _ = fs::remove_file(&self.partial);
        }
    }
}

/// Local paths from a `text/uri-list` (`file://` URLs, one per line; `#`
/// lines are comments).
pub fn parse_uri_list(data: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(data)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(file_url_to_path)
        .collect()
}

/// A `text/uri-list` of `paths`, `\r\n` separated as RFC 2483 wants.
pub fn uri_list(paths: &[PathBuf]) -> Vec<u8> {
    paths
        .iter()
        .map(|path| path_to_file_url(path))
        .collect::<Vec<_>>()
        .join("\r\n")
        .into_bytes()
}

fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    // Only local files: an empty host or "localhost".
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let decoded = percent_decode(rest)?;
    if cfg!(windows) {
        // file:///C:/Users/… → C:\Users\…
        let path = decoded.strip_prefix('/').unwrap_or(&decoded);
        Some(PathBuf::from(path.replace('/', "\\")))
    } else {
        Some(PathBuf::from(decoded))
    }
}

fn path_to_file_url(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = if text.starts_with('/') {
        text
    } else {
        format!("/{text}")
    };
    let mut url = String::from("file://");
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~:".contains(&byte) {
            url.push(byte as char);
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    url
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}
