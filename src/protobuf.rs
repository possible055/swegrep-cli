use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use std::io::{Read, Write};

#[derive(Default, Debug, Clone)]
pub struct ProtobufEncoder {
    chunks: Vec<Vec<u8>>,
}

impl ProtobufEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn varint(mut value: u64) -> Vec<u8> {
        if value == 0 {
            return vec![0];
        }

        let mut bytes = Vec::new();
        while value > 0x7f {
            bytes.push(((value & 0x7f) as u8) | 0x80);
            value >>= 7;
        }
        bytes.push((value & 0x7f) as u8);
        bytes
    }

    fn tag(field: u64, wire: u64) -> Vec<u8> {
        Self::varint((field << 3) | wire)
    }

    pub fn write_varint(&mut self, field: u64, value: u64) -> &mut Self {
        self.chunks.push(Self::tag(field, 0));
        self.chunks.push(Self::varint(value));
        self
    }

    pub fn write_string(&mut self, field: u64, value: &str) -> &mut Self {
        self.write_bytes(field, value.as_bytes())
    }

    pub fn write_bytes(&mut self, field: u64, value: &[u8]) -> &mut Self {
        self.chunks.push(Self::tag(field, 2));
        self.chunks.push(Self::varint(value.len() as u64));
        self.chunks.push(value.to_vec());
        self
    }

    pub fn write_message(&mut self, field: u64, sub: &ProtobufEncoder) -> &mut Self {
        let data = sub.to_bytes();
        self.write_bytes(field, &data)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.chunks.concat()
    }
}

pub fn decode_varint(buf: &[u8], mut offset: usize) -> (u64, usize) {
    let mut value = 0_u64;
    let mut shift = 0_u32;

    while offset < buf.len() {
        let byte = buf[offset];
        offset += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            break;
        }
    }

    (value, offset)
}

pub fn extract_strings(data: &[u8]) -> Vec<String> {
    let mut strings = Vec::new();
    let mut i = 0_usize;

    while i < data.len() {
        let (tag, new_i) = decode_varint(data, i);
        if new_i == i {
            break;
        }
        i = new_i;
        let wire = tag & 0x7;

        match wire {
            0 => {
                let (_, new_i) = decode_varint(data, i);
                if new_i == i {
                    break;
                }
                i = new_i;
            }
            1 => i = i.saturating_add(8),
            2 => {
                let (length, new_i) = decode_varint(data, i);
                if new_i == i {
                    break;
                }
                i = new_i;
                let length = length as usize;
                if i + length <= data.len() {
                    let raw = &data[i..i + length];
                    if let Ok(text) = std::str::from_utf8(raw)
                        && text.len() > 5
                    {
                        strings.push(text.to_string());
                    }
                }
                i = i.saturating_add(length);
            }
            5 => i = i.saturating_add(4),
            _ => break,
        }
    }

    strings
}

pub fn gzip_compress(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data)?;
    encoder.finish()
}

pub fn gzip_decompress(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

pub fn connect_frame_encode(proto_bytes: &[u8], compress: bool) -> Vec<u8> {
    let (flags, payload) = if compress {
        (
            1_u8,
            gzip_compress(proto_bytes).unwrap_or_else(|_| proto_bytes.to_vec()),
        )
    } else {
        (0_u8, proto_bytes.to_vec())
    };

    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(flags);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    frame
}

pub fn connect_frame_decode(data: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut i = 0_usize;

    while i + 5 <= data.len() {
        let flags = data[i];
        let length =
            u32::from_be_bytes([data[i + 1], data[i + 2], data[i + 3], data[i + 4]]) as usize;
        i += 5;
        if i + length > data.len() {
            break;
        }

        let payload = &data[i..i + length];
        i += length;
        if matches!(flags, 1 | 3) {
            frames.push(gzip_decompress(payload).unwrap_or_else(|_| payload.to_vec()));
        } else {
            frames.push(payload.to_vec());
        }
    }

    frames
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_encode_decode() {
        assert_eq!(ProtobufEncoder::varint(0), b"\x00");
        assert_eq!(ProtobufEncoder::varint(1), b"\x01");
        assert_eq!(ProtobufEncoder::varint(127), b"\x7f");
        assert_eq!(ProtobufEncoder::varint(128), b"\x80\x01");
        assert_eq!(ProtobufEncoder::varint(300), b"\xac\x02");

        assert_eq!(decode_varint(b"\x00", 0), (0, 1));
        assert_eq!(decode_varint(b"\xac\x02", 0), (300, 2));
    }

    #[test]
    fn protobuf_encoder_writes_expected_bytes() {
        let mut encoder = ProtobufEncoder::new();
        encoder.write_varint(1, 150);
        assert_eq!(encoder.to_bytes(), b"\x08\x96\x01");

        let mut encoder = ProtobufEncoder::new();
        encoder.write_string(2, "hello");
        assert_eq!(encoder.to_bytes(), b"\x12\x05hello");

        let mut encoder = ProtobufEncoder::new();
        encoder.write_bytes(3, b"\x01\x02");
        assert_eq!(encoder.to_bytes(), b"\x1a\x02\x01\x02");

        let mut sub = ProtobufEncoder::new();
        sub.write_varint(1, 10);
        let mut encoder = ProtobufEncoder::new();
        encoder.write_message(4, &sub);
        assert_eq!(encoder.to_bytes(), b"\x22\x02\x08\x0a");
    }

    #[test]
    fn connect_frame_encode_decode_round_trips() {
        let data = b"my test protobuf payload";

        let frame = connect_frame_encode(data, true);
        assert_eq!(frame[0], 1);
        assert_eq!(connect_frame_decode(&frame), vec![data.to_vec()]);

        let frame = connect_frame_encode(data, false);
        assert_eq!(frame[0], 0);
        assert_eq!(connect_frame_decode(&frame), vec![data.to_vec()]);
    }

    #[test]
    fn extract_strings_filters_short_values() {
        let mut encoder = ProtobufEncoder::new();
        encoder.write_string(1, "short");
        encoder.write_string(2, "longer_string_here");
        encoder.write_varint(3, 99999);
        encoder.write_string(4, "another_long_one");

        let strings = extract_strings(&encoder.to_bytes());
        assert!(strings.contains(&"longer_string_here".to_string()));
        assert!(strings.contains(&"another_long_one".to_string()));
        assert!(!strings.contains(&"short".to_string()));
    }
}
