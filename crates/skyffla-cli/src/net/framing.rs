use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};
use iroh::endpoint::{ReadError, ReadExactError, RecvStream, SendStream};
use skyffla_protocol::{
    decode_frame, encode_frame, ControlMessage, DataStreamHeader, Envelope, ErrorMessage,
};
use tokio::io::AsyncWriteExt;

static MESSAGE_COUNTER: AtomicU64 = AtomicU64::new(1);

pub(crate) async fn send_transfer_error(
    session_id: &str,
    send: &mut SendStream,
    code: &str,
    message: &str,
    transfer_id: Option<&str>,
) -> Result<()> {
    write_envelope(
        send,
        &Envelope::new(
            session_id,
            next_message_id(),
            ControlMessage::Error(ErrorMessage {
                code: code.to_string(),
                message: message.to_string(),
                transfer_id: transfer_id.map(ToOwned::to_owned),
            }),
        ),
    )
    .await
}

pub(crate) async fn write_envelope(send: &mut SendStream, envelope: &Envelope) -> Result<()> {
    let bytes = encode_frame(envelope)?;
    send.write_all(&bytes)
        .await
        .context("failed to write envelope bytes")?;
    send.flush()
        .await
        .context("failed to flush envelope bytes")?;
    Ok(())
}

pub(crate) async fn write_data_header(
    send: &mut SendStream,
    header: &DataStreamHeader,
) -> Result<()> {
    let bytes = encode_frame(header)?;
    send.write_all(&bytes)
        .await
        .context("failed to write data header bytes")?;
    send.flush()
        .await
        .context("failed to flush data header bytes")?;
    Ok(())
}

pub(crate) async fn read_envelope(recv: &mut RecvStream) -> Result<Option<Envelope>> {
    read_framed_message(recv, "envelope").await
}

pub(crate) async fn read_data_header(recv: &mut RecvStream) -> Result<Option<DataStreamHeader>> {
    read_framed_message(recv, "data header").await
}

pub(crate) fn next_message_id() -> String {
    let counter = MESSAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("m{counter}")
}

async fn read_framed_message<T>(recv: &mut RecvStream, label: &str) -> Result<Option<T>>
where
    T: serde::de::DeserializeOwned,
{
    let mut prefix = [0_u8; 4];
    match recv.read_exact(&mut prefix).await {
        Ok(_) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(ReadExactError::FinishedEarly(bytes_read)) => {
            bail!("peer closed {label} mid-frame after {bytes_read} bytes")
        }
        Err(ReadExactError::ReadError(ReadError::ClosedStream))
        | Err(ReadExactError::ReadError(ReadError::ConnectionLost(_))) => return Ok(None),
        Err(ReadExactError::ReadError(error)) => {
            return Err(error).with_context(|| format!("failed to read {label} prefix"))
        }
    }

    let payload_len = u32::from_be_bytes(prefix) as usize;
    let mut frame = Vec::with_capacity(4 + payload_len);
    frame.extend_from_slice(&prefix);
    frame.resize(4 + payload_len, 0);
    recv.read_exact(&mut frame[4..])
        .await
        .with_context(|| format!("failed to read {label} payload"))?;
    Ok(Some(decode_frame(&frame)?))
}
