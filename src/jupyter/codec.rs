//! DEFAULT Jupyter WebSocket protocol. Buffers remain raw bytes and are never UTF-8 decoded.
use crate::{Error, Result};
use serde_json::{Value, json};

pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_SEGMENTS: usize = 65;
#[derive(Debug, Clone, PartialEq)]
pub struct KernelMessage {
    pub envelope: Value,
    pub buffers: Vec<Vec<u8>>,
}
#[derive(Debug, PartialEq)]
pub enum Frame {
    Text(String),
    Binary(Vec<u8>),
}
impl KernelMessage {
    pub fn new(kind: &str, channel: &str, session: &str, content: Value) -> Self {
        Self {
            envelope: json!({"header":{"msg_id":message_id(),"msg_type":kind,"session":session,"username":"colab-cli",
            "date":chrono::Utc::now().to_rfc3339(),"version":"5.3"},"parent_header":{},"metadata":{},"content":content,"channel":channel}),
            buffers: Vec::new(),
        }
    }
    pub fn kind(&self) -> &str {
        self.envelope["header"]["msg_type"].as_str().unwrap_or("")
    }
    pub fn id(&self) -> &str {
        self.envelope["header"]["msg_id"].as_str().unwrap_or("")
    }
    pub fn channel(&self) -> &str {
        self.envelope["channel"].as_str().unwrap_or("")
    }
    pub fn parent(&self) -> Option<&str> {
        self.envelope["parent_header"]["msg_id"].as_str()
    }
    pub fn content(&self) -> &Value {
        &self.envelope["content"]
    }
    pub fn validate(&self) -> Result<()> {
        if self.id().is_empty()
            || self.kind().is_empty()
            || !["shell", "iopub", "stdin", "control"].contains(&self.channel())
            || !self.envelope["header"].is_object()
            || !self.envelope["parent_header"].is_object()
            || !self.envelope["metadata"].is_object()
            || !self.envelope["content"].is_object()
        {
            return Err(Error::KernelProtocol("无效的 Jupyter 消息结构或通道"));
        }
        Ok(())
    }
}
pub fn decode_text(text: &str) -> Result<KernelMessage> {
    if text.len() > MAX_MESSAGE_BYTES {
        return Err(Error::KernelProtocol("消息超过 8 MiB"));
    }
    let envelope =
        serde_json::from_str(text).map_err(|_| Error::KernelProtocol("无效的 Jupyter JSON"))?;
    let message = KernelMessage {
        envelope,
        buffers: Vec::new(),
    };
    message.validate()?;
    Ok(message)
}
pub fn decode_binary(bytes: &[u8]) -> Result<KernelMessage> {
    if bytes.len() < 4 || bytes.len() > MAX_MESSAGE_BYTES {
        return Err(Error::KernelProtocol("无效的二进制帧大小"));
    }
    let count = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    if !(2..=MAX_SEGMENTS).contains(&count) {
        return Err(Error::KernelProtocol("无效的二进制分段数量"));
    }
    let header_size = 4 * (count + 1);
    if bytes.len() < header_size {
        return Err(Error::KernelProtocol("二进制 offset 表被截断"));
    }
    let mut offsets = Vec::with_capacity(count + 1);
    for part in bytes[4..header_size].chunks_exact(4) {
        offsets.push(u32::from_be_bytes(part.try_into().unwrap()) as usize);
    }
    if offsets[0] != header_size
        || offsets.iter().any(|offset| *offset > bytes.len())
        || offsets.windows(2).any(|p| p[0] > p[1])
    {
        return Err(Error::KernelProtocol("二进制 offset 越界或乱序"));
    }
    offsets.push(bytes.len());
    let text = std::str::from_utf8(&bytes[offsets[0]..offsets[1]])
        .map_err(|_| Error::KernelProtocol("JSON 分段不是 UTF-8"))?;
    let mut message = decode_text(text)?;
    message.buffers = offsets[1..]
        .windows(2)
        .map(|pair| bytes[pair[0]..pair[1]].to_vec())
        .collect();
    Ok(message)
}
pub fn encode(message: &KernelMessage) -> Result<Frame> {
    message.validate()?;
    let mut envelope = message.envelope.clone();
    envelope.as_object_mut().unwrap().remove("buffers");
    let text = serde_json::to_string(&envelope)
        .map_err(|_| Error::KernelProtocol("Jupyter JSON 编码失败"))?;
    if message.buffers.is_empty() {
        if text.len() > MAX_MESSAGE_BYTES {
            return Err(Error::KernelProtocol("消息超过 8 MiB"));
        }
        return Ok(Frame::Text(text));
    }
    let count = message.buffers.len() + 1;
    if count > MAX_SEGMENTS {
        return Err(Error::KernelProtocol("buffer 数量超过上限"));
    }
    let length = message
        .buffers
        .iter()
        .try_fold(text.len() + 4 * (count + 1), |size, buffer| {
            size.checked_add(buffer.len())
        })
        .filter(|size| *size <= MAX_MESSAGE_BYTES)
        .ok_or(Error::KernelProtocol("消息超过 8 MiB"))?;
    let mut bytes = Vec::with_capacity(length);
    bytes.extend_from_slice(&(count as u32).to_be_bytes());
    let mut offset = 4 * (count + 1);
    bytes.extend_from_slice(&(offset as u32).to_be_bytes());
    offset += text.len();
    for buffer in &message.buffers {
        bytes.extend_from_slice(&(offset as u32).to_be_bytes());
        offset += buffer.len();
    }
    bytes.extend_from_slice(text.as_bytes());
    for buffer in &message.buffers {
        bytes.extend_from_slice(buffer);
    }
    Ok(Frame::Binary(bytes))
}
pub fn message_id() -> String {
    crate::colab_api::notebook_identity()[..36].replace('_', "-")
}

/// jupyter_client.session.new_id: 16 random bytes rendered as 8 + 24 hex characters.
pub fn session_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}-{}", &hex[..8], &hex[8..])
}
