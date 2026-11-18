//! Build a remote installer without treating package names or paths as Python source.
use crate::{Error, Result, contents, execution::Document};
use std::path::Path;

// The probe executes this exact template with a fake subprocess module, never pip/uv.
const INSTALL_TEMPLATE: &str = r#"import json, subprocess, sys
def install():
    packages = json.loads(__COLAB_INSTALL_ARGUMENTS_JSON__)
    try:
        subprocess.check_call(['uv', 'pip', 'install', '--system'] + packages)
        print('Installation Complete (via uv)!')
    except Exception:
        subprocess.check_call([sys.executable, '-m', 'pip', 'install'] + packages)
        print('Installation Complete (via pip)!')
install()
"#;

pub fn prepare(packages: &[String], requirement: Option<&Path>) -> Result<Document> {
    if packages.is_empty() && requirement.is_none() {
        return Err(Error::KernelProtocol("未指定包或 requirements 文件"));
    }
    if packages
        .iter()
        .any(|argument| argument.is_empty() || argument.contains('\0'))
    {
        return Err(Error::KernelProtocol("安装参数不能为空或包含 NUL"));
    }
    let mut arguments = Vec::new();
    let upload = if let Some(path) = requirement {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(Error::KernelProtocol("requirements 文件名必须为 UTF-8"))?;
        let remote = format!("content/{name}");
        contents::validate_remote_path(&remote, true)?;
        let bytes = contents::read_local_file(path)?;
        arguments.extend(["-r".to_owned(), format!("/{remote}")]);
        Some((remote, bytes))
    } else {
        None
    };
    arguments.extend_from_slice(packages);
    let arguments_json =
        serde_json::to_string(&arguments).map_err(|_| Error::KernelProtocol("安装参数编码失败"))?;
    let literal = serde_json::to_string(&arguments_json)
        .map_err(|_| Error::KernelProtocol("安装参数编码失败"))?;
    // Single pass replacement; marker text inside a package remains ordinary data.
    let payload = INSTALL_TEMPLATE.replace("__COLAB_INSTALL_ARGUMENTS_JSON__", &literal);
    Document::installation(&payload, upload)
}
