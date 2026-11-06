// SPDX-License-Identifier: MIT
//! Boot Loader Specification Type #1 entries on the XBOOTLDR partition.
//!
//! Finds the one XBOOTLDR partition in the GPT, reads its FAT file system,
//! parses `/loader/entries/*.conf`, checks every entry against the disk and
//! sorts them in BLS order (as systemd-boot does). Read-only.
#![deny(unsafe_op_in_unsafe_fn)]

use crate::c_size_t;
use crate::gpt;
use crate::println;
use crate::storage;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::ffi::{c_char, c_int, c_void, CStr};
use core::fmt::Write as _;
use fatfs::{FileSystem, FsOptions, Read, ReadWriteSeek, Seek, SeekFrom};
use uuid::{uuid, Uuid};

pub const XBOOTLDR: Uuid = uuid!("bc13c2ff-59e6-4262-a352-b275fd6f7172");
pub const ROOT_ARM64: Uuid = uuid!("b921b045-1df0-41c3-af44-4c6f280d3fae");

const ENTRIES_DIR: &str = "/loader/entries";
const MAX_ENTRIES: usize = 64;
const MAX_ENTRY_FILE: u64 = 64 * 1024;

pub struct Entry {
    /// File name without `.conf` and without a boot counter.
    pub id: String,
    /// Keys in file order; repeated keys appear repeatedly.
    pub keys: Vec<(String, String)>,
    /// Why the entry can't boot; empty if it can.
    pub problems: Vec<String>,
}

impl Entry {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.all(key).next()
    }

    pub fn all<'a, 'k>(&'a self, key: &'k str) -> impl Iterator<Item = &'a str> + use<'a, 'k> {
        self.keys
            .iter()
            .filter(move |(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// `options` lines, joined with spaces as BLS specifies.
    pub fn options(&self) -> String {
        let mut out = String::new();
        for value in self.all("options") {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(value);
        }
        out
    }

    /// The `root=PARTUUID=` of the options, if it is a valid UUID.
    pub fn root(&self) -> Option<Uuid> {
        self.options()
            .split_ascii_whitespace()
            .filter_map(|arg| arg.strip_prefix("root=PARTUUID="))
            .last()
            .and_then(|id| Uuid::try_parse(id).ok())
    }

    pub fn bootable(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Parses one entry file. Unknown keys are kept; they don't make it unbootable.
pub fn parse_entry(id: &str, text: &str) -> Entry {
    let mut keys = Vec::new();
    for line in text.lines() {
        let line = line.trim_matches([' ', '\t', '\r']);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once([' ', '\t'])
            .map_or((line, ""), |(k, v)| (k, v.trim_matches([' ', '\t'])));
        keys.push((key.to_string(), value.to_string()));
    }
    Entry {
        id: id.to_string(),
        keys,
        problems: Vec::new(),
    }
}

/// The entry id for a file name: `.conf` and a `+LEFT[-DONE]` boot counter removed.
pub fn entry_id(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".conf")?;
    if let Some((id, counter)) = stem.rsplit_once('+') {
        let (left, done) = counter.split_once('-').unwrap_or((counter, "0"));
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if digits(left) && digits(done) {
            return Some(id);
        }
    }
    Some(stem)
}

fn is_version_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"~-^.".contains(&c)
}

/// The UAPI version comparison (systemd's `strverscmp_improved`).
pub fn version_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    if a.is_empty() || b.is_empty() {
        return a.cmp(b);
    }
    let first = |s: &[u8]| s.first().copied().unwrap_or(0);
    // A string whose segment starts with `sep` is older than one whose doesn't.
    let separator = |a: &mut &[u8], b: &mut &[u8], sep: u8| -> Ordering {
        let (sa, sb) = (first(a) == sep, first(b) == sep);
        if sa != sb {
            return if sa {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        if sa {
            *a = &a[1..];
            *b = &b[1..];
        }
        Ordering::Equal
    };
    loop {
        while !a.is_empty() && !is_version_char(a[0]) {
            a = &a[1..];
        }
        while !b.is_empty() && !is_version_char(b[0]) {
            b = &b[1..];
        }
        // `~` sorts before everything, even the end of the string.
        let r = separator(&mut a, &mut b, b'~');
        if r.is_ne() {
            return r;
        }
        if a.is_empty() || b.is_empty() {
            return a.cmp(b);
        }
        for sep in [b'-', b'^', b'.'] {
            let r = separator(&mut a, &mut b, sep);
            if r.is_ne() {
                return r;
            }
        }
        let numeric = first(a).is_ascii_digit() || first(b).is_ascii_digit();
        let class = |c: &u8| {
            if numeric {
                c.is_ascii_digit()
            } else {
                c.is_ascii_alphabetic()
            }
        };
        let la = a.iter().take_while(|c| class(c)).count();
        let lb = b.iter().take_while(|c| class(c)).count();
        let (sa, sb) = (&a[..la], &b[..lb]);
        let r = if numeric {
            // A number is newer than no number; then compare without leading zeros.
            let za = sa.iter().take_while(|&&c| c == b'0').count();
            let zb = sb.iter().take_while(|&&c| c == b'0').count();
            (la != 0)
                .cmp(&(lb != 0))
                .then((la - za).cmp(&(lb - zb)))
                .then(sa[za..].cmp(&sb[zb..]))
        } else {
            let n = la.min(lb);
            sa[..n].cmp(&sb[..n]).then(la.cmp(&lb))
        };
        if r.is_ne() {
            return r;
        }
        a = &a[la..];
        b = &b[lb..];
    }
}

/// BLS order, as systemd-boot sorts: entries with a sort-key first, by
/// sort-key, machine-id and newest version; then everything by id, newest first.
pub fn entry_cmp(a: &Entry, b: &Entry) -> Ordering {
    let (ka, kb) = (a.get("sort-key"), b.get("sort-key"));
    let r = kb.is_some().cmp(&ka.is_some());
    if r.is_ne() {
        return r;
    }
    if ka.is_some() {
        let r = ka
            .cmp(&kb)
            .then(a.get("machine-id").cmp(&b.get("machine-id")))
            .then(version_cmp(
                b.get("version").unwrap_or(""),
                a.get("version").unwrap_or(""),
            ));
        if r.is_ne() {
            return r;
        }
    }
    version_cmp(&b.id, &a.id)
}

pub struct Listing {
    pub entries: Vec<Entry>,
}

fn fs_error<E>(path: &str, err: fatfs::Error<E>) -> String {
    match err {
        fatfs::Error::NotFound => format!("{}: missing", path),
        fatfs::Error::InvalidInput => format!("{}: invalid path", path),
        _ => format!("{}: read error", path),
    }
}

fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && path
            .split('/')
            .skip(1)
            .all(|p| !p.is_empty() && p != "." && p != "..")
}

/// Reads and checks every entry. `open(lba)` gives the disk from that block on;
/// `devicetree` is the DTB file name this board needs, if known.
pub fn read_entries<IO, F>(open: F, devicetree: Option<&str>) -> Result<Listing, String>
where
    IO: ReadWriteSeek,
    IO::Error: core::fmt::Debug,
    F: Fn(u64) -> IO,
{
    with_entries(open, devicetree, |_, entries| Ok(Listing { entries }))
}

/// Opens the boot partition, reads and checks its entries in BLS order and
/// hands them to `then` along with the partition's root directory.
fn with_entries<IO, F, R>(
    open: F,
    devicetree: Option<&str>,
    then: impl FnOnce(&Dir<IO>, Vec<Entry>) -> Result<R, String>,
) -> Result<R, String>
where
    IO: ReadWriteSeek,
    IO::Error: core::fmt::Debug,
    F: Fn(u64) -> IO,
{
    let mut table = gpt::GPT::new(open(0)).map_err(|_| "No GPT on this disk".to_string())?;
    let mut boot = None;
    let mut boots = 0;
    let mut roots = Vec::new();
    for i in 0..table.count().min(1024) {
        let part = table
            .index(i)
            .map_err(|_| "Partition table read error".to_string())?;
        let kind = part.get_type_guid();
        if kind == XBOOTLDR {
            boots += 1;
            boot = Some(part.get_starting_lba());
        } else if kind == ROOT_ARM64 {
            roots.push(part.get_partition_guid());
        }
    }
    let boot = match (boots, boot) {
        (1, Some(lba)) => lba,
        _ => {
            return Err(format!(
                "Expected exactly one XBOOTLDR boot partition, found {}",
                boots
            ))
        }
    };

    let opts = FsOptions::new().update_accessed_date(false);
    let fs = FileSystem::new(open(boot), opts)
        .map_err(|_| "Boot partition has no FAT file system".to_string())?;
    let root_dir = fs.root_dir();
    let dir = match root_dir.open_dir(ENTRIES_DIR) {
        Ok(dir) => dir,
        Err(fatfs::Error::NotFound) => return then(&root_dir, Vec::new()),
        Err(err) => return Err(fs_error(ENTRIES_DIR, err)),
    };

    let mut names = Vec::new();
    for item in dir.iter() {
        let item = item.map_err(|err| fs_error(ENTRIES_DIR, err))?;
        let name = item.file_name();
        if item.is_file() && !name.starts_with('.') && name.to_ascii_lowercase().ends_with(".conf")
        {
            names.push((name, item.len()));
        }
    }
    names.sort();

    let mut entries = Vec::new();
    for (name, size) in names.into_iter().take(MAX_ENTRIES) {
        // The suffix check is case-blind; the id keeps the name's case.
        let lower = name.to_ascii_lowercase();
        let id = &name[..entry_id(&lower).map_or(name.len() - 5, str::len)];
        let path = format!("{}/{}", ENTRIES_DIR, name);
        let text = if size > MAX_ENTRY_FILE {
            Err(format!("{}: too large", path))
        } else {
            read_file(&root_dir, &path, size as usize).and_then(|data| {
                String::from_utf8(data).map_err(|_| format!("{}: not UTF-8", path))
            })
        };
        let mut entry = match text {
            Ok(text) => parse_entry(id, &text),
            Err(problem) => {
                entries.push(Entry {
                    id: id.to_string(),
                    keys: Vec::new(),
                    problems: vec![problem],
                });
                continue;
            }
        };
        check(&mut entry, &root_dir, &roots, devicetree);
        entries.push(entry);
    }
    entries.sort_by(entry_cmp);
    then(&root_dir, entries)
}

/// An entry's files in memory, as `P_BOOT_START` needs them.
pub struct Loaded {
    /// The kernel as stored: gzip, or an Image with room for its `image_size`
    /// at a `KERNEL_ALIGN` boundary.
    pub linux: (*mut u8, usize),
    /// All initrds, concatenated in entry order.
    pub initrd: (*mut u8, usize),
    pub devicetree: (*mut u8, usize),
    pub options: String,
}

pub const KERNEL_ALIGN: usize = 2 << 20;

fn read_into<IO: ReadWriteSeek>(dir: &Dir<IO>, path: &str, dst: &mut [u8]) -> Result<(), String> {
    let mut file = dir.open_file(path).map_err(|e| fs_error(path, e))?;
    file.read_exact(dst)
        .map_err(|_| format!("{}: read error", path))
}

pub const SPLASH_MAGIC: &[u8; 4] = b"OSPL";

/// Reads the entry's splash (`splash` next to its kernel): `OSPL`, width and
/// height as little-endian u32, then the pixels as b, g, r, 0.
fn read_splash<IO: ReadWriteSeek>(
    dir: &Dir<IO>,
    linux: &str,
) -> Result<(Vec<u8>, u32, u32), String> {
    let path = format!("{}/splash", linux.rsplit_once('/').map_or("", |(d, _)| d));
    let (size, head) = probe(dir, &path)?;
    let width = u32::from_le_bytes(head[4..8].try_into().unwrap());
    let height = u32::from_le_bytes(head[8..12].try_into().unwrap());
    if &head[..4] != SPLASH_MAGIC || size != 12 + width as u64 * height as u64 * 4 {
        return Err(format!("{}: not a splash", path));
    }
    let mut data = vec![0; size as usize];
    read_into(dir, &path, &mut data)?;
    data.drain(..12);
    Ok((data, width, height))
}

/// Checks the entries again and loads the bootable entry `id`. `alloc(size,
/// align)` gives memory that stays valid after this returns. If `paint` is
/// given, the entry's splash goes to it before anything else is loaded; a
/// missing or broken splash only costs the picture.
pub fn load_entry<IO, F, A>(
    open: F,
    id: &str,
    devicetree: Option<&str>,
    mut alloc: A,
    paint: Option<&mut dyn FnMut(Result<(&[u8], u32, u32), String>)>,
) -> Result<Loaded, String>
where
    IO: ReadWriteSeek,
    IO::Error: core::fmt::Debug,
    F: Fn(u64) -> IO,
    A: FnMut(usize, usize) -> *mut u8,
{
    with_entries(open, devicetree, |dir, entries| {
        let entry = entries
            .iter()
            .find(|e| e.id == id)
            .ok_or_else(|| format!("No boot entry {}", id))?;
        if let Some(problem) = entry.problems.first() {
            return Err(format!("{} is not bootable: {}", id, problem));
        }
        let mut alloc_slice = |size: usize, align: usize| -> Result<&mut [u8], String> {
            let ptr = alloc(size.max(1), align);
            if ptr.is_null() {
                return Err(format!("Out of memory for {} bytes", size));
            }
            Ok(unsafe { core::slice::from_raw_parts_mut(ptr, size) })
        };

        let path = entry.get("linux").unwrap_or_default();
        if let Some(paint) = paint {
            match read_splash(dir, path) {
                Ok((pixels, width, height)) => paint(Ok((&pixels, width, height))),
                Err(problem) => paint(Err(problem)),
            }
        }
        let (size, head) = probe(dir, path)?;
        let size = size as usize;
        let linux = if head[..2] == [0x1f, 0x8b] {
            alloc_slice(size, 64)?
        } else {
            let image_size = u64::from_le_bytes(head[16..24].try_into().unwrap()) as usize;
            &mut alloc_slice(size.max(image_size), KERNEL_ALIGN)?[..size]
        };
        read_into(dir, path, linux)?;
        let linux = (linux.as_mut_ptr(), size);

        let mut initrds = Vec::new();
        for path in entry.all("initrd") {
            initrds.push((path, probe(dir, path)?.0 as usize));
        }
        let total = initrds.iter().map(|(_, size)| size).sum();
        let initrd = alloc_slice(total, 4096)?;
        let mut at = 0;
        for (path, size) in initrds {
            read_into(dir, path, &mut initrd[at..at + size])?;
            at += size;
        }
        let initrd = (initrd.as_mut_ptr(), total);

        let path = entry.get("devicetree").unwrap_or_default();
        let size = probe(dir, path)?.0 as usize;
        let tree = alloc_slice(size, 8)?;
        read_into(dir, path, tree)?;

        Ok(Loaded {
            linux,
            initrd,
            devicetree: (tree.as_mut_ptr(), size),
            options: entry.options(),
        })
    })
}

type Dir<'a, IO> = fatfs::Dir<'a, IO, fatfs::NullTimeProvider, fatfs::LossyOemCpConverter>;

fn read_file<IO: ReadWriteSeek>(dir: &Dir<IO>, path: &str, size: usize) -> Result<Vec<u8>, String> {
    let mut file = dir.open_file(path).map_err(|e| fs_error(path, e))?;
    let mut data = vec![0; size];
    file.read_exact(&mut data)
        .map_err(|_| format!("{}: read error", path))?;
    Ok(data)
}

/// Opens a file the entry names and returns its size and first bytes.
fn probe<IO: ReadWriteSeek>(dir: &Dir<IO>, path: &str) -> Result<(u64, [u8; 64]), String> {
    if !valid_path(path) {
        return Err(format!("{}: not an absolute path", path));
    }
    let mut file = dir.open_file(path).map_err(|e| fs_error(path, e))?;
    let size = file
        .seek(SeekFrom::End(0))
        .map_err(|_| format!("{}: read error", path))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|_| format!("{}: read error", path))?;
    let mut head = [0u8; 64];
    let n = (size as usize).min(head.len());
    file.read_exact(&mut head[..n])
        .map_err(|_| format!("{}: read error", path))?;
    Ok((size, head))
}

fn check<IO: ReadWriteSeek>(
    entry: &mut Entry,
    dir: &Dir<IO>,
    roots: &[Uuid],
    devicetree: Option<&str>,
) {
    let mut problems = Vec::new();
    for key in ["devicetree-overlay", "efi", "uki"] {
        if entry.get(key).is_some() {
            problems.push(format!("{} is not supported", key));
        }
    }
    if entry
        .get("architecture")
        .is_some_and(|a| !a.eq_ignore_ascii_case("aa64"))
    {
        problems.push("architecture is not aa64".to_string());
    }
    for key in ["linux", "devicetree"] {
        if entry.all(key).count() != 1 {
            problems.push(format!("needs exactly one {}", key));
        }
    }
    if let Some(path) = entry.get("linux") {
        match probe(dir, path) {
            Ok((_, head)) if head[..2] == [0x1f, 0x8b] || head[56..60] == *b"ARM\x64" => {}
            Ok(_) => problems.push(format!("{}: not an arm64 Image", path)),
            Err(problem) => problems.push(problem),
        }
    }
    if let Some(path) = entry.get("devicetree") {
        match probe(dir, path) {
            Ok((_, head)) if head[..4] == [0xd0, 0x0d, 0xfe, 0xed] => {}
            Ok(_) => problems.push(format!("{}: not a device tree", path)),
            Err(problem) => problems.push(problem),
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        if let Some(wanted) = devicetree {
            if !name.eq_ignore_ascii_case(wanted) {
                problems.push(format!("devicetree is not {}", wanted));
            }
        }
    }
    for path in entry.all("initrd") {
        if let Err(problem) = probe(dir, path) {
            problems.push(problem);
        }
    }
    match entry.root() {
        None => problems.push("options lack root=PARTUUID=".to_string()),
        Some(root) if !roots.contains(&root) => {
            problems.push("root=PARTUUID is not an arm64 root partition on this disk".to_string())
        }
        Some(_) => {}
    }
    entry.problems = problems;
}

/// The keys BLS defines; only these are passed on, so an unknown key in an
/// entry file can't pose as one of the lines `render` adds.
const BLS_KEYS: &[&str] = &[
    "title",
    "version",
    "machine-id",
    "sort-key",
    "linux",
    "initrd",
    "efi",
    "options",
    "devicetree",
    "devicetree-overlay",
    "architecture",
    "uki",
];

/// The version of the text `render` writes, its first line.
pub const CONTRACT: u32 = 1;

/// The listing as text in BLS syntax, for the host: a `contract N` line, then
/// per entry an `entry ID`
/// line, its BLS keys in file order, `root PARTUUID` if it has one and a
/// `problem TEXT` line per problem, then an empty line. If the disk itself
/// can't be read, a single `error TEXT` line.
pub fn render(listing: &Result<Listing, String>) -> String {
    let mut out = format!("contract {}\n", CONTRACT);
    match listing {
        Err(error) => {
            let _ = writeln!(out, "error {}", error);
        }
        Ok(listing) => {
            for entry in &listing.entries {
                let _ = writeln!(out, "entry {}", entry.id);
                for (key, value) in entry
                    .keys
                    .iter()
                    .filter(|(k, _)| BLS_KEYS.contains(&k.as_str()))
                {
                    let _ = writeln!(out, "{} {}", key, value);
                }
                if let Some(root) = entry.root() {
                    let _ = writeln!(out, "root {}", root);
                }
                for problem in &entry.problems {
                    let _ = writeln!(out, "problem {}", problem);
                }
                out.push('\n');
            }
        }
    }
    out
}

fn log(listing: &Result<Listing, String>) {
    match listing {
        Err(error) => println!("bls: {}", error),
        Ok(listing) => {
            println!("bls: {} entries", listing.entries.len());
            for (n, entry) in listing.entries.iter().enumerate() {
                let title = entry.get("title").unwrap_or(&entry.id);
                match entry.problems.first() {
                    None => println!("bls: {}. {} ({})", n + 1, title, entry.id),
                    Some(p) => println!(
                        "bls: {}. {} ({}): not bootable: {}",
                        n + 1,
                        title,
                        entry.id,
                        p
                    ),
                }
            }
        }
    }
}

/// Lists the boot entries of main storage, which the caller has initialised.
/// `devicetree` may be NULL. Returns the text of `render` (NUL-terminated,
/// `*size` without the NUL) or NULL; free it with `rust_free_boot_entries`.
#[no_mangle]
pub unsafe extern "C" fn rust_boot_entries(
    devicetree: *const c_char,
    size: *mut c_size_t,
) -> *mut c_void {
    let devicetree = match devicetree.is_null() {
        true => None,
        false => unsafe { CStr::from_ptr(devicetree) }.to_str().ok(),
    };
    let listing = read_entries(storage::MainStorage::new, devicetree);
    log(&listing);
    let mut text = render(&listing).into_bytes();
    let len = text.len();
    text.push(0);
    unsafe { *size = len };
    text.leak().as_mut_ptr() as *mut c_void
}

#[repr(C)]
pub struct BootFiles {
    kernel: *mut c_void,
    kernel_size: c_size_t,
    initrd: *mut c_void,
    initrd_size: c_size_t,
    devicetree: *mut c_void,
    devicetree_size: c_size_t,
    options: *mut c_char,
}

extern "C" {
    fn heapblock_alloc_aligned(size: c_size_t, align: c_size_t) -> *mut c_void;
    fn bootmgr_paint_splash(pixels: *const c_void, width: u32, height: u32);
}

/// Loads the bootable entry `id` of main storage, which the caller has
/// initialised, into heapblock memory, after painting its splash with
/// `bootmgr_paint_splash` if `splash` is set. `devicetree` may be NULL.
/// Returns 0, or -1 with the reason on the console.
#[no_mangle]
pub unsafe extern "C" fn rust_boot_entry_load(
    id: *const c_char,
    devicetree: *const c_char,
    splash: bool,
    files: *mut BootFiles,
) -> c_int {
    let Ok(id) = unsafe { CStr::from_ptr(id) }.to_str() else {
        println!("bls: the entry id is not UTF-8");
        return -1;
    };
    let devicetree = match devicetree.is_null() {
        true => None,
        false => unsafe { CStr::from_ptr(devicetree) }.to_str().ok(),
    };
    let alloc = |size, align| unsafe { heapblock_alloc_aligned(size, align) as *mut u8 };
    let mut paint = |splash: Result<(&[u8], u32, u32), String>| match splash {
        Ok((pixels, width, height)) => unsafe {
            bootmgr_paint_splash(pixels.as_ptr() as *const c_void, width, height)
        },
        Err(problem) => println!("bls: no splash: {}", problem),
    };
    let paint: Option<&mut dyn FnMut(Result<(&[u8], u32, u32), String>)> =
        if splash { Some(&mut paint) } else { None };
    match load_entry(storage::MainStorage::new, id, devicetree, alloc, paint) {
        Err(error) => {
            println!("bls: {}", error);
            -1
        }
        Ok(loaded) => {
            println!(
                "bls: loaded {}: linux {} bytes, initrd {} bytes, devicetree {} bytes",
                id, loaded.linux.1, loaded.initrd.1, loaded.devicetree.1
            );
            let mut options = loaded.options.into_bytes();
            options.retain(|&b| b != 0);
            options.push(0);
            unsafe {
                *files = BootFiles {
                    kernel: loaded.linux.0 as *mut c_void,
                    kernel_size: loaded.linux.1,
                    initrd: loaded.initrd.0 as *mut c_void,
                    initrd_size: loaded.initrd.1,
                    devicetree: loaded.devicetree.0 as *mut c_void,
                    devicetree_size: loaded.devicetree.1,
                    options: options.leak().as_mut_ptr() as *mut c_char,
                };
            }
            0
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn rust_free_boot_entries(data: *mut c_void, size: c_size_t) {
    if !data.is_null() {
        drop(unsafe { Vec::from_raw_parts(data as *mut u8, size + 1, size + 1) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::rc::Rc;
    use core::cell::RefCell;
    use fatfs::{FatType, FormatVolumeOptions, Write};

    const BLOCK: u64 = 4096;
    const BOOT_LBA: u64 = 16;
    const BOOT_BLOCKS: u64 = 10240; // 40 MiB: FAT32 with 512-byte clusters
    const ROOT: &str = "4d6c1a2b-0000-4000-8000-000000000001";

    #[derive(Clone)]
    struct Mem {
        disk: Rc<RefCell<Vec<u8>>>,
        base: u64,
        pos: u64,
    }

    impl fatfs::IoBase for Mem {
        type Error = ();
    }
    impl fatfs::Read for Mem {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, ()> {
            let disk = self.disk.borrow();
            let at = (self.base + self.pos) as usize;
            let n = buf.len().min(disk.len().saturating_sub(at));
            buf[..n].copy_from_slice(&disk[at..at + n]);
            self.pos += n as u64;
            Ok(n)
        }
    }
    impl fatfs::Write for Mem {
        fn write(&mut self, buf: &[u8]) -> Result<usize, ()> {
            let mut disk = self.disk.borrow_mut();
            let at = (self.base + self.pos) as usize;
            let n = buf.len().min(disk.len().saturating_sub(at));
            disk[at..at + n].copy_from_slice(&buf[..n]);
            self.pos += n as u64;
            Ok(n)
        }
        fn flush(&mut self) -> Result<(), ()> {
            Ok(())
        }
    }
    impl fatfs::Seek for Mem {
        fn seek(&mut self, from: SeekFrom) -> Result<u64, ()> {
            self.pos = match from {
                SeekFrom::Start(n) => n,
                SeekFrom::Current(n) => self.pos.checked_add_signed(n).ok_or(())?,
                SeekFrom::End(_) => return Err(()),
            };
            Ok(self.pos)
        }
    }

    fn gpt_entry(disk: &mut [u8], index: usize, kind: Uuid, id: Uuid, first: u64, last: u64) {
        let at = 2 * BLOCK as usize + index * 128;
        disk[at..at + 16].copy_from_slice(&kind.to_bytes_le());
        disk[at + 16..at + 32].copy_from_slice(&id.to_bytes_le());
        disk[at + 32..at + 40].copy_from_slice(&first.to_le_bytes());
        disk[at + 40..at + 48].copy_from_slice(&last.to_le_bytes());
    }

    /// A disk with a GPT (XBOOTLDR + one arm64 root), a FAT32 boot partition and `files`.
    fn disk(files: &[(&str, &[u8])], extra_boot: bool) -> Rc<RefCell<Vec<u8>>> {
        let blocks = BOOT_LBA + BOOT_BLOCKS + 16;
        let mut raw = vec![0u8; (blocks * BLOCK) as usize];
        let hdr = BLOCK as usize;
        raw[hdr..hdr + 8].copy_from_slice(b"EFI PART");
        raw[hdr + 24..hdr + 32].copy_from_slice(&1u64.to_le_bytes());
        raw[hdr + 72..hdr + 80].copy_from_slice(&2u64.to_le_bytes());
        raw[hdr + 80..hdr + 84].copy_from_slice(&128u32.to_le_bytes());
        raw[hdr + 84..hdr + 88].copy_from_slice(&128u32.to_le_bytes());
        let boot_id = uuid!("11111111-2222-4333-8444-555555555555");
        gpt_entry(
            &mut raw,
            0,
            XBOOTLDR,
            boot_id,
            BOOT_LBA,
            BOOT_LBA + BOOT_BLOCKS - 1,
        );
        let root_lba = BOOT_LBA + BOOT_BLOCKS;
        gpt_entry(
            &mut raw,
            1,
            ROOT_ARM64,
            Uuid::parse_str(ROOT).unwrap(),
            root_lba,
            root_lba + 15,
        );
        if extra_boot {
            gpt_entry(&mut raw, 2, XBOOTLDR, boot_id, root_lba, root_lba + 15);
        }
        let disk = Rc::new(RefCell::new(raw));
        let part = Mem {
            disk: disk.clone(),
            base: BOOT_LBA * BLOCK,
            pos: 0,
        };
        {
            let mut fmt = part.clone();
            // fatfs formats into a slice of the disk; bound it to the partition.
            let opts = FormatVolumeOptions::new()
                .fat_type(FatType::Fat32)
                .bytes_per_cluster(512)
                .total_sectors((BOOT_BLOCKS * BLOCK / 512) as u32);
            fatfs::format_volume(&mut fmt, opts).unwrap();
        }
        let fs = FileSystem::new(part, FsOptions::new()).unwrap();
        for (path, data) in files {
            let mut dir = fs.root_dir();
            let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
            for name in &parts[..parts.len() - 1] {
                dir = match dir.open_dir(name) {
                    Ok(d) => d,
                    Err(_) => dir.create_dir(name).unwrap(),
                };
            }
            let mut file = dir.create_file(parts[parts.len() - 1]).unwrap();
            file.write_all(data).unwrap();
        }
        drop(fs);
        disk
    }

    fn list(disk: &Rc<RefCell<Vec<u8>>>, dt: Option<&str>) -> Result<Listing, String> {
        read_entries(
            |lba| Mem {
                disk: disk.clone(),
                base: lba * BLOCK,
                pos: 0,
            },
            dt,
        )
    }

    fn image() -> Vec<u8> {
        let mut img = vec![0u8; 128];
        img[56..60].copy_from_slice(b"ARM\x64");
        img
    }

    const DTB: &[u8] = &[0xd0, 0x0d, 0xfe, 0xed, 0, 0, 0, 0];

    fn entry_text(title: &str, version: &str) -> String {
        format!(
            "# Orchard\ntitle {}\nversion {}\nsort-key omarchy\nlinux /tok/{v}/linux\n\
             initrd /tok/{v}/initrd\ndevicetree /tok/{v}/t8010-j71b.dtb\n\
             options root=PARTUUID={} rw\noptions quiet\n",
            title,
            version,
            ROOT,
            v = version
        )
    }

    #[test]
    fn versions() {
        use Ordering::*;
        let cases = [
            ("6.18.2", "6.18.10", Less),
            ("6.18", "6.18.0", Less),
            ("1.0~rc1", "1.0", Less),
            ("1.0", "1.0^1", Less),
            ("1.0-1", "1.0.1", Less),
            ("007", "7", Equal),
            ("a", "1", Less),
            ("abc", "abcd", Less),
            ("6.18.2-orchard", "6.18.2-orchard", Equal),
            ("", "1", Less),
        ];
        for (a, b, want) in cases {
            assert_eq!(version_cmp(a, b), want, "{} vs {}", a, b);
            assert_eq!(version_cmp(b, a), want.reverse(), "{} vs {}", b, a);
        }
    }

    #[test]
    fn ids_and_parsing() {
        assert_eq!(entry_id("omarchy-6.18.conf"), Some("omarchy-6.18"));
        assert_eq!(entry_id("omarchy+3-1.conf"), Some("omarchy"));
        assert_eq!(entry_id("omarchy+3.conf"), Some("omarchy"));
        assert_eq!(entry_id("a+b.conf"), Some("a+b"));
        assert_eq!(entry_id("x.txt"), None);
        let e = parse_entry(
            "x",
            "title\tHello  World \r\n  options a=1\noptions b\nempty\n",
        );
        assert_eq!(e.get("title"), Some("Hello  World"));
        assert_eq!(e.options(), "a=1 b");
        assert_eq!(e.get("empty"), Some(""));
    }

    #[test]
    fn lists_and_checks() {
        let img = image();
        let a = entry_text("Omarchy", "6.18.2-orchard");
        let b = entry_text("Omarchy", "6.18.10-orchard");
        let broken = format!("title Broken\nproblem none\nroot x\nlinux /tok/6.18.2-orchard/initrd\nlinux /x\noptions root=PARTUUID=00000000-0000-4000-8000-000000000000\nefi /e\n");
        let files: Vec<(&str, &[u8])> = vec![
            (
                "/loader/entries/omarchy-4d6c1a2b-6.18.2-orchard.conf",
                a.as_bytes(),
            ),
            (
                "/loader/entries/omarchy-4d6c1a2b-6.18.10-orchard+2-1.conf",
                b.as_bytes(),
            ),
            ("/loader/entries/zz-broken.conf", broken.as_bytes()),
            ("/loader/entries/notes.txt", b"ignored"),
            ("/tok/6.18.2-orchard/linux", &img),
            ("/tok/6.18.2-orchard/initrd", b"070701"),
            ("/tok/6.18.2-orchard/t8010-j71b.dtb", DTB),
        ];
        let d = disk(&files, false);
        let listing = list(&d, Some("t8010-j71b.dtb")).unwrap();
        let ids: Vec<&str> = listing.entries.iter().map(|e| e.id.as_str()).collect();
        // sort-key entries first, newest version first; then the rest by id.
        assert_eq!(
            ids,
            [
                "omarchy-4d6c1a2b-6.18.10-orchard",
                "omarchy-4d6c1a2b-6.18.2-orchard",
                "zz-broken"
            ]
        );
        let [newer, good, broken] = &listing.entries[..] else {
            panic!()
        };
        assert!(good.bootable(), "{:?}", good.problems);
        assert_eq!(good.root(), Uuid::try_parse(ROOT).ok());
        assert_eq!(good.options(), format!("root=PARTUUID={} rw quiet", ROOT));
        assert!(
            newer.problems.iter().all(|p| p.ends_with(": missing")),
            "{:?}",
            newer.problems
        );
        assert_eq!(newer.problems.len(), 3);
        assert_eq!(
            broken.problems,
            [
                "efi is not supported",
                "needs exactly one linux",
                "needs exactly one devicetree",
                "/tok/6.18.2-orchard/initrd: not an arm64 Image",
                "root=PARTUUID is not an arm64 root partition on this disk",
            ]
        );

        let other = list(&d, Some("t8015-d22.dtb")).unwrap();
        let good = other
            .entries
            .iter()
            .find(|e| e.id.ends_with("6.18.2-orchard"))
            .unwrap();
        assert_eq!(good.problems, ["devicetree is not t8015-d22.dtb"]);

        let text = render(&Ok(listing));
        assert!(
            text.starts_with("contract 1\nentry omarchy-4d6c1a2b-6.18.10-orchard\ntitle Omarchy\n")
        );
        assert!(text.contains(&format!("\nroot {}\n", ROOT)));
        assert!(text.contains("\nproblem efi is not supported\n"));
        assert!(!text.contains("problem none") && !text.contains("\nroot x\n"));
        assert!(text.ends_with("\n\n"));
    }

    #[test]
    fn loads_an_entry() {
        let mut img = image();
        img[16..24].copy_from_slice(&4096u64.to_le_bytes()); // image_size beyond the file
        let a = entry_text("Omarchy", "6.18.2-orchard").replace(
            "initrd /tok/6.18.2-orchard/initrd\n",
            "initrd /tok/6.18.2-orchard/initrd\ninitrd /tok/6.18.2-orchard/extra\n",
        );
        let files: Vec<(&str, &[u8])> = vec![
            ("/loader/entries/a.conf", a.as_bytes()),
            ("/loader/entries/bad.conf", b"title Bad\n"),
            ("/tok/6.18.2-orchard/linux", &img),
            ("/tok/6.18.2-orchard/initrd", b"070701"),
            ("/tok/6.18.2-orchard/extra", b"XYZ"),
            (
                "/tok/6.18.2-orchard/splash",
                b"OSPL\x02\0\0\0\x01\0\0\0\x01\x02\x03\0\x04\x05\x06\0",
            ),
            ("/tok/6.18.2-orchard/t8010-j71b.dtb", DTB),
        ];
        let d = disk(&files, false);
        let mut allocs = Vec::new();
        let alloc = |size: usize, align: usize| {
            let block = vec![0u8; size + align].leak();
            let at = block.as_ptr().align_offset(align);
            allocs.push((size, align));
            block[at..].as_mut_ptr()
        };
        let open = |lba| Mem {
            disk: d.clone(),
            base: lba * BLOCK,
            pos: 0,
        };
        let mut painted = Vec::new();
        let mut paint = |s: Result<(&[u8], u32, u32), String>| {
            let (p, w, h) = s.unwrap();
            painted.push((p.to_vec(), w, h))
        };
        let loaded =
            load_entry(open, "a", Some("t8010-j71b.dtb"), alloc, Some(&mut paint)).unwrap();
        assert_eq!(painted, [(vec![1, 2, 3, 0, 4, 5, 6, 0], 2, 1)]);
        let bytes =
            |(p, n): (*mut u8, usize)| unsafe { core::slice::from_raw_parts(p, n) }.to_vec();
        assert_eq!(bytes(loaded.linux), img);
        assert_eq!(loaded.linux.0 as usize % KERNEL_ALIGN, 0);
        assert_eq!(bytes(loaded.initrd), b"070701XYZ");
        assert_eq!(bytes(loaded.devicetree), DTB);
        assert_eq!(loaded.options, format!("root=PARTUUID={} rw quiet", ROOT));
        assert_eq!(allocs[0], (4096, KERNEL_ALIGN));

        let open = |lba| Mem {
            disk: d.clone(),
            base: lba * BLOCK,
            pos: 0,
        };
        let err = load_entry(open, "bad", None, |_, _| core::ptr::null_mut(), None)
            .err()
            .unwrap();
        assert!(
            err.starts_with("bad is not bootable: needs exactly one linux"),
            "{}",
            err
        );
        let open = |lba| Mem {
            disk: d.clone(),
            base: lba * BLOCK,
            pos: 0,
        };
        let err = load_entry(open, "nope", None, |_, _| core::ptr::null_mut(), None)
            .err()
            .unwrap();
        assert_eq!(err, "No boot entry nope");
    }

    #[test]
    fn disk_errors() {
        let empty = disk(&[], false);
        assert!(list(&empty, None).unwrap().entries.is_empty());
        let two = disk(&[], true);
        let err = list(&two, None).err().unwrap();
        assert_eq!(err, "Expected exactly one XBOOTLDR boot partition, found 2");
        assert_eq!(
            render(&Err(err)),
            "contract 1\nerror Expected exactly one XBOOTLDR boot partition, found 2\n"
        );
        let blank = Rc::new(RefCell::new(vec![0u8; 64 * 1024]));
        assert_eq!(list(&blank, None).err().unwrap(), "No GPT on this disk");
    }
}
