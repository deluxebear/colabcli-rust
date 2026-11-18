#![cfg(unix)]
use colabcli_rust::{
    Error,
    update::{self, UpdateClient},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::{fs, path::PathBuf, process::Command, sync::OnceLock, thread, time::Duration};
use tiny_http::{Request, Response, Server, StatusCode};
use url::Url;
fn images() -> &'static std::collections::BTreeMap<&'static str, Vec<u8>> {
    static IMAGES: OnceLock<std::collections::BTreeMap<&'static str, Vec<u8>>> = OnceLock::new();
    IMAGES.get_or_init(|| {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("main.rs");
        let mut images = std::collections::BTreeMap::new();
        for (name, body) in [
            ("old", "println!(\"Version: 0.1.0\");"),
            ("new", "println!(\"Version: 0.2.0\");"),
            ("third", "println!(\"Version: 0.3.0\");"),
            (
                "hang",
                "loop {std::thread::sleep(std::time::Duration::from_secs(1));}",
            ),
            ("flood", "print!(\"{}\",\"x\".repeat(5000));"),
        ] {
            fs::write(&source, format!("fn main() {{{body}}}")).unwrap();
            let binary = temp.path().join(name);
            let result = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
                .args([
                    "--edition=2024",
                    "--crate-name",
                    "update_fixture",
                    "--target",
                    update::current_target().unwrap(),
                    "-C",
                    "opt-level=s",
                    "-C",
                    "strip=symbols",
                ])
                .arg(&source)
                .arg("-o")
                .arg(&binary)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            images.insert(name, fs::read(binary).unwrap());
        }
        images
    })
}
struct Fixture {
    temp: tempfile::TempDir,
    target: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let target = bin.join("colab");
        fs::write(&target, &images()["old"]).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        Self { temp, target }
    }
}
fn http_server() -> Server {
    Server::http("127.0.0.1:0").unwrap()
}
fn server_base(server: &Server) -> String {
    format!("http://{}", server.server_addr())
}
fn recv(server: &Server, path: &str) -> Request {
    let request = server
        .recv_timeout(Duration::from_secs(8))
        .unwrap()
        .expect("expected request");
    assert_eq!(request.url(), path);
    assert_eq!(request.method().as_str(), "GET");
    assert!(
        !request
            .headers()
            .iter()
            .any(|h| h.field.equiv("Authorization") || h.field.equiv("Cookie"))
    );
    request
}
fn manifest(base: &str, version: &str, bytes: &[u8]) -> Value {
    json!({"schema_version":1,"package":"colabcli-rust","version":version,"artifacts":{update::current_target().unwrap():{"url":format!("{base}/colab"),"size":bytes.len(),"sha256":format!("{:x}",Sha256::digest(bytes))}}})
}
fn client() -> UpdateClient {
    UpdateClient::new(Duration::from_secs(10)).unwrap()
}
fn feed(doc: Value, bytes: Vec<u8>, server: Server, asset: bool) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        recv(&server, "/manifest.json")
            .respond(Response::from_string(doc.to_string()))
            .unwrap();
        if asset {
            recv(&server, "/colab")
                .respond(Response::from_data(bytes))
                .unwrap();
        }
    })
}
fn status(f: &Fixture) -> Value {
    update::status(&f.target).unwrap()
}
fn journal(f: &Fixture) -> PathBuf {
    let canonical = fs::canonicalize(&f.target).unwrap();
    f.target
        .parent()
        .unwrap()
        .join(".colab-updates")
        .join(format!(
            "{:x}",
            Sha256::digest(canonical.as_os_str().as_encoded_bytes())
        ))
        .join("state.json")
}
fn install(
    f: &Fixture,
    version: &str,
    image: &str,
) -> colabcli_rust::Result<update::InstallResult> {
    let server = http_server();
    let base = server_base(&server);
    let doc = manifest(&base, version, &images()[image]);
    let handler = feed(doc, images()[image].clone(), server, true);
    let client = client();
    let plan = client
        .check(
            &Url::parse(&format!("{base}/manifest.json")).unwrap(),
            "0.1.0",
        )
        .unwrap();
    let result = client.install(&plan, &f.target);
    handler.join().unwrap();
    result
}
#[test]
fn installs_a_verified_native_image_preserves_permissions_and_rolls_back_offline() {
    let f = Fixture::new();
    let original = fs::read(&f.target).unwrap();
    assert_eq!(status(&f)["state"], "unmanaged");
    let result = install(&f, "0.2.0", "new").unwrap();
    assert_eq!(result.status, "installed");
    assert!(!result.cleanup_pending);
    assert_eq!(fs::read(&f.target).unwrap(), images()["new"]);
    assert_eq!(
        fs::metadata(&f.target).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let view = status(&f);
    assert_eq!(view["state"], "committed");
    assert_eq!(view["current"], "installed");
    assert_eq!(view["rollback_available"], true);
    let result = update::rollback(&f.target).unwrap();
    assert_eq!(result.status, "rolled_back");
    assert_eq!(result.version, "0.1.0");
    assert!(!result.cleanup_pending);
    assert_eq!(fs::read(&f.target).unwrap(), original);
    assert_eq!(status(&f)["state"], "unmanaged");
}
#[test]
fn semver_precedence_ignores_build_metadata_and_check_never_downloads_or_initializes_accounts() {
    for (version, current, available) in [
        ("0.1.0+other", "0.1.0", false),
        ("0.0.9", "0.1.0", false),
        ("0.2.0-alpha.1", "0.1.0", true),
    ] {
        let server = http_server();
        let base = server_base(&server);
        let bytes = b"unused";
        let doc = manifest(&base, version, bytes);
        let handler = feed(doc, vec![], server, false);
        let plan = client()
            .check(
                &Url::parse(&format!("{base}/manifest.json")).unwrap(),
                current,
            )
            .unwrap();
        assert_eq!(plan.update_available, available);
        handler.join().unwrap();
    }
    let f = Fixture::new();
    let server = http_server();
    let base = server_base(&server);
    let doc = manifest(&base, "0.2.0", b"not-downloaded");
    let handler = feed(doc, vec![], server, false);
    let config = f.temp.path().join("must-not-create");
    let output = cli(&f)
        .arg("--config-dir")
        .arg(&config)
        .args([
            "update",
            "--manifest",
            &format!("{base}/manifest.json"),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["update_available"], true);
    assert!(!result.to_string().contains("sha256"));
    handler.join().unwrap();
    assert!(!config.exists());
    assert!(!journal(&f).parent().unwrap().exists());
}
fn cli(f: &Fixture) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_colab"));
    command
        .env("HOME", f.temp.path())
        .env_remove("COLAB_CLI_UPDATE_MANIFEST")
        .env_remove("COLAB_ACCOUNT")
        .env_remove("COLAB_CLI_CONFIG_DIR");
    command
}
#[test]
fn cli_installs_and_rolls_back_temp_target_and_status_ignores_a_broken_channel() {
    let f = Fixture::new();
    let server = http_server();
    let base = server_base(&server);
    let handler = feed(
        manifest(&base, "0.2.0", &images()["new"]),
        images()["new"].clone(),
        server,
        true,
    );
    let output = cli(&f)
        .args([
            "update",
            "--install",
            "--manifest",
            &format!("{base}/manifest.json"),
            "--json",
            "--install-path",
        ])
        .arg(&f.target)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["status"],
        "installed"
    );
    handler.join().unwrap();
    let output = cli(&f)
        .env("COLAB_CLI_UPDATE_MANIFEST", "private-invalid-channel")
        .args(["update", "--status", "--install-path"])
        .arg(&f.target)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["rollback_available"],
        true
    );
    let output = cli(&f)
        .env("COLAB_CLI_UPDATE_MANIFEST", "private-invalid-channel")
        .args(["update", "--rollback", "--json", "--install-path"])
        .arg(&f.target)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&f.target).unwrap(), images()["old"]);
}
#[test]
fn corrupted_download_wrong_architecture_or_wrong_version_never_replaces_the_original() {
    for problem in ["hash", "length", "format", "arch", "version", "flood"] {
        let f = Fixture::new();
        let mut bytes = match problem {
            "format" => b"not a native executable".to_vec(),
            "version" => images()["third"].clone(),
            "flood" => images()["flood"].clone(),
            _ => images()["new"].clone(),
        };
        if problem == "arch" {
            if cfg!(target_os = "macos") {
                bytes[4..8].copy_from_slice(&0u32.to_le_bytes());
            } else {
                bytes[18..20].copy_from_slice(&0u16.to_le_bytes());
            }
        }
        let server = http_server();
        let base = server_base(&server);
        let mut doc = manifest(&base, "0.2.0", &bytes);
        let target = update::current_target().unwrap();
        if problem == "hash" {
            doc["artifacts"][target]["sha256"] = json!("0".repeat(64));
        }
        if problem == "length" {
            doc["artifacts"][target]["size"] = json!(bytes.len() + 1);
        }
        let handler = feed(doc, bytes, server, true);
        let client = client();
        let plan = client
            .check(
                &Url::parse(&format!("{base}/manifest.json")).unwrap(),
                "0.1.0",
            )
            .unwrap();
        assert!(client.install(&plan, &f.target).is_err(), "{problem}");
        handler.join().unwrap();
        assert_eq!(fs::read(&f.target).unwrap(), images()["old"]);
        assert_eq!(status(&f)["state"], "unmanaged");
    }
}
#[test]
fn a_hung_native_version_probe_is_killed_without_touching_the_installation() {
    let f = Fixture::new();
    let started = std::time::Instant::now();
    assert!(install(&f, "0.2.0", "hang").is_err());
    assert!(started.elapsed() < Duration::from_secs(9));
    assert_eq!(fs::read(&f.target).unwrap(), images()["old"]);
    assert!(!journal(&f).exists());
}
#[test]
fn a_prepared_or_rolling_back_record_blocks_new_install_and_can_resume_rollback() {
    for (phase, image) in [
        ("prepared", "old"),
        ("prepared", "new"),
        ("rolling_back", "old"),
        ("rolling_back", "new"),
    ] {
        let f = Fixture::new();
        install(&f, "0.2.0", "new").unwrap();
        let path = journal(&f);
        let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        state["phase"] = json!(phase);
        fs::write(&f.target, &images()[image]).unwrap();
        fs::write(&path, state.to_string()).unwrap();
        assert!(matches!(install_no_download(&f), Err(Error::UpdatePending)));
        assert_eq!(status(&f)["state"], phase);
        update::rollback(&f.target).unwrap();
        assert_eq!(fs::read(&f.target).unwrap(), images()["old"]);
    }
}
fn install_no_download(f: &Fixture) -> colabcli_rust::Result<update::InstallResult> {
    let server = http_server();
    let base = server_base(&server);
    let handler = feed(
        manifest(&base, "0.3.0", b"not-downloaded"),
        vec![],
        server,
        false,
    );
    let client = client();
    let plan = client
        .check(
            &Url::parse(&format!("{base}/manifest.json")).unwrap(),
            "0.1.0",
        )
        .unwrap();
    let result = client.install(&plan, &f.target);
    handler.join().unwrap();
    result
}
#[test]
fn rollback_rejects_modified_installations_and_corrupt_or_symlinked_backups() {
    for problem in ["target", "backup", "symlink"] {
        let f = Fixture::new();
        install(&f, "0.2.0", "new").unwrap();
        let state: Value = serde_json::from_slice(&fs::read(journal(&f)).unwrap()).unwrap();
        let backup = journal(&f)
            .parent()
            .unwrap()
            .join(state["generation"].as_str().unwrap())
            .join("previous");
        if problem == "target" {
            fs::write(&f.target, &images()["third"]).unwrap();
        } else if problem == "backup" {
            fs::write(&backup, b"corrupt").unwrap();
        } else {
            fs::remove_file(&backup).unwrap();
            std::os::unix::fs::symlink(&f.target, &backup).unwrap();
        }
        let before = fs::read(&f.target).unwrap();
        assert!(update::rollback(&f.target).is_err());
        assert_eq!(fs::read(&f.target).unwrap(), before);
        assert!(journal(&f).exists());
    }
}
#[test]
fn changed_target_during_download_is_not_overwritten() {
    let f = Fixture::new();
    let server = http_server();
    let base = server_base(&server);
    let doc = manifest(&base, "0.2.0", &images()["new"]);
    let target = f.target.clone();
    let handler = thread::spawn(move || {
        recv(&server, "/manifest.json")
            .respond(Response::from_string(doc.to_string()))
            .unwrap();
        let request = recv(&server, "/colab");
        fs::write(target, &images()["third"]).unwrap();
        request
            .respond(Response::from_data(images()["new"].clone()))
            .unwrap();
    });
    let client = client();
    let plan = client
        .check(
            &Url::parse(&format!("{base}/manifest.json")).unwrap(),
            "0.1.0",
        )
        .unwrap();
    assert!(client.install(&plan, &f.target).is_err());
    handler.join().unwrap();
    assert_eq!(fs::read(&f.target).unwrap(), images()["third"]);
    assert!(!journal(&f).exists());
}
#[test]
fn urls_bad_manifests_and_http_failures_are_rejected_without_echoing_remote_data() {
    for uri in [
        "http://private.invalid/manifest",
        "https://user:private-password@example.invalid/manifest",
        "https://example.invalid/manifest?private-key=x",
    ] {
        let error = client()
            .check(&Url::parse(uri).unwrap(), "0.1.0")
            .err()
            .unwrap();
        assert!(!error.to_string().contains("private-"));
    }
    for problem in [
        "schema", "package", "version", "target", "sha", "size", "json", "oversize", "status",
    ] {
        let server = http_server();
        let base = server_base(&server);
        let mut doc = manifest(&base, "0.2.0", b"unused");
        let target = update::current_target().unwrap();
        match problem {
            "schema" => doc["schema_version"] = json!(2),
            "package" => doc["package"] = json!("private-package"),
            "version" => doc["version"] = json!("private-version"),
            "target" => doc["artifacts"] = json!({}),
            "sha" => doc["artifacts"][target]["sha256"] = json!("private-sha"),
            "size" => doc["artifacts"][target]["size"] = json!(u64::MAX),
            _ => {}
        }
        let body = match problem {
            "json" => "private-body".to_owned(),
            "oversize" => "x".repeat(65537),
            _ => doc.to_string(),
        };
        let handler = thread::spawn(move || {
            recv(&server, "/manifest.json")
                .respond(
                    Response::from_string(body)
                        .with_status_code(StatusCode(if problem == "status" { 500 } else { 200 })),
                )
                .unwrap();
        });
        let error = client()
            .check(
                &Url::parse(&format!("{base}/manifest.json")).unwrap(),
                "0.1.0",
            )
            .err()
            .unwrap();
        assert!(!error.to_string().contains("private-"));
        handler.join().unwrap();
    }
}

#[test]
fn successive_upgrades_keep_one_generation_and_rollback_does_not_chain() {
    let f = Fixture::new();
    install(&f, "0.2.0", "new").unwrap();
    let server = http_server();
    let base = server_base(&server);
    let handler = feed(
        manifest(&base, "0.3.0", &images()["third"]),
        images()["third"].clone(),
        server,
        true,
    );
    let client = client();
    let plan = client
        .check(
            &Url::parse(&format!("{base}/manifest.json")).unwrap(),
            "0.2.0",
        )
        .unwrap();
    let result = client.install(&plan, &f.target).unwrap();
    handler.join().unwrap();
    assert!(!result.cleanup_pending);
    let view = status(&f);
    assert_eq!(view["previous_version"], "0.2.0");
    assert_eq!(view["staging_generations"], 0);
    assert_eq!(view["cleanup_pending"], false);
    update::rollback(&f.target).unwrap();
    assert_eq!(fs::read(&f.target).unwrap(), images()["new"]);
    assert!(update::rollback(&f.target).is_err());
}
#[test]
fn concurrent_installers_serialize_and_the_old_plan_does_not_download_twice() {
    let f = Fixture::new();
    let server = http_server();
    let base = server_base(&server);
    let doc = manifest(&base, "0.2.0", &images()["new"]);
    let (ready, wait) = std::sync::mpsc::channel();
    let (release, resume) = std::sync::mpsc::channel();
    let handler = thread::spawn(move || {
        recv(&server, "/manifest.json")
            .respond(Response::from_string(doc.to_string()))
            .unwrap();
        let request = recv(&server, "/colab");
        ready.send(()).unwrap();
        resume.recv_timeout(Duration::from_secs(5)).unwrap();
        request
            .respond(Response::from_data(images()["new"].clone()))
            .unwrap();
        assert!(
            server
                .recv_timeout(Duration::from_millis(200))
                .unwrap()
                .is_none()
        );
    });
    let client = std::sync::Arc::new(client());
    let plan = std::sync::Arc::new(
        client
            .check(
                &Url::parse(&format!("{base}/manifest.json")).unwrap(),
                "0.1.0",
            )
            .unwrap(),
    );
    let first_client = client.clone();
    let first_plan = plan.clone();
    let target = f.target.clone();
    let first = thread::spawn(move || first_client.install(&first_plan, &target));
    wait.recv_timeout(Duration::from_secs(5)).unwrap();
    let target = f.target.clone();
    let second = thread::spawn(move || client.install(&plan, &target));
    thread::sleep(Duration::from_millis(100));
    release.send(()).unwrap();
    assert_eq!(first.join().unwrap().unwrap().status, "installed");
    assert!(second.join().unwrap().is_err());
    handler.join().unwrap();
    assert_eq!(fs::read(&f.target).unwrap(), images()["new"]);
}
#[test]
fn redirects_support_release_downloads_but_stop_loops_and_unsafe_destinations() {
    let f = Fixture::new();
    let server = http_server();
    let base = server_base(&server);
    let doc = manifest(&base, "0.2.0", &images()["new"]);
    let redirect_base = base.clone();
    let handler = thread::spawn(move || {
        for (from, to) in [
            ("/manifest.json", "/redirect.json"),
            ("/colab", "/payload?signature=fixture"),
        ] {
            recv(&server, from)
                .respond(
                    Response::empty(302).with_header(
                        tiny_http::Header::from_bytes("Location", format!("{redirect_base}{to}"))
                            .unwrap(),
                    ),
                )
                .unwrap();
            if to == "/redirect.json" {
                recv(&server, to)
                    .respond(Response::from_string(doc.to_string()))
                    .unwrap();
            } else {
                recv(&server, to)
                    .respond(Response::from_data(images()["new"].clone()))
                    .unwrap();
            }
        }
    });
    let client = client();
    let plan = client
        .check(
            &Url::parse(&format!("{base}/manifest.json")).unwrap(),
            "0.1.0",
        )
        .unwrap();
    client.install(&plan, &f.target).unwrap();
    handler.join().unwrap();
    for unsafe_destination in [true, false] {
        let server = http_server();
        let base = server_base(&server);
        let redirect_base = base.clone();
        let handler = thread::spawn(move || {
            if unsafe_destination {
                recv(&server, "/manifest.json")
                    .respond(
                        Response::empty(302).with_header(
                            tiny_http::Header::from_bytes(
                                "Location",
                                "http://private-invalid.invalid/secret",
                            )
                            .unwrap(),
                        ),
                    )
                    .unwrap();
            } else {
                for index in 0..6 {
                    let path = if index == 0 {
                        "/manifest.json".into()
                    } else {
                        format!("/r{index}")
                    };
                    recv(&server, &path)
                        .respond(
                            Response::empty(302).with_header(
                                tiny_http::Header::from_bytes(
                                    "Location",
                                    format!("{redirect_base}/r{}", index + 1),
                                )
                                .unwrap(),
                            ),
                        )
                        .unwrap();
                }
            }
        });
        let error = client
            .check(
                &Url::parse(&format!("{base}/manifest.json")).unwrap(),
                "0.1.0",
            )
            .err()
            .unwrap();
        assert!(matches!(error, Error::UpdateTransport));
        assert!(!error.to_string().contains("private"));
        handler.join().unwrap();
    }
}
#[test]
fn symlink_or_world_writable_install_locations_are_rejected_without_mutation() {
    let f = Fixture::new();
    let alias = f.target.parent().unwrap().join("alias");
    std::os::unix::fs::symlink(&f.target, &alias).unwrap();
    assert!(update::status(&alias).is_err());
    fs::set_permissions(&f.target, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(update::status(&f.target).is_err());
    fs::set_permissions(&f.target, fs::Permissions::from_mode(0o700)).unwrap();
    let parent = f.target.parent().unwrap();
    fs::set_permissions(parent, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(update::status(&f.target).is_err());
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(fs::read(&f.target).unwrap(), images()["old"]);
}
#[test]
fn atomic_rename_failure_keeps_the_original_and_clears_the_uncommitted_record() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let f = Fixture::new();
    let server = http_server();
    let base = server_base(&server);
    let doc = manifest(&base, "0.2.0", &images()["new"]);
    let parent = f.target.parent().unwrap().to_path_buf();
    let handler = thread::spawn(move || {
        recv(&server, "/manifest.json")
            .respond(Response::from_string(doc.to_string()))
            .unwrap();
        let request = recv(&server, "/colab");
        fs::set_permissions(parent, fs::Permissions::from_mode(0o500)).unwrap();
        request
            .respond(Response::from_data(images()["new"].clone()))
            .unwrap();
    });
    let client = client();
    let plan = client
        .check(
            &Url::parse(&format!("{base}/manifest.json")).unwrap(),
            "0.1.0",
        )
        .unwrap();
    let result = client.install(&plan, &f.target);
    handler.join().unwrap();
    fs::set_permissions(
        f.target.parent().unwrap(),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    assert!(result.is_err());
    assert_eq!(fs::read(&f.target).unwrap(), images()["old"]);
    assert!(!journal(&f).exists());
}
#[test]
fn failed_asset_requests_leave_no_published_update_and_do_not_expose_the_body() {
    let f = Fixture::new();
    let server = http_server();
    let base = server_base(&server);
    let doc = manifest(&base, "0.2.0", &images()["new"]);
    let handler = thread::spawn(move || {
        recv(&server, "/manifest.json")
            .respond(Response::from_string(doc.to_string()))
            .unwrap();
        recv(&server, "/colab")
            .respond(
                Response::from_string("private-update-token").with_status_code(StatusCode(503)),
            )
            .unwrap();
    });
    let client = client();
    let plan = client
        .check(
            &Url::parse(&format!("{base}/manifest.json")).unwrap(),
            "0.1.0",
        )
        .unwrap();
    let error = client.install(&plan, &f.target).err().unwrap();
    assert!(matches!(error, Error::UpdateStatus(503)));
    assert!(!error.to_string().contains("private-update-token"));
    handler.join().unwrap();
    assert_eq!(fs::read(&f.target).unwrap(), images()["old"]);
    assert!(!journal(&f).exists());
}
