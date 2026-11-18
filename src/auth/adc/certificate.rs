//! X.509 federation reads one bounded snapshot for subject and TLS identity.
use crate::{Error, Result, models::Secret};
use base64::{Engine, engine::general_purpose::STANDARD};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use serde::Deserialize;
use serde_json::Value;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

#[derive(Debug, Clone, Default)]
pub struct CertificateOptions {
    pub default_config: Option<PathBuf>,
    pub fallback_config: Option<PathBuf>,
    pub home: Option<PathBuf>,
}
impl CertificateOptions {
    pub fn capture() -> Self {
        Self::from_values(|key| {
            std::env::var_os(key)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
    }
    fn from_values(mut path: impl FnMut(&str) -> Option<PathBuf>) -> Self {
        let home = path("HOME");
        let directory = path("CLOUDSDK_CONFIG").or_else(|| {
            if cfg!(windows) {
                path("APPDATA").map(|p| p.join("gcloud"))
            } else {
                home.as_ref().map(|p| p.join(".config/gcloud"))
            }
        });
        let fallback_config = directory.map(|p| p.join("certificate_config.json"));
        Self {
            default_config: path("GOOGLE_API_CERTIFICATE_CONFIG")
                .or_else(|| path("CLOUDSDK_CONTEXT_AWARE_CERTIFICATE_CONFIG_FILE_PATH"))
                .or_else(|| fallback_config.clone()),
            fallback_config,
            home,
        }
    }
    fn expand(&self, path: &Path) -> Result<PathBuf> {
        if path == Path::new("~") {
            return self
                .home
                .clone()
                .ok_or(Error::OAuth("证书配置缺少 home 目录"));
        }
        if let Ok(tail) = path.strip_prefix("~") {
            return self
                .home
                .as_ref()
                .map(|p| p.join(tail))
                .ok_or(Error::OAuth("证书配置缺少 home 目录"));
        }
        Ok(path.to_owned())
    }
}

pub(super) struct Certificate {
    config: PathBuf,
    fallback: Option<PathBuf>,
    trust_chain: Option<PathBuf>,
}
pub(super) struct Material {
    pub identity: reqwest::Identity,
    pub subject: Secret,
}
#[derive(Deserialize)]
struct Source {
    certificate_config_location: Option<PathBuf>,
    #[serde(default)]
    use_default_certificate_config: bool,
    trust_chain_path: Option<PathBuf>,
}
#[derive(Deserialize)]
struct Config {
    cert_configs: Configs,
}
#[derive(Deserialize)]
struct Configs {
    workload: Option<Workload>,
}
#[derive(Deserialize)]
struct Workload {
    cert_path: PathBuf,
    key_path: PathBuf,
}

pub(super) fn read(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|_| Error::OAuth("无法读取证书配置或密钥文件"))?;
    if !file
        .metadata()
        .map_err(|_| Error::OAuth("无法检查证书文件"))?
        .is_file()
    {
        return Err(Error::OAuth("证书配置与密钥必须为普通文件"));
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::OAuth("无法读取证书配置或密钥文件"))?;
    if bytes.is_empty() || bytes.len() > 1024 * 1024 {
        return Err(Error::OAuth("证书文件为空或超过 1 MiB"));
    }
    Ok(bytes)
}
pub(super) fn certificates(bytes: &[u8]) -> Result<Vec<CertificateDer<'static>>> {
    let chain = CertificateDer::pem_slice_iter(bytes)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| Error::OAuth("证书 PEM 无效"))?;
    if chain.is_empty() || chain.len() > 16 {
        return Err(Error::OAuth("证书链为空或过长"));
    }
    for cert in &chain {
        rustls::server::ParsedCertificate::try_from(cert)
            .map_err(|_| Error::OAuth("X.509 证书 DER 无效"))?;
    }
    Ok(chain)
}
impl Certificate {
    pub(super) fn load(value: Value, options: &CertificateOptions) -> Result<Self> {
        let source: Source =
            serde_json::from_value(value).map_err(|_| Error::OAuth("certificate 来源配置无效"))?;
        let explicit = source
            .certificate_config_location
            .filter(|p| !p.as_os_str().is_empty());
        if explicit.is_some() == source.use_default_certificate_config {
            return Err(Error::OAuth("证书配置必须选择显式位置或默认配置之一"));
        }
        let fallback = if explicit.is_none() {
            options.fallback_config.clone()
        } else {
            None
        };
        let config = explicit
            .or_else(|| options.default_config.clone())
            .ok_or(Error::OAuth("缺少默认证书配置位置"))?;
        Ok(Self {
            config: options.expand(&config)?,
            fallback: fallback.map(|p| options.expand(&p)).transpose()?,
            trust_chain: source
                .trust_chain_path
                .filter(|p| !p.as_os_str().is_empty()),
        })
    }
    pub(super) fn material(&self) -> Result<Material> {
        let config: Config = serde_json::from_slice(&read(&self.config)?)
            .map_err(|_| Error::OAuth("证书配置缺少有效 workload 路径"))?;
        let workload = config
            .cert_configs
            .workload
            .or_else(|| {
                let path = self.fallback.as_ref().filter(|p| *p != &self.config)?;
                let config: Config = serde_json::from_slice(&read(path).ok()?).ok()?;
                config.cert_configs.workload
            })
            .ok_or(Error::OAuth("证书配置缺少有效 workload 路径"))?;
        if workload.cert_path.as_os_str().is_empty() || workload.key_path.as_os_str().is_empty() {
            return Err(Error::OAuth("证书配置缺少有效 workload 路径"));
        }
        let (identity, chain) = read_identity(&workload.cert_path, &workload.key_path)?;
        let leaf = chain[0].clone();
        let mut subject = vec![STANDARD.encode(&leaf)];
        if let Some(path) = &self.trust_chain {
            let trust = certificates(&read(path)?)?;
            for (i, cert) in trust.iter().enumerate() {
                if cert == &leaf {
                    if i == 0 {
                        continue;
                    }
                    return Err(Error::OAuth("叶证书只能位于证书链首位"));
                }
                subject.push(STANDARD.encode(cert));
            }
        }
        Ok(Material {
            identity,
            subject: Secret::new(
                serde_json::to_string(&subject).map_err(|_| Error::OAuth("无法编码证书主体"))?,
            ),
        })
    }
}

pub(super) fn read_identity(
    cert_path: &Path,
    key_path: &Path,
) -> Result<(reqwest::Identity, Vec<CertificateDer<'static>>)> {
    let mut pem = read(cert_path)?;
    let key_bytes = if cert_path == key_path {
        pem.clone()
    } else {
        read(key_path)?
    };
    let chain = certificates(&pem)?;
    let key = PrivateKeyDer::from_pem_slice(&key_bytes)
        .map_err(|_| Error::OAuth("证书私钥 PEM 无效或已加密"))?;
    // Rebuild from parsed blocks: a projected/combined file may include
    // both certificate and key even when its two paths differ.
    pem.clear();
    for cert in &chain {
        pem.extend_from_slice(
            format!(
                "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
                STANDARD.encode(cert)
            )
            .as_bytes(),
        );
    }
    let label = match &key {
        PrivateKeyDer::Pkcs1(_) => "RSA PRIVATE KEY",
        PrivateKeyDer::Sec1(_) => "EC PRIVATE KEY",
        PrivateKeyDer::Pkcs8(_) => "PRIVATE KEY",
        _ => return Err(Error::OAuth("证书私钥格式不受支持")),
    };
    let encoded_key = Zeroizing::new(STANDARD.encode(key.secret_der()));
    pem.extend_from_slice(format!("-----BEGIN {label}-----\n").as_bytes());
    pem.extend_from_slice(encoded_key.as_bytes());
    pem.extend_from_slice(format!("\n-----END {label}-----\n").as_bytes());
    rustls::sign::CertifiedKey::from_der(
        chain.clone(),
        key,
        &rustls::crypto::ring::default_provider(),
    )
    .map_err(|_| Error::OAuth("证书与私钥不匹配或无效"))?;
    let identity =
        reqwest::Identity::from_pem(&pem).map_err(|_| Error::OAuth("无法创建证书 TLS 身份"))?;
    Ok((identity, chain))
}

#[cfg(test)]
mod tests;
