//! Execute Python and notebooks without replaying code after an uncertain result.
mod automation_kernel;
pub mod display;
pub mod images;
pub mod repl;
use crate::{
    Error, Result, contents,
    error::io,
    jupyter::{
        ExecutionResult, ExecutionStatus, ExecutionTimeout, KernelRest, KernelRuntime, codec,
        output::{Output, OutputChange},
    },
    sessions::SessionService,
};
pub use automation_kernel::cleanup as automation_cleanup;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
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
    compatible: bool,
    install_upload: Option<(String, Vec<u8>)>,
}
impl Document {
    pub fn for_repl_stdin(reader: impl Read) -> Result<Self> {
        let mut document = Self::from_stdin(reader, &[])?;
        document.via = Some("repl");
        document.setup =
            "import os; os.makedirs('/content', exist_ok=True); os.chdir('/content')".into();
        document.compatible = true;
        Ok(document)
    }
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
                compatible: false,
                install_upload: None,
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
        // Cell IDs are part of the nbformat 4.5 schema. Keep newer minors.
        let minor = match notebook.get("nbformat_minor") {
            Some(value) => value
                .as_u64()
                .ok_or(Error::KernelProtocol("notebook 次版本必须为非负整数"))?,
            None => 5,
        };
        notebook["nbformat_minor"] = json!(minor.max(5));
        let all_cells = notebook["cells"]
            .as_array_mut()
            .ok_or(Error::KernelProtocol("notebook cells 必须为数组"))?;
        let mut cells = Vec::new();
        let mut ids = HashSet::new();
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
            let id_is_unique = cell["id"]
                .as_str()
                .filter(|id| valid_cell_id(id))
                .is_some_and(|id| ids.insert(id.to_owned()));
            if !id_is_unique {
                let id = loop {
                    let id = codec::message_id();
                    if ids.insert(id.clone()) {
                        break id;
                    }
                };
                cell["id"] = json!(id);
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
            compatible: false,
            install_upload: None,
        }
        .validated()
    }
    pub(crate) fn installation(code: &str, upload: Option<(String, Vec<u8>)>) -> Result<Self> {
        let mut document = Self::automation(code, "install")?;
        document.install_upload = upload;
        // Package arguments may contain private index credentials. Do not persist the wrapper.
        document.history_code = Some("# install: argument values omitted".into());
        Ok(document)
    }
    pub fn automation(code: &str, operation: &str) -> Result<Self> {
        if !["auth", "drivemount", "install", "drivefs"].contains(&operation) {
            return Err(Error::KernelProtocol("不支持的自动化操作"));
        }
        let mut document = Self::from_stdin(code.as_bytes(), &[])?;
        document.source_label = format!("automation:{operation}");
        document.via = Some("automation");
        Ok(document)
    }
    pub(crate) fn drivefs(code: &str, operation_id: &str) -> Result<Self> {
        let mut document = Self::automation(code, "drivefs")?;
        document.history_code = Some(format!("# drivefs operation: {operation_id}"));
        Ok(document)
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
            compatible: false,
            install_upload: None,
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
            compatible: false,
            install_upload: None,
        }
        .validated()
    }
    pub fn use_official_output(&mut self) {
        self.compatible = true;
    }
    fn cell_id(&self, index: Option<usize>) -> Option<&str> {
        self.notebook
            .as_ref()?
            .get("cells")?
            .get(index?)?
            .get("id")?
            .as_str()
    }
    pub fn has_code(&self) -> bool {
        self.cells.iter().any(|(_, code)| !code.trim().is_empty())
    }
    pub fn save(&self) -> Result<()> {
        if let (Some(notebook), Some(path)) = (&self.notebook, &self.output_path) {
            let mut notebook = notebook.clone();
            if self.compatible {
                crate::cli_compat::notebook_lines(&mut notebook);
            }
            let bytes = serde_json::to_vec_pretty(&notebook)
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
            if !self.compatible {
                cell["execution_count"] = result
                    .reply
                    .as_ref()
                    .and_then(|r| r.get("execution_count"))
                    .cloned()
                    .unwrap_or(Value::Null);
            }
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
fn valid_cell_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn notebook_title(source: &str) -> Option<&str> {
    // Match the fixed upstream TITLE_REGEX: ^\s*#\s*@title\s+(.*).
    fn space(ch: char) -> bool {
        ch.is_whitespace() || matches!(ch, '\u{1c}'..='\u{1f}')
    }
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        offset += line.len();
        let Some(comment) = line.trim_start_matches(space).strip_prefix('#') else {
            continue;
        };
        // Start just after # in the full source because upstream whitespace
        // can span physical lines.
        let rest = &source[offset - comment.len()..];
        let Some(rest) = rest.trim_start_matches(space).strip_prefix("@title") else {
            continue;
        };
        if !rest.starts_with(space) {
            continue;
        }
        return Some(
            rest.trim_start_matches(space)
                .split('\n')
                .next()
                .unwrap_or_default()
                .trim_matches(space),
        );
    }
    None
}

#[cfg(test)]
#[test]
fn notebook_titles_follow_the_upstream_directive_pattern() {
    for (source, expected) in [
        ("  #\t@title   中文标题  \r\nprint(1)", Some("中文标题")),
        ("#@title 紧凑标题", Some("紧凑标题")),
        ("# @titleSuffix invalid\n# @title Valid", Some("Valid")),
        ("#\n  @title\n  Other line", Some("Other line")),
        ("# @title ", Some("")),
        ("# @title", None),
        ("print(1) # @title Inline", None),
    ] {
        assert_eq!(notebook_title(source), expected, "{source:?}");
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
    output_hook: impl FnMut(&OutputChange) -> Result<()>,
) -> Result<Vec<ExecutionResult>> {
    execute_document_with_input(service, name, document, options, output_hook, None)
}

pub fn execute_document_with_input(
    service: &SessionService,
    name: Option<&str>,
    document: &mut Document,
    options: ExecuteOptions<'_>,
    mut output_hook: impl FnMut(&OutputChange) -> Result<()>,
    mut input: Option<&mut dyn crate::jupyter::InputHandler>,
) -> Result<Vec<ExecutionResult>> {
    if !document.has_code() && document.via != Some("repl") {
        document.save()?;
        return Ok(Vec::new());
    }
    if document.via == Some("automation") {
        return automation_kernel::execute(service, name, document, options, output_hook, input);
    }
    let result = service.with_execution_session(name, |original, checkpoint| {
        let mut state = original.clone();
        if state.extra.contains_key("automation_pending") {return Err(Error::AutomationPending);}
        if let Some(handler) = input.as_deref_mut() { handler.bind_endpoint(&state.endpoint)?; }
        let mut runtime = open_runtime(&mut state, &options, checkpoint)?;
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
        if setup.status != ExecutionStatus::Unknown { state.running = None; }
        else if document.compatible {
            if let Some(id) = state.running.take() { state.extra.insert("execution_pending".into(), json!(id)); }
        }
        checkpoint(state.clone())?;
        if !setup.succeeded() {
            return Ok(vec![setup]);
        }
        let mut results = Vec::new();
        let cell_count = document.cells.len();
        for (position, (index, source)) in document.cells.clone().into_iter().enumerate() {
            if source.trim().is_empty() {
                continue;
            }
            let cell_id = document.cell_id(index).map(str::to_owned);
            let identifier = cell_id.as_ref().map(|id| notebook_title(&source).unwrap_or(id));
            if document.compatible && index.is_some() {
                let suffix = identifier.filter(|title| !title.is_empty()).map(|title| format!(" - {title}")).unwrap_or_default();
                println!("[colab] Executing cell {}/{}{suffix}...", position + 1, cell_count);
            }
            let mut result = runtime.execute_with_input(
                &source,
                options.timeout,
                |msg_id| {
                    state.running = Some(msg_id.into());
                    state.last_execution = Some((
                        document.source_label.clone(),
                        identifier.map(str::to_owned),
                        chrono::Utc::now().to_rfc3339(),
                    ));
                    checkpoint(state.clone())
                },
                &mut output_hook,
                input.as_mut().map(|handler| &mut **handler as &mut dyn crate::jupyter::InputHandler),
            )?;
            if result.status != ExecutionStatus::Unknown { state.running = None; }
            else if document.compatible {
                if let Some(id) = state.running.take() { state.extra.insert("execution_pending".into(), json!(id)); }
            }
            document.record_cell(index, &result)?;
            let history_outputs:Vec<_>=result.outputs.iter().filter(|output|document.via!=Some("run") || !output.data.get("data").is_some_and(|data|data.get("application/vnd.colab-cli.run-result+json").is_some())).collect();
            if let Err(error)=service.record_event(&state.name,Some(&state.endpoint),"execution",json!({"code":document.history_code.as_deref().unwrap_or(&source),"outputs":history_outputs,
                "status":result.status,"msg_id":result.msg_id,"error":result.error,"source_file":document.source_label,"cell_index":index,"cell_id":cell_id,
                "execution_count":result.reply.as_ref().and_then(|reply|reply.get("execution_count")),"via":document.via,"source":if document.via==Some("repl") {Some("piped")} else {None}}),Some(&result.msg_id)) {
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

/// Common kernel adoption/creation and unknown-result guards for exec and REPL.
fn open_runtime(
    state: &mut crate::models::SessionState,
    options: &ExecuteOptions<'_>,
    checkpoint: &dyn Fn(crate::models::SessionState) -> Result<()>,
) -> Result<KernelRuntime> {
    if state.extra.contains_key("automation_pending") {
        return Err(Error::AutomationPending);
    }
    let rest = KernelRest::new(state, options.http_timeout)?;
    if let Some(id) = options.kernel_id {
        rest.get(id)?;
        state.kernel_id = Some(id.into());
        state.extra.remove("kernel_start_pending");
        // Selecting an existing kernel does not prove an older execution finished.
    }
    if let Some(running) = state
        .running
        .as_deref()
        .or_else(|| state.extra.get("execution_pending").and_then(Value::as_str))
    {
        return Err(Error::ExecutionPending(running.to_owned()));
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
        state.session_id = Some(codec::session_id());
    }
    checkpoint(state.clone())?;
    let mut runtime = KernelRuntime::connect(
        &rest,
        state.kernel_id.as_deref().unwrap(),
        state.session_id.as_deref().unwrap(),
    )?;
    runtime.set_cancellation(options.cancellation.clone());
    Ok(runtime)
}

pub fn restart_kernel(
    service: &SessionService,
    name: Option<&str>,
    http_timeout: Duration,
) -> Result<()> {
    restart_kernel_inner(service, name, http_timeout, false)
}

/// The official CLI initializes/reuses the runtime before requesting restart.
pub fn restart_kernel_compatible(
    service: &SessionService,
    name: Option<&str>,
    http_timeout: Duration,
) -> Result<()> {
    restart_kernel_inner(service, name, http_timeout, true)
}

fn restart_kernel_inner(
    service: &SessionService,
    name: Option<&str>,
    http_timeout: Duration,
    connect: bool,
) -> Result<()> {
    service.with_session_checkpoints(name, |original, checkpoint| {
        let mut state = original.clone();
        if state.extra.contains_key("automation_pending") {
            return Err(Error::AutomationPending);
        }
        let rest = KernelRest::new(&state, http_timeout)?;
        let existing = state.kernel_id.is_some();
        if connect && !existing {
            if state.extra.get("kernel_start_pending") == Some(&json!(true)) {
                return Err(Error::KernelStartPending);
            }
            state
                .extra
                .insert("kernel_start_pending".into(), json!(true));
            checkpoint(state.clone())?;
            state.kernel_id = Some(rest.create()?.id);
            state.extra.remove("kernel_start_pending");
            checkpoint(state.clone())?;
        }
        let id = state
            .kernel_id
            .clone()
            .ok_or(Error::KernelProtocol("会话尚未记录 kernel ID"))?;
        let _runtime = if connect {
            if existing {
                rest.get(&id)?;
            }
            let session_id = state
                .session_id
                .get_or_insert_with(codec::session_id)
                .clone();
            checkpoint(state.clone())?;
            Some(KernelRuntime::connect(&rest, &id, &session_id)?)
        } else {
            None
        };
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
        state.extra.remove("execution_pending");
        checkpoint(state)
    })
}
pub fn render_output(change: &OutputChange) -> Result<()> {
    render_output_with_image_target(change, None)
}

pub fn render_output_with_image_target(
    change: &OutputChange,
    image_target: Option<&Path>,
) -> Result<()> {
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
    } else if let Some(text) = data.get("data").and_then(display::text) {
        writeln!(std::io::stdout(), "{text}").map_err(|e| io("stdout", e))?;
    }
    if let Some(bundle) = data.get("data") {
        images::render(bundle, image_target)?;
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
                    .decode(image_base64_bytes(encoded))
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
            .decode(image_base64_bytes(encoded))
            .map_err(|_| Error::KernelProtocol("图片 base64 无效"))?;
        contents::write_atomic(path, &bytes)?;
    }
    Ok(())
}

// Colab/IPython may wrap MIME base64 with ASCII whitespace.
fn image_base64_bytes(encoded: &str) -> Vec<u8> {
    encoded
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect()
}
