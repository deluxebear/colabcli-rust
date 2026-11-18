//! Quota metadata is attached to the credentials that authorize a request.
use crate::{Error, Result};
use reqwest::header::HeaderValue;
use serde_json::Value;

pub(crate) fn header(project: &str) -> Result<HeaderValue> {
    if project.is_empty()
        || project.len() > 256
        || !project.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(Error::OAuth("ADC quota_project_id 请求头无效"));
    }
    let mut value = HeaderValue::from_str(project)
        .map_err(|_| Error::OAuth("ADC quota_project_id 请求头无效"))?;
    value.set_sensitive(true);
    Ok(value)
}
pub(crate) fn select(document: &Value, override_project: Option<&str>) -> Result<Option<String>> {
    let project = match override_project.filter(|value| !value.is_empty()) {
        Some(value) => Some(value),
        None => match document.get("quota_project_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) if value.is_empty() => None,
            Some(Value::String(value)) => Some(value.as_str()),
            _ => return Err(Error::OAuth("ADC quota_project_id 字段无效")),
        },
    };
    project
        .map(|project| {
            header(project)?;
            Ok(project.to_owned())
        })
        .transpose()
}
