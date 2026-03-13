use anyhow::Context;
use iroh::endpoint::{RecvStream, SendStream};
use serde_json::json;
use skyffla_protocol::ControlMessage;
use skyffla_transport::IrohConnection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::app::sink::EventSink;
use crate::cli_error::CliError;
use crate::net::framing::read_envelope;

const STDIO_BUFFER_SIZE: usize = 64 * 1024;

pub(crate) async fn run_stdio_session(
    sink: &EventSink,
    session_id: &str,
    connection: &IrohConnection,
    send: &mut SendStream,
    recv: &mut RecvStream,
    is_host: bool,
) -> Result<(), CliError> {
    let (mut data_send, mut data_recv) = if is_host {
        connection
            .accept_data_stream()
            .await
            .map_err(|error| CliError::transport(error.to_string()))?
    } else {
        connection
            .open_data_stream()
            .await
            .map_err(|error| CliError::transport(error.to_string()))?
    };

    sink.emit_json_event(json!({
        "event": "machine_open",
        "session_id": session_id,
    }));

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut stdin_buffer = vec![0_u8; STDIO_BUFFER_SIZE];
    let mut data_buffer = vec![0_u8; STDIO_BUFFER_SIZE];
    let mut send_closed = false;
    let mut receive_closed = false;
    let mut control_closed = false;
    let mut sent_bytes = 0_u64;
    let mut received_bytes = 0_u64;

    while !(send_closed && receive_closed) {
        tokio::select! {
            read_result = stdin.read(&mut stdin_buffer), if !send_closed => {
                let bytes_read = read_result
                    .context("failed to read stdin for stdio session")
                    .map_err(|error| CliError::local_io(error.to_string()))?;
                if bytes_read == 0 {
                    data_send
                        .finish()
                        .context("failed to finish stdio send half")
                        .map_err(|error| CliError::transport(error.to_string()))?;
                    send_closed = true;
                    sink.emit_json_event(json!({
                        "event": "stdin_eof",
                    }));
                } else {
                    data_send
                        .write_all(&stdin_buffer[..bytes_read])
                        .await
                        .context("failed to write duplex stdio payload bytes")
                        .map_err(|error| CliError::transport(error.to_string()))?;
                    sent_bytes += bytes_read as u64;
                    sink.emit_json_event(json!({
                        "event": "machine_send_progress",
                        "bytes_done": sent_bytes,
                    }));
                }
            }
            read_result = data_recv.read(&mut data_buffer), if !receive_closed => {
                match read_result {
                    Ok(Some(bytes_read)) => {
                        stdout
                            .write_all(&data_buffer[..bytes_read])
                            .await
                            .context("failed to write duplex stdio payload to stdout")
                            .map_err(|error| CliError::local_io(error.to_string()))?;
                        stdout
                            .flush()
                            .await
                            .context("failed to flush stdout")
                            .map_err(|error| CliError::local_io(error.to_string()))?;
                        received_bytes += bytes_read as u64;
                        sink.emit_json_event(json!({
                            "event": "machine_receive_progress",
                            "bytes_done": received_bytes,
                        }));
                    }
                    Ok(None) => {
                        receive_closed = true;
                        sink.emit_json_event(json!({
                            "event": "remote_eof",
                        }));
                    }
                    Err(error) => {
                        return Err(CliError::transport(format!(
                            "failed reading duplex stdio payload stream: {error}"
                        )));
                    }
                }
            }
            envelope = read_envelope(recv), if !control_closed => {
                match envelope
                    .map_err(|error| CliError::protocol(error.to_string()))? {
                    Some(envelope) => {
                        match envelope.payload {
                            ControlMessage::Cancel(cancel) => {
                                return Err(CliError::cancelled(format!(
                                    "stdio session cancelled{}",
                                    cancel
                                        .reason
                                        .as_deref()
                                        .map(|reason| format!(" ({reason})"))
                                        .unwrap_or_default()
                                )));
                            }
                            ControlMessage::Error(error) => {
                                return Err(CliError::peer(format!(
                                    "stdio session error {}: {}",
                                    error.code, error.message
                                )));
                            }
                            other => {
                                return Err(CliError::protocol(format!(
                                    "unexpected control message during stdio session: {:?}",
                                    other
                                )));
                            }
                        }
                    }
                    None => {
                        control_closed = true;
                    }
                }
            }
        }
    }

    let _ = send.finish();
    sink.emit_json_event(json!({
        "event": "machine_closed",
        "send_closed": send_closed,
        "receive_closed": receive_closed,
    }));
    Ok(())
}
