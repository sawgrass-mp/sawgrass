//! Records what each client sends, for `sawgrass-bots replay` (task SRV-22). The format is
//! [`sawgrass_protocol::recording`].
//!
//! Connection tasks hand each record to a writer thread through a queue bounded both in records and in bytes. A
//! record that does not fit is dropped and counted, so a slow disk never slows a connection or the tick and the
//! queue never holds more than [`QUEUE_BYTES`]; the count is reported with the recording.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sawgrass_protocol::PROTOCOL_VERSION;
use sawgrass_protocol::recording::{Record, RecordKind, file_header};

/// Records the writer thread may be behind by before new ones are dropped: about two seconds of 4096 players
/// sending 20 states a second.
const QUEUE: usize = 1 << 17;

/// Bytes the queue may hold before new records are dropped. Records are taken before any rate limit, so without
/// this a flood of 64 KiB events could fill [`QUEUE`] with gigabytes.
pub const QUEUE_BYTES: usize = 256 << 20;

/// How often the writer thread writes out what it buffered when records keep coming.
const FLUSH_EVERY: Duration = Duration::from_millis(500);

/// A recording being written. Share it between connections with an [`Arc`]; [`Recorder::finish`] ends it.
#[derive(Debug)]
pub struct Recorder {
    start: Instant,
    queue: SyncSender<Vec<u8>>,
    writer: Mutex<Option<JoinHandle<std::io::Result<()>>>>,
    written: Arc<AtomicU64>,
    dropped: AtomicU64,
    queued_bytes: Arc<AtomicUsize>,
    max_queued_bytes: usize,
}

impl Recorder {
    /// Creates (or truncates) `path` and starts the writer thread.
    ///
    /// # Errors
    ///
    /// If the file cannot be created, its header written, or the thread started.
    pub fn create(path: &Path) -> std::io::Result<Arc<Self>> {
        Self::with_byte_limit(path, QUEUE_BYTES)
    }

    /// [`Recorder::create`] with a queue that holds at most `max_queued_bytes`.
    ///
    /// # Errors
    ///
    /// As [`Recorder::create`].
    pub fn with_byte_limit(path: &Path, max_queued_bytes: usize) -> std::io::Result<Arc<Self>> {
        let mut out = BufWriter::new(File::create(path)?);
        out.write_all(&file_header(PROTOCOL_VERSION))?;
        let (queue, records) = mpsc::sync_channel::<Vec<u8>>(QUEUE);
        let written = Arc::new(AtomicU64::new(0));
        let counted = written.clone();
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let dequeued = queued_bytes.clone();
        let writer =
            std::thread::Builder::new()
                .name("recording".into())
                .spawn(move || -> std::io::Result<()> {
                    let mut flushed = Instant::now();
                    loop {
                        match records.recv_timeout(FLUSH_EVERY) {
                            // An empty message is `finish`.
                            Ok(record) if record.is_empty() => break,
                            Ok(record) => {
                                dequeued.fetch_sub(record.len(), Ordering::Relaxed);
                                out.write_all(&record)?;
                                counted.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                        if flushed.elapsed() >= FLUSH_EVERY {
                            out.flush()?;
                            flushed = Instant::now();
                        }
                    }
                    out.flush()
                })?;
        Ok(Arc::new(Self {
            start: Instant::now(),
            queue,
            writer: Mutex::new(Some(writer)),
            written,
            dropped: AtomicU64::new(0),
            queued_bytes,
            max_queued_bytes,
        }))
    }

    /// Queues one record for `connection`, stamped now. Never blocks: a record that would take the queue past its
    /// record or byte limit is dropped and counted.
    pub fn record(&self, connection: u32, kind: RecordKind, payload: &[u8]) {
        let size = Record::HEADER + payload.len();
        let taken = self.queued_bytes.fetch_add(size, Ordering::Relaxed);
        if taken.saturating_add(size) > self.max_queued_bytes {
            self.queued_bytes.fetch_sub(size, Ordering::Relaxed);
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let micros = u64::try_from(self.start.elapsed().as_micros()).unwrap_or(u64::MAX);
        let mut bytes = Vec::with_capacity(size);
        let encoded = Record {
            micros,
            connection,
            kind,
            payload,
        }
        .encode(&mut bytes);
        if encoded.is_err() || self.queue.try_send(bytes).is_err() {
            self.queued_bytes.fetch_sub(size, Ordering::Relaxed);
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Bytes queued for the writer thread and not yet written.
    pub fn queued_bytes(&self) -> usize {
        self.queued_bytes.load(Ordering::Relaxed)
    }

    /// Records written to the file so far.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    /// Records dropped because the writer was behind: its queue was full in records or in bytes.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Writes out everything queued before this call and closes the file. Records queued after it are dropped.
    ///
    /// # Errors
    ///
    /// The first write error the writer thread met.
    pub fn finish(&self) -> std::io::Result<()> {
        let Some(writer) = self.writer.lock().ok().and_then(|mut w| w.take()) else {
            return Ok(());
        };
        // Blocks only until the writer takes it: everything queued before is written first.
        let _ = self.queue.send(Vec::new());
        writer
            .join()
            .unwrap_or_else(|_| Err(std::io::Error::other("the recording thread panicked")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sawgrass_protocol::recording::{FILE_HEADER, read_file_header};

    #[test]
    fn records_arrive_in_order_after_finish() {
        let path = std::env::temp_dir().join(format!("sawgrass-recording-{}", std::process::id()));
        let recorder = Recorder::create(&path).expect("create");
        recorder.record(3, RecordKind::Join, &[]);
        for i in 0..1000u16 {
            recorder.record(3, RecordKind::Datagram, &i.to_le_bytes());
        }
        recorder.record(3, RecordKind::Leave, &[]);
        recorder.finish().expect("finish");
        assert_eq!((recorder.written(), recorder.dropped()), (1002, 0));
        let file = std::fs::read(&path).expect("read");
        let _ = std::fs::remove_file(&path);
        assert_eq!(read_file_header(&file), Some(PROTOCOL_VERSION));
        let (mut rest, mut last, mut kinds) = (&file[FILE_HEADER..], 0, Vec::new());
        while !rest.is_empty() {
            let (record, after) = Record::decode(rest).expect("record");
            assert!(record.micros >= last && record.connection == 3);
            last = record.micros;
            if record.kind == RecordKind::Datagram {
                assert_eq!(
                    record.payload,
                    &u16::try_from(kinds.len() - 1).expect("index").to_le_bytes()
                );
            }
            kinds.push(record.kind);
            rest = after;
        }
        assert_eq!(kinds.len(), 1002);
        assert_eq!((kinds[0], kinds[1001]), (RecordKind::Join, RecordKind::Leave));
        // A second finish and records after it do nothing harmful.
        recorder.finish().expect("again");
        recorder.record(3, RecordKind::Leave, &[]);
    }

    #[test]
    fn the_queue_is_capped_in_bytes_and_keeps_maximum_events_whole() {
        let path = std::env::temp_dir().join(format!("sawgrass-recording-bytes-{}", std::process::id()));
        let event = vec![7u8; 64 * 1024];
        let size = Record::HEADER + event.len();
        // Room for three maximum-size events, so most of a burst of a hundred is dropped while the writer starts.
        let recorder = Recorder::with_byte_limit(&path, 3 * size).expect("create");
        for _ in 0..100 {
            recorder.record(1, RecordKind::Event, &event);
            assert!(recorder.queued_bytes() <= 3 * size);
        }
        recorder.finish().expect("finish");
        let (written, dropped) = (recorder.written(), recorder.dropped());
        assert_eq!(written + dropped, 100);
        assert!(written >= 3 && dropped > 0, "written {written} dropped {dropped}");
        assert_eq!(recorder.queued_bytes(), 0);
        let file = std::fs::read(&path).expect("read");
        let _ = std::fs::remove_file(&path);
        let mut rest = &file[FILE_HEADER..];
        let mut read = 0;
        while !rest.is_empty() {
            let (record, after) = Record::decode(rest).expect("record");
            assert_eq!(record.payload, &event[..]);
            read += 1;
            rest = after;
        }
        assert_eq!(read, written);
    }
}
