//! Offline static shell completion; installation only follows an explicit option.
use crate::cli_command;
use clap::{Args, ValueEnum};
use colabcli_rust::{Error, Result};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Error {
    Error::Io {
        path: path.into(),
        source,
    }
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Shell {
    Auto,
    Bash,
    Zsh,
    Fish,
    Powershell,
    Pwsh,
}
#[derive(Args)]
pub struct Options {
    /// Print native static completion; omitted shell uses SHELL.
    #[arg(long, value_enum, num_args=0..=1, default_missing_value="auto", conflicts_with="install_completion")]
    show_completion: Option<Shell>,
    /// Install completion into the current user's shell configuration.
    #[arg(long, value_enum, num_args=0..=1, default_missing_value="auto")]
    install_completion: Option<Shell>,
}
pub fn run(matches: &clap::ArgMatches) -> Result<()> {
    let install = matches.get_one::<Shell>("install_completion");
    let shell = *install
        .or(matches.get_one::<Shell>("show_completion"))
        .ok_or(Error::Completion("缺少补全操作"))?;
    let shell = if matches!(shell, Shell::Auto) {
        let path = std::env::var_os("SHELL").ok_or(Error::Completion(
            "无法识别 shell；请显式指定 bash/zsh/fish/powershell/pwsh",
        ))?;
        match Path::new(&path).file_stem().and_then(|s| s.to_str()) {
            Some("bash") => Shell::Bash,
            Some("zsh") => Shell::Zsh,
            Some("fish") => Shell::Fish,
            Some("pwsh") => Shell::Pwsh,
            Some("powershell") => Shell::Powershell,
            _ => return Err(Error::Completion("SHELL 不受支持；请显式指定 shell")),
        }
    } else {
        shell
    };
    let generator = match shell {
        Shell::Bash => clap_complete::Shell::Bash,
        Shell::Zsh => clap_complete::Shell::Zsh,
        Shell::Fish => clap_complete::Shell::Fish,
        Shell::Powershell | Shell::Pwsh => clap_complete::Shell::PowerShell,
        Shell::Auto => unreachable!(),
    };
    let mut script = Vec::new();
    clap_complete::generate(generator, &mut cli_command(), "colab", &mut script);
    if install.is_none() {
        std::io::stdout()
            .lock()
            .write_all(&script)
            .map_err(|e| io("stdout", e))?;
        return Ok(());
    }
    install_script(shell, &script)
}
fn checked(path: &Path, directory: bool) -> Result<Option<fs::Metadata>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(path, e)),
    };
    if meta.file_type().is_symlink()
        || (directory && !meta.is_dir())
        || (!directory && !meta.is_file())
    {
        return Err(Error::UnsafePath(path.into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o022 != 0
            || (!directory && meta.nlink() != 1)
        {
            return Err(Error::UnsafePath(path.into()));
        }
    }
    Ok(Some(meta))
}
fn ensure_directory(home: &Path, relative: &str) -> Result<PathBuf> {
    let mut path = home.to_owned();
    for part in Path::new(relative).components() {
        path.push(part);
        if checked(&path, true)?.is_none() {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(io(&path, e)),
            }
            checked(&path, true)?.ok_or_else(|| Error::UnsafePath(path.clone()))?;
        }
    }
    Ok(path)
}
fn read(path: &Path) -> Result<(Vec<u8>, Option<fs::Permissions>)> {
    let Some(meta) = checked(path, false)? else {
        return Ok((Vec::new(), None));
    };
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|file| file.take(1_048_577).read_to_end(&mut bytes))
        .map_err(|e| io(path, e))?;
    if bytes.len() > 1_048_576 {
        return Err(Error::Completion("补全配置文件过大"));
    }
    Ok((bytes, Some(meta.permissions())))
}
fn write_atomic(
    path: &Path,
    bytes: &[u8],
    permissions: Option<fs::Permissions>,
    expected: &[u8],
) -> Result<()> {
    checked(path, false)?;
    let parent = path.parent().ok_or(Error::Completion("补全目标缺少目录"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|e| io(parent, e))?;
    file.write_all(bytes).map_err(|e| io(path, e))?;
    let expected_present = permissions.is_some();
    if let Some(mode) = permissions {
        file.as_file()
            .set_permissions(mode)
            .map_err(|e| io(path, e))?;
    }
    file.as_file().sync_all().map_err(|e| io(path, e))?;
    checked(path, false)?;
    let (current, mode) = read(path)?;
    if current != expected || mode.is_some() != expected_present {
        return Err(Error::Completion("补全文件在安装期间已变化；请重试"));
    }
    file.persist(path).map_err(|e| io(path, e.error))?;
    fs::File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|e| io(parent, e))?;
    Ok(())
}
fn install_script(shell: Shell, script: &[u8]) -> Result<()> {
    if !cfg!(unix) {
        return Err(Error::Completion(
            "自动安装补全当前仅支持 Unix；可用 --show-completion 生成脚本",
        ));
    }
    let (relative, name, rc, init) = match shell {
        Shell::Bash => (
            ".bash_completions",
            "colab.sh",
            Some(".bashrc"),
            "[ -r \"$HOME/.bash_completions/colab.sh\" ] && . \"$HOME/.bash_completions/colab.sh\"",
        ),
        Shell::Zsh => (
            ".zfunc",
            "_colab",
            Some(".zshrc"),
            "fpath=(\"$HOME/.zfunc\" $fpath)\nautoload -Uz compinit\ncompinit",
        ),
        Shell::Fish => (".config/fish/completions", "colab.fish", None, ""),
        _ => {
            return Err(Error::Completion(
                "当前仅安装 bash/zsh/fish；PowerShell 请用 --show-completion 后手动配置 profile",
            ));
        }
    };
    let home = std::env::var_os("HOME").ok_or(Error::Completion("安装补全需要 HOME"))?;
    if !Path::new(&home).is_absolute() {
        return Err(Error::Completion("HOME 必须为绝对路径"));
    }
    let home = fs::canonicalize(&home).map_err(|_| Error::Completion("无法读取 HOME"))?;
    checked(&home, true)?.ok_or(Error::Completion("HOME 不存在"))?;
    let directory = ensure_directory(&home, relative)?;
    let lock_path = directory.join(".colab-completion.lock");
    checked(&lock_path, false)?;
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options.open(&lock_path).map_err(|e| io(&lock_path, e))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match fs2::FileExt::try_lock_exclusive(&lock) {
            Ok(()) => break,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                return Err(Error::LockTimeout(lock_path));
            }
            Err(e) => return Err(io(&lock_path, e)),
        }
    }
    let target = directory.join(name);
    let (old_script, mode) = read(&target)?;
    let rc = rc.map(|name| home.join(name));
    let rc_change = if let Some(path) = rc.as_ref() {
        let (bytes, permissions) = read(path)?;
        let text = String::from_utf8(bytes.clone())
            .map_err(|_| Error::Completion("shell 配置不是 UTF-8；请手动安装补全"))?;
        let block =
            format!("# >>> colab native completion >>>\n{init}\n# <<< colab native completion <<<");
        if text.contains(&block) {
            None
        } else if text.contains("# >>> colab native completion >>>")
            || text.contains("# <<< colab native completion <<<")
        {
            return Err(Error::Completion(
                "已有不完整或不同的 colab 补全块；请先检查 shell 配置",
            ));
        } else {
            Some((
                format!(
                    "{text}{}{}\n",
                    if text.ends_with('\n') || text.is_empty() {
                        ""
                    } else {
                        "\n"
                    },
                    block
                )
                .into_bytes(),
                permissions,
                bytes,
            ))
        }
    } else {
        None
    };
    write_atomic(&target, script, mode, &old_script)?;
    if let (Some(path), Some((bytes, mode, expected))) = (rc, rc_change) {
        write_atomic(&path, &bytes, mode, &expected)?;
    }
    println!("已安装补全：{}；在新的 shell 会话中加载", target.display());
    Ok(())
}
