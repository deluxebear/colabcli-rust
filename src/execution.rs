//! Execute Python and notebooks without replaying code after an uncertain result.
use crate::{
    Error, Result, contents,
    error::io,
    jupyter::{
        ExecutionResult, ExecutionStatus, ExecutionTimeout, KernelRest, KernelRuntime, codec,
        output::{Output, OutputChange},
    },
    sessions::SessionService,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

pub struct Document {
    notebook: Option<Value>,
    pub output_path: Option<PathBuf>,
    cells: Vec<(Option<usize>, String)>,
    setup: String,
    source_label: String,
    history_code: Option<String>,
    via: Option<&'static str>,
}
impl Document {
    pub fn from_file(path: &Path, environment: &[String]) -> Result<Self> {
        let bytes = contents::read_local_file(path)?;
        let source =
            String::from_utf8(bytes).map_err(|_| Error::KernelProtocol("输入文件必须为 UTF-8"))?;
        let setup = environment_setup(environment)?;
        if path.extension().and_then(|s| s.to_str()) != Some("ipynb") {
            if path.extension().and_then(|s| s.to_str()) != Some("py") {
                return Err(Error::KernelProtocol("执行文件需为 .py 或 .ipynb"));
            }
            return Self {
                notebook: None,
                output_path: None,
                cells: vec![(None, source)],
                setup,
                source_label: path.to_string_lossy().into_owned(),
                history_code: None,
                via: None,
            }
            .validated();
        }
        let mut notebook: Value = serde_json::from_str(&source)
            .map_err(|_| Error::KernelProtocol("无效的 notebook JSON"))?;
        if notebook
            .get("metadata")
            .is_some_and(|value| !value.is_object())
        {
            return Err(Error::KernelProtocol("notebook metadata 必须为对象"));
        }
        if notebook["nbformat"] != 4 {
            return Err(Error::KernelProtocol("仅支持 nbformat 4 notebook"));
        }
        let all_cells = notebook["cells"]
            .as_array_mut()
            .ok_or(Error::KernelProtocol("notebook cells 必须为数组"))?;
        let mut cells = Vec::new();
        for (index, cell) in all_cells.iter_mut().enumerate() {
            if !cell.is_object() {
                return Err(Error::KernelProtocol("notebook cell 必须为对象"));
            }
            if cell.get("metadata").is_some_and(|value| !value.is_object()) {
                return Err(Error::KernelProtocol("cell metadata 必须为对象"));
            }
            if cell.get("id").is_some_and(|value| !value.is_string()) {
                return Err(Error::KernelProtocol("cell ID 必须为字符串"));
            }
            if cell.get("id").is_none() || cell["id"] == "" {
                cell["id"] = json!(codec::message_id());
            }
            match cell["cell_type"].as_str() {
                Some("code") => {
                    let source = cell_source(&cell["source"])?;
                    cells.push((Some(index), source));
                }
                Some("markdown" | "raw") => {}
                _ => return Err(Error::KernelProtocol("未知 notebook cell_type")),
            }
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or(Error::KernelProtocol("notebook 文件名无效"))?;
        let output_path = path.with_file_name(format!("{stem}_output.ipynb"));
        contents::validate_download_target(&output_path)?;
        Self {
            notebook: Some(notebook),
            output_path: Some(output_path),
            cells,
            setup,
            source_label: path.to_string_lossy().into_owned(),
            history_code: None,
            via: None,
        }
        .validated()
    }
    pub fn from_stdin(input: impl Read, environment: &[String]) -> Result<Self> {
        let mut bytes = Vec::new();
        input
            .take(contents::MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| io("stdin", e))?;
        if bytes.len() as u64 > contents::MAX_FILE_BYTES {
            return Err(Error::FileTooLarge(contents::MAX_FILE_BYTES));
        }
        let code =
            String::from_utf8(bytes).map_err(|_| Error::KernelProtocol("stdin 必须为 UTF-8"))?;
        Self {
            notebook: None,
            output_path: None,
            cells: vec![(None, code)],
            setup: environment_setup(environment)?,
            source_label: "stdin".into(),
            history_code: None,
            via: None,
        }
        .validated()
    }
    fn validated(self) -> Result<Self> {
        for code in std::iter::once(self.setup.as_str())
            .chain(self.cells.iter().map(|(_, source)| source.as_str()))
        {
            codec::encode(&codec::KernelMessage::new(
                "execute_request",
                "shell",
                "preflight",
                json!({"code":code,"silent":false,"store_history":true,"user_expressions":{},"allow_stdin":false,"stop_on_error":true}),
            ))?;
        }
        Ok(self)
    }
    pub(crate) fn script(
        payload: String,
        original: String,
        label: String,
        environment: &[String],
    ) -> Result<Self> {
        Self {
            notebook: None,
            output_path: None,
            cells: vec![(None, payload)],
            setup: environment_setup(environment)?,
            source_label: label,
            history_code: Some(original),
            via: Some("run"),
        }
        .validated()
    }
    pub fn has_code(&self) -> bool {
        self.cells.iter().any(|(_, code)| !code.trim().is_empty())
    }
    pub fn save(&self) -> Result<()> {
        if let (Some(notebook), Some(path)) = (&self.notebook, &self.output_path) {
            let bytes = serde_json::to_vec_pretty(notebook)
                .map_err(|_| Error::KernelProtocol("notebook 编码失败"))?;
            contents::write_atomic(path, &bytes)?;
        }
        Ok(())
    }
    fn record_cell(&mut self, index: Option<usize>, result: &ExecutionResult) -> Result<()> {
        if let Some(index) = index {
            let cell = &mut self.notebook.as_mut().unwrap()["cells"][index];
            use base64::{Engine, engine::general_purpose::STANDARD};
            let mut buffers = serde_json::Map::new();
            let mut outputs = Vec::new();
            for (index, output) in result.outputs.iter().enumerate() {
                let mut data = output.data.clone();
                data.remove("transient");
                if !output.buffers.is_empty() {
                    buffers.insert(
                        index.to_string(),
                        json!(
                            output
                                .buffers
                                .iter()
                                .map(|buffer| STANDARD.encode(buffer))
                                .collect::<Vec<_>>()
                        ),
                    );
                }
                outputs.push(Value::Object(data));
            }
            cell["outputs"] = Value::Array(outputs);
            if !buffers.is_empty() {
                cell["metadata"]["colab_cli_output_buffers"] = Value::Object(buffers);
            } else if let Some(metadata) = cell.get_mut("metadata").and_then(Value::as_object_mut) {
                metadata.remove("colab_cli_output_buffers");
            }
            cell["execution_count"] = result
                .reply
                .as_ref()
                .and_then(|r| r.get("execution_count"))
                .cloned()
                .unwrap_or(Value::Null);
            if result.status == ExecutionStatus::Unknown {
                cell["metadata"]["colab_cli_execution"] =
                    json!({"status":"unknown","msg_id":result.msg_id,"error":result.error});
            } else if let Some(metadata) = cell.get_mut("metadata").and_then(Value::as_object_mut) {
                metadata.remove("colab_cli_execution");
            }
        }
        Ok(())
    }
}
fn cell_source(source: &Value) -> Result<String> {
    match source {
        Value::String(source) => Ok(source.clone()),
        Value::Array(lines) => lines
            .iter()
            .map(|line| {
                line.as_str()
                    .ok_or(Error::KernelProtocol("cell source 数组必须为字符串"))
            })
            .collect::<Result<Vec<_>>>()
            .map(|lines| lines.concat()),
        _ => Err(Error::KernelProtocol(
            "cell source 必须为字符串或字符串数组",
        )),
    }
}
fn environment_setup(environment: &[String]) -> Result<String> {
    let mut variables = BTreeMap::new();
    for binding in environment {
        let (key, value) = binding
            .split_once('=')
            .ok_or(Error::KernelProtocol("--env 需要 KEY=VALUE"))?;
        if key.is_empty()
            || !key.bytes().enumerate().all(|(index, b)| {
                b == b'_' || b.is_ascii_alphabetic() || (index > 0 && b.is_ascii_digit())
            })
            || value.contains('\0')
        {
            return Err(Error::KernelProtocol("无效的环境变量名称或值"));
        }
        variables.insert(key, value);
    }
    let encoded =
        serde_json::to_string(&variables).map_err(|_| Error::KernelProtocol("环境变量编码失败"))?;
    let literal =
        serde_json::to_string(&encoded).map_err(|_| Error::KernelProtocol("环境变量编码失败"))?;
    Ok(format!(
        "__import__('os').chdir('/content')\n__import__('os').environ.update(__import__('json').loads({literal}))\n"
    ))
}

pub struct ExecuteOptions<'a> {
    pub timeout: ExecutionTimeout,
    pub http_timeout: Duration,
    /// Explicit recovery/adoption; always verified against the selected VM's REST endpoint.
    pub kernel_id: Option<&'a str>,
    pub cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
}
pub fn execute_document(
    service: &SessionService,
    name: Option<&str>,
    document: &mut Document,
    options: ExecuteOptions<'_>,
    mut output_hook: impl FnMut(&OutputChange) -> Result<()>,
) -> Result<Vec<ExecutionResult>> {
    if !document.has_code() {
        document.save()?;
        return Ok(Vec::new());
    }
    let result = service.with_execution_session(name, |original, checkpoint| {
        let mut state = original.clone();
        let rest = KernelRest::new(&state, options.http_timeout)?;
        if let Some(id) = options.kernel_id {
            rest.get(id)?;
            state.kernel_id = Some(id.into());
            state.extra.remove("kernel_start_pending");
            // Selecting an existing kernel does not prove an older execution finished.
        }
        if let Some(running) = &state.running {
            return Err(Error::ExecutionPending(running.clone()));
        }
        if state.extra.get("kernel_restart_pending") == Some(&json!(true)) {
            return Err(Error::KernelRestartPending);
        }
        if let Some(id) = &state.kernel_id {
            if options.kernel_id.is_none() {
                rest.get(id)?;
            }
        } else {
            if state.extra.get("kernel_start_pending") == Some(&json!(true)) {
                return Err(Error::KernelStartPending);
            }
            state
                .extra
                .insert("kernel_start_pending".into(), json!(true));
            checkpoint(state.clone())?;
            let model = rest.create()?;
            state.kernel_id = Some(model.id);
            state.extra.remove("kernel_start_pending");
            checkpoint(state.clone())?;
        }
        if state.session_id.is_none() {
            state.session_id = Some(codec::message_id());
        }
        checkpoint(state.clone())?;
        let mut runtime = KernelRuntime::connect(
            &rest,
            state.kernel_id.as_deref().unwrap(),
            state.session_id.as_deref().unwrap(),
        )?;
        runtime.set_cancellation(options.cancellation.clone());
        // Setup is a separate request so future imports and the user's final expression retain Python semantics.
        let setup = runtime.execute_with_checkpoint(
            &document.setup,
            options.timeout,
            |msg_id| {
                state.running = Some(msg_id.into());
                checkpoint(state.clone())
            },
            &mut output_hook,
        )?;
        if setup.status != ExecutionStatus::Unknown {
            state.running = None;
        }
        checkpoint(state.clone())?;
        if !setup.succeeded() {
            return Ok(vec![setup]);
        }
        let mut results = Vec::new();
        for (index, source) in document.cells.clone() {
            if source.trim().is_empty() {
                continue;
            }
            let mut result = runtime.execute_with_checkpoint(
                &source,
                options.timeout,
                |msg_id| {
                    state.running = Some(msg_id.into());
                    state.last_execution = Some((
                        document.source_label.clone(),
                        index.and_then(|i| {
                            document
                                .notebook
                                .as_ref()
                                .and_then(|n| n["cells"][i]["id"].as_str())
                                .map(str::to_owned)
                        }),
                        chrono::Utc::now().to_rfc3339(),
                    ));
                    checkpoint(state.clone())
                },
                &mut output_hook,
            )?;
            if result.status != ExecutionStatus::Unknown {
                state.running = None;
            }
            document.record_cell(index, &result)?;
            let history_outputs:Vec<_>=result.outputs.iter().filter(|output|document.via!=Some("run") || !output.data.get("data").is_some_and(|data|data.get("application/vnd.colab-cli.run-result+json").is_some())).collect();
            if let Err(error)=service.record_event(&state.name,Some(&state.endpoint),"execution",json!({"code":document.history_code.as_deref().unwrap_or(&source),"outputs":history_outputs,
                "status":result.status,"msg_id":result.msg_id,"error":result.error,"source_file":document.source_label,"cell_index":index,
                "execution_count":result.reply.as_ref().and_then(|reply|reply.get("execution_count")),"via":document.via}),Some(&result.msg_id)) {
                result.history_error=Some(error.to_string());
            }
            checkpoint(state.clone())?;
            let stop = !result.succeeded();
            results.push(result);
            if stop {
                break;
            }
        }
        Ok(results)
    });
    // Also persist completed notebook cells if a later request/connection fails.
    let saved = document.save();
    match result {
        Ok(results) => {
            saved?;
            Ok(results)
        }
        Err(error) => {
            saved?;
            Err(error)
        }
    }
}

pub fn restart_kernel(
    service: &SessionService,
    name: Option<&str>,
    http_timeout: Duration,
) -> Result<()> {
    service.with_session_checkpoints(name, |original, checkpoint| {
        let mut state = original.clone();
        let rest = KernelRest::new(&state, http_timeout)?;
        let id = state
            .kernel_id
            .clone()
            .ok_or(Error::KernelProtocol("会话尚未记录 kernel ID"))?;
        state
            .extra
            .insert("kernel_generation".into(), json!(codec::message_id()));
        state
            .extra
            .insert("kernel_restart_pending".into(), json!(true));
        checkpoint(state.clone())?;
        rest.restart(&id)?;
        state.running = None;
        state.extra.remove("kernel_restart_pending");
        checkpoint(state)
    })
}
pub fn render_output(change: &OutputChange) -> Result<()> {
    use std::io::Write;
    let output = match change {
        OutputChange::Append(output) | OutputChange::Update { output, .. } => output,
        OutputChange::Clear => return Ok(()),
    };
    let data = &output.data;
    if data.get("output_type") == Some(&json!("stream")) {
        let text = data.get("text").and_then(Value::as_str).unwrap_or("");
        if data.get("name") == Some(&json!("stderr")) {
            std::io::stderr()
                .write_all(text.as_bytes())
                .and_then(|_| std::io::stderr().flush())
                .map_err(|e| io("stderr", e))?;
        } else {
            std::io::stdout()
                .write_all(text.as_bytes())
                .and_then(|_| std::io::stdout().flush())
                .map_err(|e| io("stdout", e))?;
        }
    } else if data.get("output_type") == Some(&json!("error")) {
        let text = data
            .get("traceback")
            .and_then(Value::as_array)
            .map(|lines| {
                lines
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                format!(
                    "{}: {}",
                    data.get("ename").and_then(Value::as_str).unwrap_or("Error"),
                    data.get("evalue").and_then(Value::as_str).unwrap_or("")
                )
            });
        writeln!(std::io::stderr(), "{text}").map_err(|e| io("stderr", e))?;
    } else if let Some(text) = data.get("data").and_then(|d| d["text/plain"].as_str()) {
        writeln!(std::io::stdout(), "{text}").map_err(|e| io("stdout", e))?;
    }
    Ok(())
}

/// Save PNG/JPEG bundles as standalone artifacts; never interpret MIME data as a local path.
pub fn save_images(outputs: &[Output], directory: &Path) -> Result<Vec<PathBuf>> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    if !directory.is_dir() {
        return Err(Error::KernelProtocol("图片输出目录必须存在"));
    }
    let mut paths = Vec::new();
    for output in outputs {
        for (mime, ext) in [("image/png", "png"), ("image/jpeg", "jpg")] {
            if let Some(encoded) = output.data.get("data").and_then(|d| d[mime].as_str()) {
                let bytes = STANDARD
                    .decode(encoded)
                    .map_err(|_| Error::KernelProtocol("图片 base64 无效"))?;
                let path = directory.join(format!("colab-{}.{ext}", codec::message_id()));
                contents::write_atomic(&path, &bytes)?;
                paths.push(path);
            }
        }
    }
    Ok(paths)
}

pub fn save_last_image(results: &[ExecutionResult], path: &Path) -> Result<()> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let last = results
        .iter()
        .flat_map(|r| &r.outputs)
        .filter_map(|output| output.data.get("data"))
        .filter_map(|data| {
            data["image/png"]
                .as_str()
                .or_else(|| data["image/jpeg"].as_str())
        })
        .next_back();
    if let Some(encoded) = last {
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| Error::KernelProtocol("图片 base64 无效"))?;
        contents::write_atomic(path, &bytes)?;
    }
    Ok(())
}
