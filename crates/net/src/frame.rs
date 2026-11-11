//! Length-prefixed frames: `u32 little-endian length` + postcard payload.

use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {0} bytes exceeds limit {1}")]
    TooLarge(usize, usize),
    #[error(transparent)]
    Decode(#[from] flow_proto::CodecError),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}

pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> Result<(), FrameError> {
    let body = flow_proto::encode(msg);
    write_raw(w, &body).await
}

pub async fn write_raw<W: AsyncWrite + Unpin>(w: &mut W, body: &[u8]) -> Result<(), FrameError> {
    let mut buf = Vec::with_capacity(body.len() + 4);
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(body);
    w.write_all(&buf).await?;
    Ok(())
}

/// Reads one frame. `Ok(None)` on a clean end of stream before a frame.
pub async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(
    r: &mut R,
    max: usize,
) -> Result<Option<T>, FrameError> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > max {
        return Err(FrameError::TooLarge(len, max));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Some(flow_proto::decode(&body)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flow_proto::{InputEvent, LinkMsg};

    #[tokio::test]
    async fn roundtrip_and_limits() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let m = LinkMsg::Event { session: 1, seq: 2, base: 1, ev: InputEvent::Leave };
        write_frame(&mut a, &m).await.unwrap();
        write_frame(&mut a, &m).await.unwrap();
        assert_eq!(read_frame::<_, LinkMsg>(&mut b, 100).await.unwrap(), Some(m.clone()));
        // Limit enforced before allocating.
        assert!(matches!(read_frame::<_, LinkMsg>(&mut b, 2).await, Err(FrameError::TooLarge(_, 2))));
        drop(a);
        let (mut a, mut b) = tokio::io::duplex(4096);
        // Garbage body.
        write_raw(&mut a, &[0xff, 0xff, 0xff]).await.unwrap();
        assert!(matches!(read_frame::<_, LinkMsg>(&mut b, 100).await, Err(FrameError::Decode(_))));
        // Truncated frame then EOF.
        a.write_all(&10u32.to_le_bytes()).await.unwrap();
        a.write_all(&[1, 2]).await.unwrap();
        drop(a);
        assert!(matches!(read_frame::<_, LinkMsg>(&mut b, 100).await, Err(FrameError::Io(_))));
        // Clean EOF.
        let (a, mut b) = tokio::io::duplex(16);
        drop(a);
        assert!(read_frame::<_, LinkMsg>(&mut b, 100).await.unwrap().is_none());
    }
}
