//! Terminal editing stays on the main thread; a worker owns the socket.
use colabcli_rust::{
    Error, Result,
    execution::{
        self, ExecuteOptions,
        repl::{self, Command, Event},
    },
    jupyter::{ExecutionStatus, ExecutionTimeout},
    runs,
    sessions::SessionService,
};
use rustyline::{
    Cmd, Editor, Helper, KeyCode, KeyEvent, Modifiers, completion::Completer, error::ReadlineError,
    highlight::Highlighter, hint::Hinter, history::MemHistory, validate::Validator,
};
use std::{
    borrow::Cow,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

struct PythonHelper {
    color: bool,
}
impl Helper for PythonHelper {}
impl Completer for PythonHelper {
    type Candidate = String;
}
impl Hinter for PythonHelper {
    type Hint = String;
}
impl Validator for PythonHelper {}
impl Highlighter for PythonHelper {
    fn highlight<'l>(&self, line: &'l str, _: usize) -> Cow<'l, str> {
        if self.color {
            Cow::Owned(highlight_python(line))
        } else {
            Cow::Borrowed(line)
        }
    }
    fn highlight_char(&self, _: &str, _: usize, _: rustyline::highlight::CmdKind) -> bool {
        self.color
    }
}
// Lightweight lexical highlighting; unlike parsing it also handles incomplete input.
fn highlight_python(source: &str) -> String {
    let mut output = String::new();
    let mut at = 0;
    while at < source.len() {
        let start = at;
        let ch = source[at..].chars().next().unwrap();
        at += ch.len_utf8();
        let color = if ch == '#' {
            while at < source.len() && source.as_bytes()[at] != b'\n' {
                at += source[at..].chars().next().unwrap().len_utf8();
            }
            Some("90")
        } else if ch == '\'' || ch == '"' {
            let triple = source[start..].starts_with(&ch.to_string().repeat(3));
            if triple {
                at = start + 3;
            }
            while at < source.len() {
                let c = source[at..].chars().next().unwrap();
                at += c.len_utf8();
                if c == '\\' {
                    if at < source.len() {
                        at += source[at..].chars().next().unwrap().len_utf8();
                    }
                } else if c == ch {
                    if !triple {
                        break;
                    }
                    if source[at..].starts_with(&ch.to_string().repeat(2)) {
                        at += 2;
                        break;
                    }
                }
            }
            Some("32")
        } else if ch.is_alphabetic() || ch == '_' {
            while at < source.len() {
                let c = source[at..].chars().next().unwrap();
                if c.is_alphanumeric() || c == '_' {
                    at += c.len_utf8();
                } else {
                    break;
                }
            }
            if [
                "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class",
                "continue", "def", "del", "elif", "else", "except", "finally", "for", "from",
                "global", "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass",
                "raise", "return", "try", "while", "with", "yield", "match", "case",
            ]
            .contains(&&source[start..at])
            {
                Some("34")
            } else {
                None
            }
        } else if ch.is_ascii_digit() {
            while at < source.len()
                && (source.as_bytes()[at].is_ascii_alphanumeric()
                    || b"._".contains(&source.as_bytes()[at]))
            {
                at += 1;
            }
            Some("36")
        } else {
            None
        };
        if let Some(color) = color {
            output.push_str(&format!("\x1b[{color}m"));
        }
        output.push_str(&source[start..at]);
        if color.is_some() {
            output.push_str("\x1b[0m");
        }
    }
    output
}
enum Wire {
    Event(Event),
    Done(Result<()>),
}

pub fn run(
    service: &SessionService,
    name: &str,
    http_timeout: Duration,
    output_image: Option<&Path>,
) -> Result<()> {
    // Create the editor only while reading input. Rustyline installs SIGINT at
    // construction, so dropping it before execution restores SignalGuard's
    // handler and lets Ctrl-C interrupt the remote request instead of the editor.
    let mut history = std::collections::VecDeque::new();
    #[cfg(unix)]
    // SAFETY: querying the current thread has no preconditions.
    let editor_thread = unsafe { libc::pthread_self() } as usize;
    let guard = runs::SignalGuard::install()?;
    let editing = Arc::new(AtomicBool::new(false));
    let (commands, input) = mpsc::sync_channel(1);
    let (events, output) = mpsc::sync_channel(4);
    std::thread::scope(|scope| {
        let cancellation = guard.flag.clone();
        let worker_editing = editing.clone();
        scope.spawn(move || {
            let result = repl::serve(
                service,
                Some(name),
                input,
                ExecuteOptions {
                    timeout: ExecutionTimeout::Idle(None),
                    http_timeout,
                    kernel_id: None,
                    cancellation: Some(cancellation),
                },
                |event| {
                    events
                        .send(Wire::Event(event))
                        .map_err(|_| Error::Run("REPL 输出端已关闭"))
                },
            );
            let failed = result.is_err();
            let _ = events.send(Wire::Done(result));
            if failed {
                // Rustyline installs its own SIGINT handler only during readline.
                // Repeated wake-ups cover the short interval before that handler
                // is installed; Done is queued BEFORE waking, so no code is sent.
                #[cfg(unix)]
                for _ in 0..40 {
                    if !worker_editing.load(Ordering::SeqCst) {
                        break;
                    }
                    // SAFETY: the scoped main thread stays alive until this worker
                    // is joined. Only wake the thread that owns terminal input.
                    let _ = unsafe {
                        libc::pthread_kill(editor_thread as libc::pthread_t, libc::SIGINT)
                    };
                    std::thread::sleep(Duration::from_millis(25));
                }
                #[cfg(not(unix))]
                if worker_editing.load(Ordering::SeqCst) {
                    eprintln!("[colab] REPL connection ended; press Enter or Ctrl-D to exit.");
                }
            }
        });
        let result = (|| {
            // Initialization may stream output before Ready.
            loop {
                match output
                    .recv()
                    .map_err(|_| Error::Run("REPL 工作线程已退出"))?
                {
                    Wire::Event(Event::Ready) => break,
                    Wire::Event(Event::Output(change)) => {
                        execution::render_output_with_image_target(&change, output_image)?
                    }
                    Wire::Event(Event::Finished(result)) => {
                        if let Some(error) = result.error {
                            eprintln!("{error}");
                        }
                    }
                    Wire::Done(result) => return result,
                }
            }
            println!("Python 3 (Google Colab Runtime)\nType /quit to exit.");
            loop {
                editing.store(true, Ordering::SeqCst);
                if let Ok(Wire::Done(result)) = output.try_recv() {
                    editing.store(false, Ordering::SeqCst);
                    return result;
                }
                let mut editor = make_editor(&history)?;
                let read = editor.readline(">>> ");
                drop(editor);
                editing.store(false, Ordering::SeqCst);
                if let Ok(Wire::Done(result)) = output.try_recv() {
                    return result;
                }
                let line = match read {
                    Ok(line) => line,
                    Err(error) if is_input_interrupt(&error) => {
                        let signal = guard.flag.load(Ordering::SeqCst);
                        if signal != 0 && signal != signal_hook::consts::SIGINT as usize {
                            return Err(Error::CliExit(128 + signal as i32));
                        }
                        let _ = guard.flag.compare_exchange(
                            signal_hook::consts::SIGINT as usize,
                            0,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        );
                        continue;
                    }
                    Err(ReadlineError::Eof) => break,
                    Err(_) => return Err(Error::Run("读取 REPL 输入失败")),
                };
                if let Ok(Wire::Done(result)) = output.try_recv() {
                    return result;
                }
                let line = line.trim();
                if ["/quit", "quit()", "exit()"]
                    .iter()
                    .any(|exit| line.eq_ignore_ascii_case(exit))
                {
                    break;
                }
                if line.is_empty() {
                    continue;
                }
                if history.back().is_none_or(|previous| previous != line) {
                    if history.len() == 100 {
                        history.pop_front();
                    }
                    history.push_back(line.to_owned());
                }
                commands
                    .send(Command::Execute(line.into()))
                    .map_err(|_| Error::Run("REPL 连接已关闭"))?;
                loop {
                    match output
                        .recv()
                        .map_err(|_| Error::Run("REPL 工作线程已退出"))?
                    {
                        Wire::Event(Event::Output(change)) => {
                            execution::render_output_with_image_target(&change, output_image)?
                        }
                        Wire::Event(Event::Finished(result)) => {
                            if let Some(error) = &result.history_error {
                                eprintln!("历史记录失败: {error}");
                            }
                            if let Some(error) = &result.error {
                                eprintln!("{error}");
                            }
                            if let Some(path) = output_image {
                                save_image(std::slice::from_ref(&result), path)?;
                            }
                            if result.status == ExecutionStatus::Unknown {
                                return Err(Error::ExecutionUnknown(result.msg_id));
                            }
                            println!();
                            break;
                        }
                        Wire::Done(result) => return result,
                        Wire::Event(Event::Ready) => {
                            return Err(Error::Run("重复 REPL 初始化事件"));
                        }
                    }
                }
            }
            commands
                .send(Command::Stop)
                .map_err(|_| Error::Run("REPL 连接已关闭"))?;
            loop {
                if let Wire::Done(result) = output
                    .recv()
                    .map_err(|_| Error::Run("REPL 工作线程已退出"))?
                {
                    result?;
                    break;
                }
            }
            println!("[*] Goodbye!");
            Ok(())
        })();
        // Unblock both bounded channels before joining a worker after an I/O error.
        editing.store(false, Ordering::SeqCst);
        guard
            .flag
            .store(signal_hook::consts::SIGTERM as usize, Ordering::SeqCst);
        drop(commands);
        drop(output);
        result
    })
}

fn make_editor(
    history: &std::collections::VecDeque<String>,
) -> Result<Editor<PythonHelper, MemHistory>> {
    let mut editor = Editor::<PythonHelper, MemHistory>::new()
        .map_err(|_| Error::Run("无法初始化 REPL 终端"))?;
    editor.set_helper(Some(PythonHelper {
        color: std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").as_deref() != Ok("dumb"),
    }));
    editor.bind_sequence(KeyEvent(KeyCode::Enter, Modifiers::ALT), Cmd::Newline);
    editor.bind_sequence(KeyEvent::ctrl('j'), Cmd::Newline);
    for entry in history {
        editor
            .add_history_entry(entry.as_str())
            .map_err(|_| Error::Run("读取 REPL 内存历史失败"))?;
    }
    Ok(editor)
}

fn is_input_interrupt(error: &ReadlineError) -> bool {
    if matches!(error, ReadlineError::Interrupted) {
        return true;
    }
    #[cfg(unix)]
    if matches!(
        error,
        ReadlineError::Signal(rustyline::error::Signal::Interrupt)
    ) {
        return true;
    }
    false
}

pub fn save_image(results: &[colabcli_rust::jupyter::ExecutionResult], path: &Path) -> Result<()> {
    execution::save_last_image(results, path)?;
    if results.iter().flat_map(|r| &r.outputs).any(|o| {
        o.data["data"].get("image/png").is_some() || o.data["data"].get("image/jpeg").is_some()
    }) {
        println!("\n[Image saved to: {}]", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn python_highlighting_preserves_unicode_multiline_and_incomplete_source() {
        let source = "def 测试():\n    return '你好\\\'x' # 注释\n123.5\n'''多\n行'''\n\"未完成";
        let highlighted = highlight_python(source);
        let mut plain = String::new();
        let mut chars = highlighted.chars();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                for ch in chars.by_ref() {
                    if ch == 'm' {
                        break;
                    }
                }
            } else {
                plain.push(ch);
            }
        }
        assert_eq!(plain, source);
        assert!(highlighted.contains("\x1b[34mdef\x1b[0m"));
    }
}
