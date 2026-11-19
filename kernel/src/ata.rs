//! ATA PIO disk driver (primary slave, QEMU IDE disk).
//!
//! The boot disk is the primary master; the Rocky OS data disk is
//! attached as the second drive (`-drive ...,if=ide,index=1`), i.e.
//! the primary slave on the same 0x1F0 channel. PIO polling only,
//! no DMA, no interrupts: 512-byte sectors, LBA28.

use x86_64::instructions::port::Port;

const DATA: u16 = 0x1F0;
const SECTOR_COUNT: u16 = 0x1F2;
const LBA_LO: u16 = 0x1F3;
const LBA_MID: u16 = 0x1F4;
const LBA_HI: u16 = 0x1F5;
const DRIVE: u16 = 0x1F6;
const STATUS: u16 = 0x1F7;
const COMMAND: u16 = 0x1F7;
/// Primary slave, LBA mode: 0xF0 (bit 6 = LBA, bit 4 = slave;
/// 0xE0 would be the LBA master). 0xB0 is the CHS slave and
/// would make the drive misread the LBA registers.
const DRIVE_SLAVE: u8 = 0xF0;

const STATUS_BSY: u8 = 0x80;
const STATUS_DRQ: u8 = 0x08;
const STATUS_ERR: u8 = 0x01;

pub const SECTOR_SIZE: usize = 512;

fn status() -> u8 {
    unsafe { Port::new(STATUS).read() }
}

fn wait_not_busy() -> Result<(), &'static str> {
    for _ in 0..1_000_000 {
        let s = status();
        if s == 0xFF {
            return Err("no ATA drive present");
        }
        if s & STATUS_BSY == 0 {
            return Ok(());
        }
    }
    Err("ATA timeout waiting for BSY clear")
}

fn wait_drq() -> Result<(), &'static str> {
    for _ in 0..1_000_000 {
        let s = status();
        if s & STATUS_ERR != 0 {
            return Err("ATA error");
        }
        if s & STATUS_BSY == 0 && s & STATUS_DRQ != 0 {
            return Ok(());
        }
    }
    Err("ATA timeout waiting for DRQ")
}

/// True if a drive answers on the primary slave slot.
pub fn present() -> bool {
    // Selecting the drive and reading status: 0xFF means nothing there.
    unsafe {
        let mut p: Port<u8> = Port::new(DRIVE);
        p.write(DRIVE_SLAVE);
    }
    // Small delay via status reads.
    for _ in 0..1000 {
        let _ = status();
    }
    let s = status();
    s != 0xFF && s != 0
}

fn select(lba: u32) -> Result<(), &'static str> {
    wait_not_busy()?;
    unsafe {
        let mut p: Port<u8> = Port::new(DRIVE);
        p.write(DRIVE_SLAVE | (((lba >> 24) & 0x0F) as u8));
        let mut c: Port<u8> = Port::new(SECTOR_COUNT);
        c.write(1);
        let mut lo: Port<u8> = Port::new(LBA_LO);
        lo.write((lba & 0xFF) as u8);
        let mut mid: Port<u8> = Port::new(LBA_MID);
        mid.write(((lba >> 8) & 0xFF) as u8);
        let mut hi: Port<u8> = Port::new(LBA_HI);
        hi.write(((lba >> 16) & 0xFF) as u8);
    }
    Ok(())
}

pub fn read_sector(lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> Result<(), &'static str> {
    // PIO polling and the interrupt handlers share the CPU stack
    // and the port registers; an interrupt in the middle of a
    // multi-step ATA command sequence can corrupt it, so run the
    // whole transfer with interrupts disabled.
    x86_64::instructions::interrupts::without_interrupts(|| read_sector_inner(lba, buf))
}

fn read_sector_inner(lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> Result<(), &'static str> {
    select(lba)?;
    unsafe {
        let mut cmd: Port<u8> = Port::new(COMMAND);
        cmd.write(0x20); // READ SECTORS
    }
    wait_drq()?;
    unsafe {
        let mut data: Port<u16> = Port::new(DATA);
        for i in 0..256 {
            let w: u16 = data.read();
            buf[i * 2] = (w & 0xFF) as u8;
            buf[i * 2 + 1] = (w >> 8) as u8;
        }
    }
    // Consume the DRDY wait so a following command is clean.
    let _ = wait_not_busy();
    Ok(())
}

pub fn write_sector(lba: u32, buf: &[u8; SECTOR_SIZE]) -> Result<(), &'static str> {
    x86_64::instructions::interrupts::without_interrupts(|| write_sector_inner(lba, buf))
}

fn write_sector_inner(lba: u32, buf: &[u8; SECTOR_SIZE]) -> Result<(), &'static str> {
    select(lba)?;
    unsafe {
        let mut cmd: Port<u8> = Port::new(COMMAND);
        cmd.write(0x30); // WRITE SECTORS
    }
    wait_drq()?;
    unsafe {
        let mut data: Port<u16> = Port::new(DATA);
        for i in 0..256 {
            let w = (buf[i * 2] as u16) | ((buf[i * 2 + 1] as u16) << 8);
            data.write(w);
        }
        let mut flush: Port<u8> = Port::new(COMMAND);
        flush.write(0xE7); // FLUSH CACHE
    }
    wait_not_busy()?;
    Ok(())
}
