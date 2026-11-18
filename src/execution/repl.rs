//! One execution lease, kernel and socket for the lifetime of an interactive REPL.
use super::{ExecuteOptions, open_runtime};
use crate::{
    Error, Result,
    jupyter::{ExecutionResult, ExecutionStatus, KernelRest, output::OutputChange},
    sessions::SessionService,
};
use serde_json::json;
use std::{
    sync::mpsc::{Receiver, RecvTimeoutError},
    time::Duration,
};

pub enum Command {
    Execute(String),
    Stop,
}
pub enum Event {
    Ready,
    Output(OutputChange),
    Finished(ExecutionResult),
}

/// The caller owns input editing. A bounded channel keeps input/output memory
/// bounded and dropping the command sender closes the socket without deleting
/// the kernel or VM. No automatic reconnect or code replay occurs.
pub fn serve(
    service: &SessionService,
    name: Option<&str>,
    commands: Receiver<Command>,
    options: ExecuteOptions<'_>,
    mut emit: impl FnMut(Event) -> Result<()>,
) -> Result<()> {
    service.with_execution_session(name, |original, checkpoint| {
        let mut state = original.clone();
        let mut runtime = open_runtime(&mut state, &options, checkpoint)?;
        runtime.interrupt_on_sigint(KernelRest::new(&state, options.http_timeout)?, state.kernel_id.clone().unwrap(), Duration::from_secs(5))?;
        let setup = runtime.execute_with_checkpoint(
            "import os; os.makedirs('/content', exist_ok=True); os.chdir('/content')",
            options.timeout,
            |id| { state.running = Some(id.into()); checkpoint(state.clone()) },
            |change| emit(Event::Output(change.clone())),
        )?;
        if setup.status != ExecutionStatus::Unknown { state.running = None; }
        checkpoint(state.clone())?;
        if !setup.succeeded() {
            let unknown = setup.status == ExecutionStatus::Unknown;
            let id = setup.msg_id.clone();
            emit(Event::Finished(setup))?;
            return Err(if unknown { Error::ExecutionUnknown(id) } else { Error::ExecutionFailed });
        }
        service.record_event(&state.name, Some(&state.endpoint), "repl_started", json!({}), None)?;
        emit(Event::Ready)?;
        loop {
            // Idle Ctrl-C belongs to the editor. TERM/HUP closes without a new request.
            let signal = options.cancellation.as_ref().map_or(0, |flag| flag.load(std::sync::atomic::Ordering::SeqCst));
            if signal != 0 && signal != signal_hook::consts::SIGINT as usize {
                return Err(Error::CliExit(128 + signal as i32));
            }
            let code = match commands.recv_timeout(Duration::from_millis(25)) {
                Ok(Command::Execute(code)) => code,
                Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => { runtime.poll_idle()?; continue; }
            };
            if code.trim().is_empty() { continue; }
            let mut result = runtime.execute_with_checkpoint(
                &code, options.timeout,
                |id| {
                    state.running = Some(id.into());
                    state.last_execution = Some(("REPL".into(), None, chrono::Utc::now().to_rfc3339()));
                    checkpoint(state.clone())
                },
                |change| emit(Event::Output(change.clone())),
            )?;
            if result.status != ExecutionStatus::Unknown { state.running = None; }
            if let Err(error) = service.record_event(&state.name, Some(&state.endpoint), "execution", json!({
                "code":code,"outputs":result.outputs,"status":result.status,"msg_id":result.msg_id,
                "error":result.error,"source_file":"REPL","source":"interactive","via":"repl",
                "execution_count":result.reply.as_ref().and_then(|reply|reply.get("execution_count"))
            }), Some(&result.msg_id)) { result.history_error = Some(error.to_string()); }
            // A concurrent stop/restart must never be overwritten by this writer.
            checkpoint(state.clone())?;
            let unknown = result.status == ExecutionStatus::Unknown;
            let id = result.msg_id.clone();
            emit(Event::Finished(result))?;
            if unknown { return Err(Error::ExecutionUnknown(id)); }
        }
        runtime.close();
        service.record_event(&state.name, Some(&state.endpoint), "repl_stopped", json!({}), None)?;
        Ok(())
    })
}
