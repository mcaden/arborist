use std::io;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub async fn write_frame<W: AsyncWriteExt + Unpin>(writer: &mut W, bytes: &[u8]) -> io::Result<()> {
    let len = u32::try_from(bytes.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame exceeds u32 length limit"))?;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(bytes).await?;
    writer.flush().await
}

pub async fn read_frame<R: AsyncReadExt + Unpin>(reader: &mut R, max_len: usize) -> io::Result<Vec<u8>> {
    let mut len_bytes = [0_u8; 4];
    reader.read_exact(&mut len_bytes).await?;

    let len = u32::from_be_bytes(len_bytes) as usize;
    if len > max_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds maximum {max_len}"),
        ));
    }

    let mut buf = vec![0_u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;

    use tokio::io::{duplex, AsyncWriteExt};

    use super::{read_frame, write_frame};

    #[tokio::test]
    async fn round_trips_a_frame() {
        let payload = br#"{\"ok\":true}"#.to_vec();
        let (mut writer, mut reader) = duplex(128);

        let writer_task = tokio::spawn(async move { write_frame(&mut writer, &payload).await });
        let received = read_frame(&mut reader, 1024).await.expect("frame should round-trip");

        writer_task.await.expect("writer task should finish").expect("writer should succeed");
        assert_eq!(received, br#"{\"ok\":true}"#);
    }

    #[tokio::test]
    async fn rejects_oversize_frames() {
        let (mut writer, mut reader) = duplex(128);
        writer.write_all(&5_u32.to_be_bytes()).await.expect("length prefix should write");

        let error = read_frame(&mut reader, 4).await.expect_err("oversize frame should fail");
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn errors_on_truncated_frame_body() {
        let (mut writer, mut reader) = duplex(128);
        writer.write_all(&5_u32.to_be_bytes()).await.expect("length prefix should write");
        writer.write_all(b"abc").await.expect("partial body should write");
        drop(writer);

        let error = read_frame(&mut reader, 1024).await.expect_err("truncated frame should fail");
        assert_eq!(error.kind(), ErrorKind::UnexpectedEof);
    }
}
