//! RockyFS — a tiny flat filesystem on the ATA data disk.
//!
//! Layout (512-byte sectors, 8 MiB disk = 16384 sectors):
//! ```text
//! sector 0        superblock: magic "ROCKYFS1", version, total
//!                 sectors, directory sectors, data start sector
//! sectors 1..17   directory: 128 entries x 64 bytes
//! sectors 17..    file data, allocated contiguously (bump pointer)
//! ```
//! Entry (64 bytes): name[32], size u32, start_sector u32, used u8.
//! Deleted files are marked unused; their sectors are not reused
//! (a toy-FS limitation, documented in the README). Files are
//! written whole: a rewrite that does not fit in place allocates
//! a fresh region.
//!
//! If the disk has no valid superblock on first boot it is formatted
//! (with a `readme.txt` and a `hello.txt` already on it). If no disk
//! is present at all the filesystem is disabled and the shell still
//! works.

use crate::ata::{self, SECTOR_SIZE};
use alloc::string::String;
use alloc::vec::Vec;
use spin::Mutex;

pub const MAGIC: &[u8; 8] = b"ROCKYFS1";
pub const DIR_SECTORS: u32 = 16;
pub const DATA_START: u32 = 1 + DIR_SECTORS;

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub name: String,
    pub size: u32,
    pub start_sector: u32,
}

struct FsState {
    available: bool,
    next_free_sector: u32,
    total_sectors: u32,
}

static FS: Mutex<FsState> = Mutex::new(FsState {
    available: false,
    next_free_sector: DATA_START,
    total_sectors: 0,
});

fn read_dir() -> Vec<Option<FileEntry>> {
    let mut entries = Vec::new();
    for sector in 0..DIR_SECTORS {
        let mut buf = [0u8; SECTOR_SIZE];
        if ata::read_sector(1 + sector, &mut buf).is_err() {
            break;
        }
        for i in 0..8 {
            let off = i * 64;
            let used = buf[off + 40];
            if used == 0 {
                entries.push(None);
                continue;
            }
            let name_len = buf[off..off + 32]
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(32);
            let name = String::from_utf8_lossy(&buf[off..off + name_len]).into_owned();
            let size = u32::from_le_bytes([
                buf[off + 32],
                buf[off + 33],
                buf[off + 34],
                buf[off + 35],
            ]);
            let start = u32::from_le_bytes([
                buf[off + 36],
                buf[off + 37],
                buf[off + 38],
                buf[off + 39],
            ]);
            entries.push(Some(FileEntry {
                name,
                size,
                start_sector: start,
            }));
        }
    }
    entries
}

fn write_dir_entry(index: usize, entry: &Option<FileEntry>) -> Result<(), &'static str> {
    let sector = 1 + (index / 8) as u32;
    let slot = index % 8;
    let mut buf = [0u8; SECTOR_SIZE];
    ata::read_sector(sector, &mut buf)?;
    let off = slot * 64;
    for b in buf[off..off + 64].iter_mut() {
        *b = 0;
    }
    if let Some(e) = entry {
        let name = e.name.as_bytes();
        let n = name.len().min(32);
        buf[off..off + n].copy_from_slice(&name[..n]);
        buf[off + 32..off + 36].copy_from_slice(&e.size.to_le_bytes());
        buf[off + 36..off + 40].copy_from_slice(&e.start_sector.to_le_bytes());
        buf[off + 40] = 1;
    }
    ata::write_sector(sector, &buf)
}

fn alloc_sectors(count: u32) -> Result<u32, &'static str> {
    let mut fs = FS.lock();
    if !fs.available {
        return Err("no disk");
    }
    if fs.next_free_sector + count > fs.total_sectors {
        return Err("disk full");
    }
    let start = fs.next_free_sector;
    fs.next_free_sector += count;
    Ok(start)
}

fn write_data(start: u32, data: &[u8]) -> Result<(), &'static str> {
    let sectors = (data.len() as u32 + 511) / 512;
    for i in 0..sectors {
        let mut buf = [0u8; SECTOR_SIZE];
        let off = (i as usize) * SECTOR_SIZE;
        let end = (off + SECTOR_SIZE).min(data.len());
        buf[..end - off].copy_from_slice(&data[off..end]);
        ata::write_sector(start + i, &buf)?;
    }
    Ok(())
}

fn read_data(entry: &FileEntry) -> Result<Vec<u8>, &'static str> {
    let sectors = (entry.size + 511) / 512;
    let mut out = Vec::new();
    for i in 0..sectors {
        let mut buf = [0u8; SECTOR_SIZE];
        ata::read_sector(entry.start_sector + i, &mut buf)?;
        let off = (i as usize) * SECTOR_SIZE;
        let end = (off + SECTOR_SIZE).min(entry.size as usize);
        out.extend_from_slice(&buf[..end - off]);
    }
    Ok(out)
}

fn format_disk() -> Result<(), &'static str> {
    // Superblock.
    let mut sb = [0u8; SECTOR_SIZE];
    sb[0..8].copy_from_slice(MAGIC);
    sb[8..12].copy_from_slice(&1u32.to_le_bytes());
    sb[12..16].copy_from_slice(&16384u32.to_le_bytes());
    sb[16..20].copy_from_slice(&DIR_SECTORS.to_le_bytes());
    sb[20..24].copy_from_slice(&DATA_START.to_le_bytes());
    ata::write_sector(0, &sb)?;
    // Zero the directory.
    let zero = [0u8; SECTOR_SIZE];
    for s in 1..1 + DIR_SECTORS {
        ata::write_sector(s, &zero)?;
    }
    {
        let mut fs = FS.lock();
        fs.available = true;
        fs.total_sectors = 16384;
        fs.next_free_sector = DATA_START;
    }
    // Default files (ignore errors: formatting must not fail the boot).
    let _ = write_file(
        "readme.txt",
        b"Welcome to Rocky OS!\nThis file lives on the RockyFS disk.\nTry: ls, cat readme.txt, write notes.txt Hello Rocky\n",
    );
    let _ = write_file("hello.txt", b"Hello from the Rocky OS filesystem!\n");
    Ok(())
}

/// Initialise the filesystem. Returns Ok(true) if a disk with a
/// filesystem is available, Ok(false) if there is no disk (the
/// kernel keeps booting), Err on a disk error.
pub fn init() -> Result<bool, &'static str> {
    if !ata::present() {
        return Ok(false);
    }
    let mut sb = [0u8; SECTOR_SIZE];
    ata::read_sector(0, &mut sb)?;
    if &sb[0..8] != MAGIC {
        format_disk()?;
        crate::sprintln!("fs: formatted new RockyFS disk (8 MiB)");
        crate::fb_println!("fs: formatted new RockyFS disk (8 MiB)");
        return Ok(true);
    }
    let total = u32::from_le_bytes([sb[12], sb[13], sb[14], sb[15]]);
    // Recompute the bump pointer from the directory so files
    // written on a previous boot are not overwritten.
    let mut next = DATA_START;
    for e in read_dir().into_iter().flatten() {
        let end = e.start_sector + (e.size + 511) / 512;
        if end > next {
            next = end;
        }
    }
    {
        let mut fs = FS.lock();
        fs.available = true;
        fs.total_sectors = total;
        fs.next_free_sector = next;
    }
    crate::sprintln!("fs: RockyFS disk ready, {} files", list().len());
    crate::fb_println!("fs: RockyFS disk ready");
    Ok(true)
}

pub fn available() -> bool {
    FS.lock().available
}

pub fn list() -> Vec<FileEntry> {
    if !available() {
        return Vec::new();
    }
    read_dir().into_iter().flatten().collect()
}

pub fn read_file(name: &str) -> Result<Vec<u8>, &'static str> {
    if !available() {
        return Err("no disk");
    }
    for e in read_dir().into_iter().flatten() {
        if e.name == name {
            return read_data(&e);
        }
    }
    Err("file not found")
}

pub fn write_file(name: &str, data: &[u8]) -> Result<(), &'static str> {
    if !available() {
        return Err("no disk");
    }
    if name.is_empty() || name.len() > 32 {
        return Err("bad file name");
    }
    let entries = read_dir();
    // Rewrite in place if it fits, else allocate fresh space.
    for (i, e) in entries.iter().enumerate() {
        if let Some(e) = e {
            if e.name == name {
                let sectors = (data.len() as u32 + 511) / 512;
                let old_sectors = (e.size + 511) / 512;
                if sectors <= old_sectors {
                    write_data(e.start_sector, data)?;
                    let new_entry = FileEntry {
                        name: name.into(),
                        size: data.len() as u32,
                        start_sector: e.start_sector,
                    };
                    return write_dir_entry(i, &Some(new_entry));
                } else {
                    let start = alloc_sectors(sectors)?;
                    write_data(start, data)?;
                    let new_entry = FileEntry {
                        name: name.into(),
                        size: data.len() as u32,
                        start_sector: start,
                    };
                    return write_dir_entry(i, &Some(new_entry));
                }
            }
        }
    }
    // New file: first free directory slot.
    for (i, e) in entries.iter().enumerate() {
        if e.is_none() {
            let sectors = (data.len() as u32 + 511) / 512;
            let start = alloc_sectors(sectors)?;
            write_data(start, data)?;
            let new_entry = FileEntry {
                name: name.into(),
                size: data.len() as u32,
                start_sector: start,
            };
            return write_dir_entry(i, &Some(new_entry));
        }
    }
    Err("directory full")
}

pub fn delete_file(name: &str) -> Result<(), &'static str> {
    if !available() {
        return Err("no disk");
    }
    let entries = read_dir();
    for (i, e) in entries.iter().enumerate() {
        if let Some(e) = e {
            if e.name == name {
                return write_dir_entry(i, &None);
            }
        }
    }
    Err("file not found")
}
