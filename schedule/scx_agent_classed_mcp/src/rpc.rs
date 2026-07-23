// SPDX-License-Identifier: GPL-2.0

use std::io::{self, BufRead, Write};

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::control::SchedulerControl;
use crate::provider::Server;

const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub fn serve_stdio<C: SchedulerControl>(server: &Server<C>) -> Result<()> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stdout = io::stdout();
    let mut writer = stdout.lock();

    loop {
        let Some(frame) = read_bounded_frame(&mut reader, MAX_FRAME_BYTES)? else {
            return Ok(());
        };
        let response = match frame {
            Frame::Data(data) => match serde_json::from_slice::<RpcRequest>(&data) {
                Ok(request) => server.handle_rpc(request),
                Err(error) => Some(rpc_error(
                    Value::Null,
                    -32700,
                    format!("invalid JSON: {error}"),
                )),
            },
            Frame::TooLarge => Some(rpc_error(
                Value::Null,
                -32600,
                format!("request exceeds {MAX_FRAME_BYTES} bytes"),
            )),
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut writer, &response)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
    }
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RpcRequest {
    pub(crate) jsonrpc: String,
    #[serde(default)]
    pub(crate) id: Option<Value>,
    pub(crate) method: String,
    #[serde(default)]
    pub(crate) params: Value,
}
pub(crate) fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub(crate) fn rpc_error(id: Value, code: i64, message: impl ToString) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message.to_string()}
    })
}

pub(crate) enum Frame {
    Data(Vec<u8>),
    TooLarge,
}

pub(crate) fn read_bounded_frame(
    reader: &mut impl BufRead,
    limit: usize,
) -> io::Result<Option<Frame>> {
    let mut data = Vec::new();
    let mut too_large = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if data.is_empty() && !too_large {
                Ok(None)
            } else if too_large {
                Ok(Some(Frame::TooLarge))
            } else {
                Ok(Some(Frame::Data(data)))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        let payload_len = newline.unwrap_or(available.len());
        if !too_large {
            if data.len().saturating_add(payload_len) <= limit {
                data.extend_from_slice(&available[..payload_len]);
            } else {
                data.clear();
                too_large = true;
            }
        }
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Some(if too_large {
                Frame::TooLarge
            } else {
                if data.last() == Some(&b'\r') {
                    data.pop();
                }
                Frame::Data(data)
            }));
        }
    }
}
