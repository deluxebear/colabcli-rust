use super::codec::KernelMessage;
use crate::{Error, Result};
use serde::Serialize;
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Serialize)]
pub struct Output {
    #[serde(flatten)]
    pub data: Map<String, Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub buffers: Vec<Vec<u8>>,
}
#[derive(Debug, Clone)]
pub enum OutputChange {
    Append(Output),
    Update { index: usize, output: Output },
    Clear,
}
#[derive(Default)]
pub struct OutputCollector {
    pub outputs: Vec<Output>,
    deferred_clear: bool,
    bytes: usize,
}
impl OutputCollector {
    fn clear(&mut self, changes: &mut Vec<OutputChange>) {
        self.outputs.clear();
        self.bytes = 0;
        self.deferred_clear = false;
        changes.push(OutputChange::Clear);
    }
    pub fn apply(&mut self, message: &KernelMessage) -> Result<Vec<OutputChange>> {
        let content = message.content();
        let mut changes = Vec::new();
        if message.kind() == "clear_output" {
            if content["wait"].as_bool().unwrap_or(false) {
                self.deferred_clear = true;
            } else {
                self.clear(&mut changes);
            }
            return Ok(changes);
        }
        if message.kind() == "update_display_data" {
            if !content["data"].is_object() {
                return Err(Error::KernelProtocol("无效的 display 更新 MIME bundle"));
            }
            let mut replacements = Vec::new();
            let mut projected = self.bytes;
            if let Some(id) = content["transient"]["display_id"].as_str() {
                for (index, output) in self.outputs.iter().enumerate() {
                    if output
                        .data
                        .get("transient")
                        .and_then(|t| t["display_id"].as_str())
                        == Some(id)
                    {
                        let old_size = encoded_size(output)?;
                        let mut updated = output.clone();
                        updated.data.insert("data".into(), content["data"].clone());
                        updated
                            .data
                            .insert("metadata".into(), content["metadata"].clone());
                        updated.buffers = message.buffers.clone();
                        projected = projected
                            .saturating_sub(old_size)
                            .saturating_add(encoded_size(&updated)?);
                        if projected > 32 * 1024 * 1024 {
                            return Err(Error::KernelProtocol("累计输出超过 32 MiB"));
                        }
                        replacements.push((index, updated));
                    }
                }
            }
            for (index, output) in replacements {
                self.outputs[index] = output.clone();
                changes.push(OutputChange::Update { index, output });
            }
            self.bytes = projected;
            return Ok(changes);
        }
        let data = match message.kind() {
            "stream" => {
                if !content["text"].is_string()
                    || ![Some("stdout"), Some("stderr")].contains(&content["name"].as_str())
                {
                    return Err(Error::KernelProtocol("无效的 stream 输出"));
                }
                json!({"output_type":"stream","name":content["name"],"text":content["text"]})
            }
            "execute_result" | "display_data" => {
                if !content["data"].is_object() {
                    return Err(Error::KernelProtocol("无效的 MIME bundle"));
                }
                let mut data = content.clone();
                if data.get("metadata").is_none() {
                    data["metadata"] = json!({});
                }
                if !data["metadata"].is_object() {
                    return Err(Error::KernelProtocol("输出 metadata 必须为对象"));
                }
                if message.kind() == "execute_result" && data.get("execution_count").is_none() {
                    data["execution_count"] = Value::Null;
                }
                data["output_type"] = json!(message.kind());
                data
            }
            "error" => {
                json!({"output_type":"error","ename":content["ename"],"evalue":content["evalue"],"traceback":content["traceback"]})
            }
            _ => return Ok(changes),
        };
        if self.deferred_clear {
            self.clear(&mut changes);
        }
        let output = Output {
            data: data.as_object().unwrap().clone(),
            buffers: message.buffers.clone(),
        };
        self.bytes = self.bytes.saturating_add(
            serde_json::to_vec(&output)
                .map_err(|_| Error::KernelProtocol("输出编码失败"))?
                .len(),
        );
        if self.bytes > 32 * 1024 * 1024 || self.outputs.len() >= 10000 {
            return Err(Error::KernelProtocol("累计输出超过 32 MiB 或 10000 条"));
        }
        self.outputs.push(output.clone());
        changes.push(OutputChange::Append(output));
        Ok(changes)
    }
    pub fn synthesize_error(&mut self, reply: &Value) -> Result<Vec<OutputChange>> {
        if reply["status"] == "error"
            && !self
                .outputs
                .iter()
                .any(|o| o.data.get("output_type") == Some(&json!("error")))
        {
            let message = KernelMessage::new(
                "error",
                "iopub",
                "",
                json!({"ename":reply.get("ename").cloned().unwrap_or(json!("Error")),
                "evalue":reply.get("evalue").cloned().unwrap_or(json!("Unknown error")),"traceback":reply.get("traceback").cloned().unwrap_or(json!([]))}),
            );
            return self.apply(&message);
        }
        Ok(Vec::new())
    }
}

fn encoded_size(output: &Output) -> Result<usize> {
    serde_json::to_vec(output)
        .map(|bytes| bytes.len())
        .map_err(|_| Error::KernelProtocol("输出编码失败"))
}
