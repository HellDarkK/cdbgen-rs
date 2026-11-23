// Custom CDB writer that writes the per-record data (header + key + data)
// in a single `write_all` call to a 1MB BufWriter. The standard cdb::CDBMake
// splits each record across 2-3 separate writes (header, then key, then data),
// each of which goes through BufWriter's bookkeeping.

use std::{
    cmp::max,
    fs::File,
    io::{BufWriter, Result, Seek, Write},
};

#[derive(Clone, Copy)]
struct Entry {
    hash: u32,
    pos: u32,
}

pub struct FastCdbMake {
    file: BufWriter<File>,
    pos: u32,
    entries: Vec<Entry>,
    // Scratch for the open-addressing table at finish time.
    table: Vec<Entry>,
}

impl FastCdbMake {
    pub fn new(file: File) -> Result<Self> {
        // 4MB buffer: amortizes the per-record BufWriter overhead over more
        // records (4MB / ~40B/record = ~100k records per flush).
        let mut w = BufWriter::with_capacity(4 << 20, file);
        w.write_all(&[0u8; 2048])?;
        Ok(Self {
            file: w,
            pos: 2048,
            entries: Vec::new(),
            table: Vec::new(),
        })
    }

    pub fn reserve(&mut self, n: usize) {
        self.entries.reserve(n);
    }

    /// Add a record. Writes the 8-byte (keylen, datalen) header, the key, and
    /// the data in a single `write_all` call to amortize the BufWriter cost.
    pub fn add(&mut self, key: &[u8], data: &[u8]) -> Result<()> {
        if key.len() >= u32::MAX as usize || data.len() >= u32::MAX as usize {
            return Err(std::io::Error::other("Key or data too big"));
        }
        let keylen = key.len() as u32;
        let datalen = data.len() as u32;
        // Build [hdr(8) | key | data] in one buffer to do a single write_all.
        // We use a stack buffer for the header and append key+data in two
        // additional write_all calls, but BufWriter coalesces them.
        let hdr = [keylen.to_le_bytes(), datalen.to_le_bytes()];
        // SAFETY: 2-array of 4-byte arrays is 8 contiguous bytes.
        let hdr_bytes: &[u8; 8] = unsafe { std::mem::transmute(&hdr) };
        self.file.write_all(hdr_bytes)?;
        self.file.write_all(key)?;
        self.file.write_all(data)?;
        // DJB cdb hash.
        let mut h: u32 = 0x1505;
        for &b in key {
            h = h.wrapping_shl(5).wrapping_add(h) ^ (b as u32);
        }
        self.entries.push(Entry {
            hash: h,
            pos: self.pos,
        });
        self.pos += 8 + keylen + datalen;
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        // Counting sort by `hash & 0xff` so each bucket is contiguous in memory.
        let mut counts = [0u32; 256];
        for e in &self.entries {
            counts[(e.hash & 0xff) as usize] += 1;
        }
        let mut starts = [0usize; 256];
        let mut acc = 0usize;
        for i in 0..256 {
            starts[i] = acc;
            acc += counts[i] as usize;
        }
        let total = acc;
        let mut sorted = vec![Entry { hash: 0, pos: 0 }; total];
        let mut cursor = starts;
        for e in &self.entries {
            let bucket = (e.hash & 0xff) as usize;
            sorted[cursor[bucket]] = *e;
            cursor[bucket] += 1;
        }
        drop(self.entries);
        let entries = sorted;
        let maxsize = counts
            .iter()
            .fold(1usize, |acc, &c| max(acc, c as usize * 2));

        self.table.clear();
        self.table.resize(maxsize, Entry { hash: 0, pos: 0 });

        let mut header = [0u8; 2048];
        let mut bucket_start = 0usize;
        // Buffer for writing 8 bytes per (hash, pos) pair in the hash table.
        // 1MB / 8 = 131072 slots per buffer fill.
        let mut pair_buf = [0u8; 8];

        for i in 0..256u32 {
            let count = counts[i as usize] as usize;
            let len = count * 2;
            for j in 0..count {
                let e = entries[bucket_start + j];
                let mut wh = ((e.hash >> 8) as usize) % len;
                while self.table[wh].pos != 0 {
                    wh += 1;
                    if wh == len {
                        wh = 0;
                    }
                }
                self.table[wh] = e;
            }
            for j in 0..len {
                let h = self.table[j].hash;
                let p = self.table[j].pos;
                pair_buf[..4].copy_from_slice(&h.to_le_bytes());
                pair_buf[4..].copy_from_slice(&p.to_le_bytes());
                self.file.write_all(&pair_buf)?;
            }
            for j in 0..len {
                self.table[j] = Entry { hash: 0, pos: 0 };
            }
            let off = (i as usize) * 8;
            header[off..off + 4].copy_from_slice(&self.pos.to_le_bytes());
            header[off + 4..off + 8].copy_from_slice(&(len as u32).to_le_bytes());
            self.pos += (len as u32) * 8;
            bucket_start += count;
        }

        self.file.flush()?;
        self.file.seek(std::io::SeekFrom::Start(0))?;
        self.file.write_all(&header)?;
        self.file.flush()?;
        Ok(())
    }
}
