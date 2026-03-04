// PE .idata section patcher.
// Fixes binutils 2.44+ regression: .idata marked read-only, but Wine's PE
// loader writes import addresses there. Patch to add IMAGE_SCN_MEM_WRITE.

use std::io::{Read, Write, Seek, SeekFrom};
use std::path::Path;

const IMAGE_SCN_MEM_WRITE: u32 = 0x8000_0000;

const PE_EXTENSIONS: &[&str] = &[
    ".dll", ".exe", ".sys", ".drv", ".cpl", ".acm", ".ax", ".ocx",
];

pub fn fix_idata_sections(pe_dir: &Path) -> std::io::Result<u32> {
    let mut count = 0u32;
    let entries = std::fs::read_dir(pe_dir)?;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !PE_EXTENSIONS.iter().any(|ext| name.ends_with(ext)) {
            continue;
        }
        if patch_pe(&entry.path())? {
            count += 1;
        }
    }
    Ok(count)
}

fn read_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn patch_pe(path: &Path) -> std::io::Result<bool> {
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(path)?;

    let mut header = [0u8; 1024];
    let n = f.read(&mut header)?;
    if n < 64 || header[0] != b'M' || header[1] != b'Z' {
        return Ok(false);
    }

    let e_lfanew = read_u32(&header, 0x3C) as usize;
    if e_lfanew + 24 > n {
        return Ok(false);
    }

    let coff_off = e_lfanew + 4;
    let num_sections = read_u16(&header, coff_off + 2) as usize;
    let opt_size = read_u16(&header, coff_off + 16) as usize;
    let sect_off = coff_off + 20 + opt_size;

    // Re-read if section headers extend past initial buffer
    let needed = sect_off + num_sections * 40;
    let header_slice = if needed > n {
        f.seek(SeekFrom::Start(0))?;
        let mut big = vec![0u8; needed];
        let got = f.read(&mut big)?;
        if got < needed {
            return Ok(false);
        }
        big
    } else {
        header[..n].to_vec()
    };

    for i in 0..num_sections {
        let s = sect_off + i * 40;
        if s + 40 > header_slice.len() {
            break;
        }
        // Section name is 8 bytes, null-padded
        let name_end = header_slice[s..s + 8]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(8);
        let name = &header_slice[s..s + name_end];
        if name == b".idata" {
            let flags = read_u32(&header_slice, s + 36);
            if flags & IMAGE_SCN_MEM_WRITE == 0 {
                let new_flags = (flags | IMAGE_SCN_MEM_WRITE).to_le_bytes();
                f.seek(SeekFrom::Start((s + 36) as u64))?;
                f.write_all(&new_flags)?;
                return Ok(true);
            }
        }
    }
    Ok(false)
}
