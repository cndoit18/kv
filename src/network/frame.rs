use crate::{KvError, CommandResponse, CommandRequest};
use anyhow::Result;
use bytes::{Buf, BufMut, BytesMut};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use prost::Message;
use std::io::{Read, Write};

pub const LEN_LEN: usize = 4;
const MAX_FRAME: usize = 2 * 1024 * 1024 * 1024;
pub(super) const COMPRESSION_LIMIT: usize = 1436;
const COMPRESSION_BIT: usize = 1 << 31;

pub trait FrameCoder
where
    Self: Message + Sized + Default,
{
    fn encode_frame(&self, buf: &mut BytesMut) -> Result<(), KvError> {
        let size = self.encoded_len();
        if size > MAX_FRAME {
            return Err(KvError::Internal(String::from(
                "overflow frame length limit",
            )));
        }
        buf.put_u32(size as _);
        if size > COMPRESSION_LIMIT {
            let mut tmp = Vec::with_capacity(size);
            self.encode(&mut tmp)?;
            let payload = buf.split_off(LEN_LEN);
            buf.clear();

            let mut encoder = GzEncoder::new(payload.writer(), Compression::default());
            encoder
                .write_all(&tmp[..])
                .map_err(|e| KvError::Internal(e.to_string()))?;

            let payload = encoder
                .finish()
                .map_err(|e| KvError::Internal(e.to_string()))?
                .into_inner();
            buf.put_u32((payload.len() | COMPRESSION_BIT) as _);
            buf.unsplit(payload);
            Ok(())
        } else {
            self.encode(buf)?;
            Ok(())
        }
    }

    fn decode_frame(buf: &mut BytesMut) -> Result<Self, KvError> {
        let header = buf.get_u32() as usize;
        let (l, compressed) = decode_header(header);
        if compressed {
            let mut decoder = GzDecoder::new(&buf[..l]);
            let mut tmp = Vec::with_capacity(l * 2);
            let _ = decoder.read_to_end(&mut tmp);
            buf.advance(l);
            Ok(Self::decode(&tmp[..tmp.len()]).map_err(|e| KvError::Internal(e.to_string()))?)
        } else {
            let msg = Self::decode(&buf[..l]).map_err(|e| KvError::Internal(e.to_string()))?;
            buf.advance(l);
            Ok(msg)
        }
    }
}

fn decode_header(header: usize) -> (usize, bool) {
    let len = header & !COMPRESSION_BIT;
    let compressed = header & COMPRESSION_BIT == COMPRESSION_BIT;
    (len, compressed)
}


impl FrameCoder for CommandRequest {}
impl FrameCoder for CommandResponse {}
