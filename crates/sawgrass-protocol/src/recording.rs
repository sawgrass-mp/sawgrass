//! The file format of a session recording (task SRV-22): what each client sent a server, exactly as it arrived,
//! so the load-test bots can play it again.
//!
//! A recording starts with [`MAGIC`] and the little-endian `u32` protocol version of the server that wrote it.
//! Then come records, each [`Record::HEADER`] bytes followed by its payload:
//!
//! | Bytes | Field |
//! |---|---|
//! | 8 | microseconds since the recording started, little-endian `u64` |
//! | 4 | the server's connection id, little-endian `u32` |
//! | 1 | [`RecordKind`] |
//! | 2 | payload length, little-endian `u16` |
//! | n | the payload: a datagram or event with its packet header, or nothing for a join or leave |
//!
//! A recording holds what players sent: positions, chat and script events. Keep it private, and never commit one.
//! This module only encodes and decodes; the server writes the file and the bots read it.

/// The first bytes of a recording.
pub const MAGIC: [u8; 8] = *b"SGREC1\n\0";

/// The bytes before the first record: [`MAGIC`] and the protocol version.
pub const FILE_HEADER: usize = MAGIC.len() + 4;

/// Writes the start of a recording made by a server speaking `version`.
#[must_use]
pub fn file_header(version: u32) -> [u8; FILE_HEADER] {
    let mut out = [0; FILE_HEADER];
    out[..MAGIC.len()].copy_from_slice(&MAGIC);
    out[MAGIC.len()..].copy_from_slice(&version.to_le_bytes());
    out
}

/// Reads the start of a recording and returns the protocol version it was made with, or `None` if `input` does
/// not start like one.
#[must_use]
pub fn read_file_header(input: &[u8]) -> Option<u32> {
    let rest = input.strip_prefix(&MAGIC)?;
    Some(u32::from_le_bytes(rest.get(..4)?.try_into().ok()?))
}

/// What a record holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordKind {
    /// The connection finished its handshake. No payload.
    Join = 1,
    /// A datagram: a client state, with its packet header.
    Datagram = 2,
    /// A reliable event, with its packet header.
    Event = 3,
    /// The connection ended. No payload.
    Leave = 4,
}

impl RecordKind {
    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Join,
            2 => Self::Datagram,
            3 => Self::Event,
            4 => Self::Leave,
            _ => return None,
        })
    }
}

/// One record of a recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record<'a> {
    /// Microseconds since the recording started.
    pub micros: u64,
    /// The server's id for the connection.
    pub connection: u32,
    /// What the record holds.
    pub kind: RecordKind,
    /// The bytes as received, or empty for a join or leave.
    pub payload: &'a [u8],
}

/// Why a record could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordError {
    /// The input ends inside a record: a recording cut off while it was written.
    Truncated,
    /// An unknown kind: not a recording, or a damaged one.
    UnknownKind(u8),
}

impl<'a> Record<'a> {
    /// Bytes before the payload.
    pub const HEADER: usize = 8 + 4 + 1 + 2;

    /// Appends the record to `out`. A payload longer than `u16::MAX` cannot arrive over QUIC, so it is cut there.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let len = self.payload.len().min(usize::from(u16::MAX));
        out.extend_from_slice(&self.micros.to_le_bytes());
        out.extend_from_slice(&self.connection.to_le_bytes());
        out.push(self.kind as u8);
        out.extend_from_slice(&(len as u16).to_le_bytes());
        out.extend_from_slice(&self.payload[..len]);
    }

    /// Reads the record at the start of `input` and returns it with the bytes after it.
    ///
    /// # Errors
    ///
    /// [`RecordError::Truncated`] if `input` ends inside the record, [`RecordError::UnknownKind`] for a kind this
    /// version does not know.
    pub fn decode(input: &'a [u8]) -> Result<(Self, &'a [u8]), RecordError> {
        let header = input.get(..Self::HEADER).ok_or(RecordError::Truncated)?;
        let micros = u64::from_le_bytes(header[..8].try_into().map_err(|_| RecordError::Truncated)?);
        let connection = u32::from_le_bytes(header[8..12].try_into().map_err(|_| RecordError::Truncated)?);
        let kind = RecordKind::from_code(header[12]).ok_or(RecordError::UnknownKind(header[12]))?;
        let len = usize::from(u16::from_le_bytes([header[13], header[14]]));
        let rest = &input[Self::HEADER..];
        let payload = rest.get(..len).ok_or(RecordError::Truncated)?;
        Ok((
            Self {
                micros,
                connection,
                kind,
                payload,
            },
            &rest[len..],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_one_after_another() {
        let records = [
            Record {
                micros: 0,
                connection: 1,
                kind: RecordKind::Join,
                payload: &[],
            },
            Record {
                micros: 50_000,
                connection: 1,
                kind: RecordKind::Datagram,
                payload: &[1, 2, 3],
            },
            Record {
                micros: 50_001,
                connection: 7,
                kind: RecordKind::Event,
                payload: &[9; 300],
            },
            Record {
                micros: u64::MAX,
                connection: u32::MAX,
                kind: RecordKind::Leave,
                payload: &[],
            },
        ];
        let mut file = file_header(9).to_vec();
        for r in &records {
            r.encode(&mut file);
        }
        assert_eq!(read_file_header(&file), Some(9));
        let mut rest = &file[FILE_HEADER..];
        for r in &records {
            let (read, after) = Record::decode(rest).expect("record");
            assert_eq!(&read, r);
            rest = after;
        }
        assert!(rest.is_empty());
    }

    #[test]
    fn a_cut_or_foreign_file_is_an_error_not_a_panic() {
        assert_eq!(read_file_header(b"SGREC1\n"), None);
        assert_eq!(read_file_header(b"not a recording"), None);
        let mut one = Vec::new();
        Record {
            micros: 1,
            connection: 2,
            kind: RecordKind::Datagram,
            payload: &[5; 10],
        }
        .encode(&mut one);
        for cut in 0..one.len() {
            assert_eq!(
                Record::decode(&one[..cut]),
                Err(RecordError::Truncated),
                "cut at {cut}"
            );
        }
        one[12] = 0;
        assert_eq!(Record::decode(&one), Err(RecordError::UnknownKind(0)));
    }
}
