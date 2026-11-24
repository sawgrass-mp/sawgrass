//! Records the snapshot datagrams a few bots receive, so the snapshot stream of a real server under load can be
//! replayed by the packet compression benchmark (`docs/research/packet-compression`).
//!
//! The file is a sequence of records, each a little-endian `u16` bot index, a little-endian `u16` length, then
//! that many bytes: one snapshot datagram exactly as it arrived, packet header included. A snapshot split over
//! several datagrams gives several records with the same tick.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;

/// Where the recorded bots write their snapshot datagrams.
#[derive(Debug)]
pub struct Recorder {
    /// Bots with an index below this record; the others do not.
    bots: u64,
    out: Mutex<BufWriter<File>>,
}

impl Recorder {
    /// Creates (or truncates) `path`, recording the bots numbered below `bots`.
    pub fn create(path: &Path, bots: u64) -> std::io::Result<Self> {
        Ok(Self {
            bots: bots.min(u64::from(u16::MAX) + 1),
            out: Mutex::new(BufWriter::new(File::create(path)?)),
        })
    }

    /// Whether bot number `index` records.
    #[must_use]
    pub fn records(&self, index: u64) -> bool {
        index < self.bots
    }

    /// Appends one datagram received by bot `index`. Datagrams longer than `u16::MAX` cannot arrive over QUIC and
    /// are skipped; a write error stops nothing, as a recording is a by-product of the run.
    pub fn write(&self, index: u64, datagram: &[u8]) {
        let (Ok(index), Ok(len)) = (u16::try_from(index), u16::try_from(datagram.len())) else {
            return;
        };
        if let Ok(mut out) = self.out.lock() {
            let _ = out.write_all(&index.to_le_bytes());
            let _ = out.write_all(&len.to_le_bytes());
            let _ = out.write_all(datagram);
        }
    }

    /// Writes out what is buffered.
    pub fn flush(&self) -> std::io::Result<()> {
        match self.out.lock() {
            Ok(mut out) => out.flush(),
            Err(_) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_only_the_first_bots_in_order() {
        let path = std::env::temp_dir().join(format!("sawgrass-record-{}.bin", std::process::id()));
        let recorder = Recorder::create(&path, 2).expect("create");
        assert!(recorder.records(0) && recorder.records(1) && !recorder.records(2));
        recorder.write(1, &[7, 8, 9]);
        recorder.write(0, &[]);
        recorder.flush().expect("flush");
        let bytes = std::fs::read(&path).expect("read");
        let _ = std::fs::remove_file(&path);
        assert_eq!(bytes, [1, 0, 3, 0, 7, 8, 9, 0, 0, 0, 0]);
    }
}
