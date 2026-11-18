#![cfg(unix)]
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    process::{Command, Output},
};
fn command(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_colab"));
    cmd.env_clear()
        .env("HOME", home)
        .env("SHELL", "/bin/bash")
        .env("COLAB_CLI_CONFIG_DIR", home.join("must-not-create"))
        .env(
            "COLAB_DRIVE_CLIENT_SECRET",
            "synthetic-private-shell-secret",
        )
        .env(
            "COLAB_DRIVE_REFRESH_TOKEN",
            "synthetic-private-shell-refresh",
        );
    cmd
}
fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
fn all_completion_formats_and_auto_shell_are_offline_and_do_not_embed_credentials() {
    let home = tempfile::tempdir().unwrap();
    let mut powershell = None;
    for shell in ["bash", "zsh", "fish", "powershell", "pwsh"] {
        let output = command(home.path())
            .args(["--show-completion", shell])
            .output()
            .unwrap();
        success(&output);
        let script = String::from_utf8(output.stdout).unwrap();
        assert!(script.contains("colab") && script.contains("account"));
        assert!(!script.contains("synthetic-private-shell"));
        if shell == "powershell" {
            powershell = Some(script.clone());
        }
        if shell == "pwsh" {
            assert_eq!(Some(script), powershell);
        }
    }
    let auto = command(home.path())
        .env("SHELL", "/bin/zsh")
        .arg("--show-completion")
        .output()
        .unwrap();
    success(&auto);
    let explicit = command(home.path())
        .args(["--show-completion=zsh"])
        .output()
        .unwrap();
    success(&explicit);
    assert_eq!(auto.stdout, explicit.stdout);
    let no_home = command(home.path())
        .env_remove("HOME")
        .args(["--show-completion=fish"])
        .output()
        .unwrap();
    success(&no_home);
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
}
#[test]
fn bash_install_preserves_rc_permissions_and_runs_actual_completion_without_executing_cli() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home '中 $name");
    fs::create_dir(&home).unwrap();
    fs::write(home.join(".bashrc"), "export EXISTING_RC=yes\n").unwrap();
    fs::set_permissions(home.join(".bashrc"), fs::Permissions::from_mode(0o640)).unwrap();
    for _ in 0..2 {
        success(
            &command(&home)
                .args(["--install-completion", "bash"])
                .output()
                .unwrap(),
        );
    }
    let rc = fs::read_to_string(home.join(".bashrc")).unwrap();
    assert!(rc.starts_with("export EXISTING_RC=yes\n"));
    assert_eq!(rc.matches("# >>> colab native completion >>>").count(), 1);
    assert_eq!(
        fs::metadata(home.join(".bashrc"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
    let output=Command::new("/bin/bash").env_clear().env("HOME",&home).args(["--noprofile","--norc","-c",r#". "$HOME/.bashrc"; test "$EXISTING_RC" = yes || exit 4; COMP_WORDS=(colab ac); COMP_CWORD=1; _colab colab ac colab; printf '%s\n' "${COMPREPLY[@]}""#]).output().unwrap();
    success(&output);
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .any(|line| line == "account")
    );
    assert!(!home.join("must-not-create").exists());
}
#[test]
fn zsh_and_fish_install_in_standard_paths_without_replacing_existing_configuration() {
    for shell in ["zsh", "fish"] {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join(".zshrc"), "# existing settings\n").unwrap();
        success(
            &command(home.path())
                .args(["--install-completion", shell])
                .output()
                .unwrap(),
        );
        success(
            &command(home.path())
                .args(["--install-completion", shell])
                .output()
                .unwrap(),
        );
        if shell == "fish" {
            assert!(
                home.path()
                    .join(".config/fish/completions/colab.fish")
                    .is_file()
            );
            assert_eq!(
                fs::read_to_string(home.path().join(".zshrc")).unwrap(),
                "# existing settings\n"
            );
        } else {
            assert!(home.path().join(".zfunc/_colab").is_file());
            let rc = fs::read_to_string(home.path().join(".zshrc")).unwrap();
            assert!(rc.starts_with("# existing settings\n"));
            assert_eq!(rc.matches("# >>> colab native completion >>>").count(), 1);
            if Path::new("/bin/zsh").exists() {
                let output = Command::new("/bin/zsh")
                    .args(["-n"])
                    .arg(home.path().join(".zfunc/_colab"))
                    .output()
                    .unwrap();
                success(&output);
                let output = Command::new("/bin/zsh")
                    .env_clear()
                    .env("HOME", home.path())
                    .args([
                        "-f",
                        "-c",
                        r#"source "$HOME/.zshrc"; autoload +X _colab; (( $+functions[_colab] ))"#,
                    ])
                    .output()
                    .unwrap();
                success(&output);
            }
        }
    }
}
#[test]
fn concurrent_installers_do_not_duplicate_the_rc_block_or_leave_partial_scripts() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join(".bashrc"), "# preserved\n").unwrap();
    let mut children = Vec::new();
    for _ in 0..4 {
        children.push(
            command(home.path())
                .args(["--install-completion=bash"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    for child in children {
        success(&child.wait_with_output().unwrap());
    }
    let rc = fs::read_to_string(home.path().join(".bashrc")).unwrap();
    assert_eq!(rc.matches("# >>> colab native completion >>>").count(), 1);
    assert!(rc.starts_with("# preserved\n"));
    let script = fs::read(home.path().join(".bash_completions/colab.sh")).unwrap();
    let shown = command(home.path())
        .args(["--show-completion=bash"])
        .output()
        .unwrap();
    success(&shown);
    assert_eq!(script, shown.stdout);
}
#[test]
fn installation_rejects_symlink_hardlink_and_writable_rc_without_changing_other_files() {
    for kind in ["symlink", "hardlink", "writable"] {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        fs::create_dir(&home).unwrap();
        let outside = root.path().join("outside");
        fs::write(&outside, b"preserve exactly").unwrap();
        let rc = home.join(".bashrc");
        match kind {
            "symlink" => symlink(&outside, &rc).unwrap(),
            "hardlink" => fs::hard_link(&outside, &rc).unwrap(),
            _ => {
                fs::write(&rc, b"preserve exactly").unwrap();
                fs::set_permissions(&rc, fs::Permissions::from_mode(0o666)).unwrap();
            }
        }
        assert!(
            !command(&home)
                .args(["--install-completion=bash"])
                .output()
                .unwrap()
                .status
                .success()
        );
        assert_eq!(fs::read(&outside).unwrap(), b"preserve exactly");
        assert_eq!(fs::read(&rc).unwrap(), b"preserve exactly");
        assert!(!home.join(".bash_completions/colab.sh").exists());
    }
}
#[test]
fn installation_rejects_foreign_directory_and_script_links_before_touching_rc() {
    for directory_link in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let outside = root.path().join("outside");
        fs::create_dir(&home).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(home.join(".bashrc"), b"unchanged").unwrap();
        fs::write(outside.join("colab.sh"), b"foreign").unwrap();
        if directory_link {
            symlink(&outside, home.join(".bash_completions")).unwrap();
        } else {
            fs::create_dir(home.join(".bash_completions")).unwrap();
            symlink(
                outside.join("colab.sh"),
                home.join(".bash_completions/colab.sh"),
            )
            .unwrap();
        }
        assert!(
            !command(&home)
                .args(["--install-completion=bash"])
                .output()
                .unwrap()
                .status
                .success()
        );
        assert_eq!(fs::read(home.join(".bashrc")).unwrap(), b"unchanged");
        assert_eq!(fs::read(outside.join("colab.sh")).unwrap(), b"foreign");
    }
}
#[test]
fn malformed_or_oversized_rc_does_not_publish_a_script() {
    for bytes in [
        vec![0xff],
        vec![b'x'; 1_048_577],
        b"# >>> colab native completion >>>\nincomplete".to_vec(),
    ] {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join(".bashrc"), &bytes).unwrap();
        assert!(
            !command(home.path())
                .args(["--install-completion=bash"])
                .output()
                .unwrap()
                .status
                .success()
        );
        assert_eq!(fs::read(home.path().join(".bashrc")).unwrap(), bytes);
        assert!(!home.path().join(".bash_completions/colab.sh").exists());
    }
}
#[test]
fn invalid_shell_conflicts_and_help_never_install_or_initialize_accounts() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        &["--show-completion=unsupported"][..],
        &["--show-completion=bash", "--install-completion=bash"][..],
    ] {
        let output = command(home.path()).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
    assert!(
        !command(home.path())
            .env("SHELL", "/bin/unknown")
            .arg("--show-completion")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        !command(home.path())
            .args(["--install-completion=pwsh"])
            .output()
            .unwrap()
            .status
            .success()
    );
    success(
        &command(home.path())
            .args(["--install-completion=bash", "--help"])
            .output()
            .unwrap(),
    );
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
}
