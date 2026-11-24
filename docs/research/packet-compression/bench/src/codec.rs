//! General-purpose compressors, applied to one datagram at a time: each packet is compressed on its own, so a lost
//! datagram never breaks the ones after it. Decompression is bounded, so a hostile packet cannot claim a huge size.

use zstd::bulk::{Compressor as ZCompressor, Decompressor as ZDecompressor};
use zstd::zstd_safe::CParameter;

/// No packet decompresses to more than this; anything that says otherwise is rejected.
pub const MAX_PACKET: usize = 16 * 1024;
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

pub trait Codec {
    fn name(&self) -> String;
    /// Compresses into the codec's own buffer.
    fn compress(&mut self, input: &[u8]) -> &[u8];
    /// Decompresses untrusted bytes into the codec's own buffer, or rejects them.
    fn decompress(&mut self, input: &[u8]) -> Option<&[u8]>;
}

/// No compression: the postcard or delta bytes as they are.
#[derive(Default)]
pub struct Plain(Vec<u8>);

impl Codec for Plain {
    fn name(&self) -> String {
        "none".into()
    }
    fn compress(&mut self, input: &[u8]) -> &[u8] {
        self.0.clear();
        self.0.extend_from_slice(input);
        &self.0
    }
    fn decompress(&mut self, input: &[u8]) -> Option<&[u8]> {
        self.compress(input);
        Some(&self.0)
    }
}

/// LZ4 block format (`lz4_flex`, safe Rust), with the original length as a 2-byte prefix.
pub struct Lz4 {
    out: Vec<u8>,
}

impl Lz4 {
    pub fn new() -> Self {
        Self {
            out: vec![0; lz4_flex::block::get_maximum_output_size(MAX_PACKET) + 2],
        }
    }
}

impl Codec for Lz4 {
    fn name(&self) -> String {
        "lz4".into()
    }
    fn compress(&mut self, input: &[u8]) -> &[u8] {
        let len = u16::try_from(input.len()).expect("packet under 64 KB");
        self.out[..2].copy_from_slice(&len.to_le_bytes());
        let n = lz4_flex::block::compress_into(input, &mut self.out[2..]).expect("fits");
        &self.out[..2 + n]
    }
    fn decompress(&mut self, input: &[u8]) -> Option<&[u8]> {
        let len = usize::from(u16::from_le_bytes(input.get(..2)?.try_into().ok()?));
        if len > MAX_PACKET {
            return None;
        }
        let n = lz4_flex::block::decompress_into(&input[2..], &mut self.out[..len]).ok()?;
        (n == len).then(|| &self.out[..len])
    }
}

/// zstd at a given level, optionally with a dictionary trained on game packets. The 4-byte frame magic is the
/// same in every packet, so it is dropped on the wire and put back before decompressing; with a dictionary, its
/// id is left out of the frame too, since both ends know which dictionary the protocol version uses.
pub struct Zstd {
    level: i32,
    dict_bytes: usize,
    compressor: ZCompressor<'static>,
    decompressor: ZDecompressor<'static>,
    out: Vec<u8>,
    framed: Vec<u8>,
}

impl Zstd {
    pub fn new(level: i32, dict: Option<&[u8]>) -> Self {
        let (mut compressor, decompressor) = match dict {
            Some(d) => (
                ZCompressor::with_dictionary(level, d).expect("dictionary"),
                ZDecompressor::with_dictionary(d).expect("dictionary"),
            ),
            None => (
                ZCompressor::new(level).expect("level"),
                ZDecompressor::new().expect("zstd"),
            ),
        };
        compressor
            .set_parameter(CParameter::DictIdFlag(false))
            .expect("parameter");
        compressor
            .set_parameter(CParameter::ChecksumFlag(false))
            .expect("parameter");
        Self {
            level,
            dict_bytes: dict.map_or(0, <[u8]>::len),
            compressor,
            decompressor,
            out: Vec::with_capacity(MAX_PACKET + 64),
            framed: Vec::with_capacity(MAX_PACKET + 64),
        }
    }
}

impl Codec for Zstd {
    fn name(&self) -> String {
        match self.dict_bytes {
            0 => format!("zstd-{}", self.level),
            n => format!("zstd-{}-dict{}k", self.level, n / 1024),
        }
    }
    fn compress(&mut self, input: &[u8]) -> &[u8] {
        self.out.clear();
        self.compressor
            .compress_to_buffer(input, &mut self.out)
            .expect("compress");
        debug_assert_eq!(self.out[..4], ZSTD_MAGIC);
        &self.out[4..]
    }
    fn decompress(&mut self, input: &[u8]) -> Option<&[u8]> {
        self.framed.clear();
        self.framed.extend_from_slice(&ZSTD_MAGIC);
        self.framed.extend_from_slice(input);
        self.out.clear();
        // The buffer's capacity bounds the output: a frame that claims or produces more fails.
        let n = self
            .decompressor
            .decompress_to_buffer(&self.framed, &mut self.out)
            .ok()?;
        (n <= MAX_PACKET).then(|| &self.out[..n])
    }
}
