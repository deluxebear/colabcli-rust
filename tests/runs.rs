mod common;
use colabcli_rust::{
    Error,
    colab_api::{ApiConfig, ColabApi, RuntimeOptions},
    credentials::TokenStorageMode,
    jupyter::{ExecutionResult, ExecutionStatus, ExecutionTimeout, output::Output},
    models::Secret,
    runs::{self, PreparedRun, RunOptions},
    sessions::SessionService,
};
use common::*;
use serde_json::json;
use std::{
    fs,
    sync::{Arc, atomic::AtomicUsize},
    thread,
    time::Duration,
};
fn options() -> RunOptions {
    RunOptions {
        runtime: RuntimeOptions::new(None, None, false).unwrap(),
        keep: false,
        timeout: ExecutionTimeout::Idle(Some(Duration::from_secs(1))),
        http_timeout: Duration::from_secs(1),
        cancellation: Arc::new(AtomicUsize::new(0)),
    }
}
fn report(nonce: &str, code: i32, status: ExecutionStatus) -> ExecutionResult {
    let value = json!({"output_type":"display_data","data":{runs::RESULT_MIME:{"nonce":nonce,"exit_code":code}},"metadata":{}});
    ExecutionResult {
        msg_id: "id".into(),
        status,
        outputs: vec![Output {
            data: value.as_object().unwrap().clone(),
            buffers: vec![],
        }],
        reply: Some(json!({"status":"ok"})),
        error: None,
        timed_out: false,
        history_error: None,
    }
}
#[test]
fn script_result_requires_matching_single_exit_marker_and_confirmed_completion() {
    for code in [0, 1, 7, 255] {
        assert_eq!(
            runs::script_exit_code(&[report("run", code, ExecutionStatus::Succeeded)], "run")
                .unwrap(),
            code
        );
    }
    assert!(
        runs::script_exit_code(&[report("other", 0, ExecutionStatus::Succeeded)], "run").is_err()
    );
    assert!(runs::script_exit_code(&[report("run", 0, ExecutionStatus::Unknown)], "run").is_err());
    assert_eq!(
        runs::script_exit_code(&[report("run", 0, ExecutionStatus::Failed)], "run").unwrap(),
        1
    );
    assert!(
        runs::script_exit_code(
            &[
                report("run", 0, ExecutionStatus::Succeeded),
                report("run", 0, ExecutionStatus::Succeeded)
            ],
            "run"
        )
        .is_err()
    );
}
#[test]
fn run_cleans_up_a_definitive_refusal_but_keeps_an_uncertain_post() {
    for uncertain in [false, true] {
        let f = Fixture::new(TokenStorageMode::File);
        let script = f.temp.path().join("job.py");
        fs::write(&script, "print('job')").unwrap();
        let prepared = PreparedRun::prepare(&script, &[], &[], Some("job")).unwrap();
        let run_id = prepared.id.clone();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = url::Url::parse(&format!("http://{}", server.server_addr())).unwrap();
        let handler = thread::spawn(move || {
            let recv = || {
                let request = server
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                assert!(request.url().starts_with("/tun/m/assign?"));
                request
            };
            let request = recv();
            assert_eq!(request.method().as_str(), "GET");
            if uncertain {
                request
                    .respond(tiny_http::Response::from_string(
                        json!({"token":"xsrf","nbh":"fixture","acc":"NONE","variant":"DEFAULT"})
                            .to_string(),
                    ))
                    .unwrap();
                let request = recv();
                assert_eq!(request.method().as_str(), "POST");
                request.respond(tiny_http::Response::empty(504)).unwrap();
            } else {
                request.respond(tiny_http::Response::empty(412)).unwrap();
            }
            assert!(
                server
                    .recv_timeout(Duration::from_millis(100))
                    .unwrap()
                    .is_none()
            );
        });
        let factory = || {
            Ok(SessionService::new(
                f.storage.clone(),
                id("default"),
                ColabApi::new(
                    ApiConfig {
                        base_url: base.clone(),
                        retries: 0,
                        ..ApiConfig::default()
                    },
                    Secret::new("fixture-oauth"),
                )?,
            ))
        };
        let outcome = runs::run(
            &f.storage,
            id("default"),
            prepared,
            options(),
            factory,
            |_| Ok(()),
        )
        .unwrap();
        handler.join().unwrap();
        assert_eq!(outcome.exit_code, 1);
        assert!(outcome.execution.is_empty());
        let plans = runs::pending(&f.storage, &id("default")).unwrap();
        if uncertain {
            assert_eq!(outcome.cleanup, runs::Cleanup::Pending);
            assert_eq!(plans[0].id, run_id);
            assert_eq!(plans[0].runtime.as_ref().unwrap().accelerator, "NONE");
            assert!(matches!(
                f.accounts.remove(&id("default"), true),
                Err(Error::AccountInUse)
            ));
        } else {
            assert_eq!(outcome.cleanup, runs::Cleanup::NotAllocated);
            assert!(plans.is_empty());
        }
    }
}
#[test]
fn run_session_name_collision_does_not_delete_the_existing_session() {
    let f = Fixture::new(TokenStorageMode::File);
    f.storage
        .save_session(&id("default"), session("existing"))
        .unwrap();
    let script = f.temp.path().join("job.py");
    fs::write(&script, "pass").unwrap();
    let prepared = PreparedRun::prepare(&script, &[], &[], Some("existing")).unwrap();
    let service = || {
        Ok(SessionService::new(
            f.storage.clone(),
            id("default"),
            ColabApi::new(ApiConfig::default(), Secret::new("not-sent"))?,
        ))
    };
    let result = runs::run(
        &f.storage,
        id("default"),
        prepared,
        options(),
        service,
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(result.cleanup, runs::Cleanup::NotAllocated);
    assert_eq!(
        f.storage.sessions(&id("default")).unwrap()["existing"].endpoint,
        "ep-existing"
    );
    assert!(
        runs::pending(&f.storage, &id("default"))
            .unwrap()
            .is_empty()
    );
}
#[test]
fn cancellation_before_allocation_never_requests_a_vm() {
    let f = Fixture::new(TokenStorageMode::File);
    let script = f.temp.path().join("job.py");
    fs::write(&script, "pass").unwrap();
    let prepared = PreparedRun::prepare(&script, &[], &[], None).unwrap();
    let mut opts = options();
    opts.cancellation = Arc::new(AtomicUsize::new(15));
    let service = || panic!("cancelled run must not initialize authentication");
    let result = runs::run(&f.storage, id("default"), prepared, opts, service, |_| {
        Ok(())
    })
    .unwrap();
    assert_eq!(result.exit_code, 143);
    assert_eq!(result.cleanup, runs::Cleanup::NotAllocated);
}
#[test]
fn keyring_timeout_before_allocation_does_not_repeat_authentication_for_cleanup() {
    let f = Fixture::new(TokenStorageMode::File);
    let script = f.temp.path().join("job.py");
    fs::write(&script, "pass").unwrap();
    let prepared = PreparedRun::prepare(&script, &[], &[], None).unwrap();
    let mut calls = 0;
    let outcome = runs::run(
        &f.storage,
        id("default"),
        prepared,
        options(),
        || {
            calls += 1;
            Err(Error::CredentialTimeout(30))
        },
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(outcome.exit_code, 1);
    assert_eq!(outcome.cleanup, runs::Cleanup::NotAllocated);
    assert_eq!(
        outcome.execution_error.as_deref(),
        Some(Error::CredentialTimeout(30).to_string().as_str())
    );
    assert!(outcome.cleanup_error.is_none());
    assert!(
        runs::pending(&f.storage, &id("default"))
            .unwrap()
            .is_empty()
    );
}
#[test]
fn cleanup_cannot_release_a_conflicting_named_resource() {
    let f = Fixture::new(TokenStorageMode::File);
    f.storage
        .save_session(&id("default"), session("same"))
        .unwrap();
    let service = SessionService::new(
        f.storage.clone(),
        id("default"),
        ColabApi::new(ApiConfig::default(), Secret::new("not-sent")).unwrap(),
    );
    // The run did not allocate this resource and has no recorded endpoint to release.
    assert_eq!(
        service.cleanup_owned("other-run", "same", None).unwrap(),
        None
    );
    assert!(
        f.storage
            .sessions(&id("default"))
            .unwrap()
            .contains_key("same")
    );
}
#[test]
fn preparation_rejects_bad_environment_and_limits_without_network() {
    let f = Fixture::new(TokenStorageMode::File);
    let script = f.temp.path().join("job.py");
    fs::write(&script, "pass").unwrap();
    assert!(PreparedRun::prepare(&script, &[], &["MISSING".into()], None).is_err());
    assert!(PreparedRun::prepare(&script, &[], &[], Some("bad\nname")).is_err());
    assert!(PreparedRun::prepare(&f.temp.path().join("absent"), &[], &[], None).is_err());
    let bytes = fs::read(script).unwrap();
    assert_eq!(bytes, b"pass");
}

#[test]
fn cli_run_requires_a_script_instead_of_panicking() {
    let temp = tempfile::tempdir().unwrap();
    let binary = std::path::Path::new(env!("CARGO_BIN_EXE_colab"));
    let renamed = temp.path().join(format!(
        "colab-release-fixture{}",
        std::env::consts::EXE_SUFFIX
    ));
    fs::copy(binary, &renamed).unwrap();
    let config = temp.path().join("must-not-initialize");
    for executable in [binary, renamed.as_path()] {
        let output = std::process::Command::new(executable)
            .arg("--config-dir")
            .arg(&config)
            .arg("run")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Missing argument 'script'."));
        assert!(!config.exists());
    }
}
