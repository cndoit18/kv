use crate::{CommandRequest, CommandResponse, KvError};
use anyhow::Result;
use bytes::{Buf, BufMut, BytesMut};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use prost::Message;
use std::io::{Read, Write};
use tokio::io::{AsyncRead, AsyncReadExt};
use tracing::debug;

pub const LEN_LEN: usize = 4;
const MAX_FRAME: usize = 2 * 1024 * 1024 * 1024;
const COMPRESSION_LIMIT: usize = 1436;
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
            debug!("Encode a frame: size {}({})", size, payload.len());

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
        debug!("Got a frame: msg len {}, compressed {}", l, compressed);

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

pub async fn read_frame<S>(stream: &mut S, buf: &mut BytesMut) -> Result<()>
where
    S: AsyncRead + Unpin + Send,
{
    let header = stream.read_u32().await? as usize;
    let (len, _compressed) = decode_header(header);
    buf.reserve(LEN_LEN + len);
    buf.put_u32(header as _);
    unsafe { buf.advance_mut(len) };
    stream.read_exact(&mut buf[LEN_LEN..]).await?;
    Ok(())
}

impl FrameCoder for CommandRequest {}
impl FrameCoder for CommandResponse {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommandRequest, CommandResponse, Value};
    use bytes::{Bytes, BytesMut};

    #[test]
    fn command_request_encode_decode_should_work() {
        let mut buf = BytesMut::new();

        let cmd = CommandRequest::new_hdel("t1", "k1");
        cmd.encode_frame(&mut buf).unwrap();

        // 最高位没设置
        assert_eq!(is_compressed(&buf), false);

        let cmd1 = CommandRequest::decode_frame(&mut buf).unwrap();
        assert_eq!(cmd, cmd1);
    }

    #[test]
    fn command_response_encode_decode_should_work() {
        let mut buf = BytesMut::new();

        let values: Vec<Value> = vec![1.into(), "hello".into(), b"data".into()];
        let res: CommandResponse = values.into();
        res.encode_frame(&mut buf).unwrap();

        // 最高位没设置
        assert_eq!(is_compressed(&buf), false);

        let res1 = CommandResponse::decode_frame(&mut buf).unwrap();
        assert_eq!(res, res1);
    }

    #[test]
    fn command_response_compressed_encode_decode_should_work() {
        let mut buf = BytesMut::new();

        let value: Value = Bytes::from(vec![0u8; COMPRESSION_LIMIT + 1]).into();
        let res: CommandResponse = value.into();
        res.encode_frame(&mut buf).unwrap();

        // 最高位设置了
        assert_eq!(is_compressed(&buf), true);

        let res1 = CommandResponse::decode_frame(&mut buf).unwrap();
        assert_eq!(res, res1);
    }

    fn is_compressed(data: &[u8]) -> bool {
        if let &[v] = &data[..1] {
            v >> 7 == 1
        } else {
            false
        }
    }

    struct DummyStream {
        buf: BytesMut,
    }

    impl AsyncRead for DummyStream {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            // 看看 ReadBuf 需要多大的数据
            let len = buf.capacity();

            // split 出这么大的数据
            let data = self.get_mut().buf.split_to(len);

            // 拷贝给 ReadBuf
            buf.put_slice(&data);

            // 直接完工
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn read_frame_should_work() {
        let mut buf = BytesMut::new();
        let cmd = CommandRequest::new_hdel("t1", "k1");
        cmd.encode_frame(&mut buf).unwrap();
        let mut stream = DummyStream { buf };

        let mut data = BytesMut::new();
        read_frame(&mut stream, &mut data).await.unwrap();

        let cmd1 = CommandRequest::decode_frame(&mut data).unwrap();
        assert_eq!(cmd, cmd1);
    }
}
