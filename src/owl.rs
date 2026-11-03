//! Ask Owl (~/versioned/personal/owl) about a passage. Owl runs as a child process in
//! raw streaming mode: stdout carries only the answer, stderr carries status/errors.
//! Internal questions go to the local Ollama model, external ones to OpenAI Codex.
//! The classifier agent is used because it sends no tools, so Owl never waits on a
//! tool approval this app cannot show.

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
};

pub enum Msg {
    Chunk(String),
    /// Finished; `Err` holds a readable reason (exit status plus the end of stderr).
    Done(Result<(), String>),
}

/// Handle to a running ask, so it can be cancelled.
#[derive(Clone)]
pub struct Running(Arc<Mutex<Option<Child>>>);

impl Running {
    pub fn cancel(&self) {
        if let Some(mut child) = self.0.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The Owl binary to use, only when settings.json opts in (`"owl": "auto"` or a path).
pub fn configured(setting: Option<&str>) -> Option<PathBuf> {
    match setting.map(str::trim) {
        None | Some("") => None,
        Some("auto") => find(),
        Some(path) => {
            let p = PathBuf::from(path);
            p.is_file().then_some(p)
        }
    }
}

/// Finder-launched apps get a minimal PATH, so also look where Go and Homebrew install.
pub fn find() -> Option<PathBuf> {
    let home = std::env::home_dir().unwrap_or_default();
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend([home.join("go/bin"), "/opt/homebrew/bin".into(), "/usr/local/bin".into(), home.join(".local/bin")]);
    dirs.into_iter().map(|d| d.join("owl")).find(|p| p.is_file())
}

pub const SYSTEM: &str = "You are helping someone review a document. They quote a passage and ask a question about it. \
Answer directly and concisely in plain text. Reason from the passage and your general knowledge; \
make sensible inferences, and say briefly when something is an assumption rather than stated in the passage.";

pub fn prompt(document: &str, page: usize, quote: &str, question: &str) -> String {
    format!("Document: {document}, page {page}\n\nPassage:\n\"\"\"\n{quote}\n\"\"\"\n\nQuestion: {question}")
}

pub fn args(external: bool, prompt: &str) -> Vec<String> {
    let mut a: Vec<String> = vec!["-raw".into(), "-stream".into()];
    if external {
        a.extend(["-model".into(), "codex".into()]);
    } else {
        a.extend(["-model".into(), "ollama".into(), "-thinking=false".into()]);
    }
    a.extend([
        "-agent".into(),
        "classifier".into(),
        "-history".into(),
        "0".into(),
        "-context_name".into(),
        "pdf-review".into(),
        "-system".into(),
        SYSTEM.into(),
        "-prompt".into(),
        prompt.into(),
    ]);
    a
}

/// Start Owl; answer chunks and the final result arrive on `tx`.
pub fn spawn(owl: &Path, external: bool, prompt: &str, tx: futures::channel::mpsc::UnboundedSender<Msg>) -> Result<Running, String> {
    let home = std::env::home_dir().unwrap_or_else(|| "/".into());
    let mut child = Command::new(owl)
        .args(args(external, prompt))
        .current_dir(home) // Owl reads .env from the working directory
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not start owl: {e}"))?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let running = Running(Arc::new(Mutex::new(Some(child))));

    let err_buf = Arc::new(Mutex::new(String::new()));
    let err_reader = {
        let err_buf = err_buf.clone();
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            *err_buf.lock().unwrap() = s;
        })
    };
    let handle = running.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        let mut pending: Vec<u8> = vec![]; // bytes of a UTF-8 char split across reads
        loop {
            match stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    pending.extend_from_slice(&buf[..n]);
                    let valid = match std::str::from_utf8(&pending) {
                        Ok(s) => s.len(),
                        Err(e) => e.valid_up_to(),
                    };
                    if valid > 0 {
                        let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
                        pending.drain(..valid);
                        if tx.unbounded_send(Msg::Chunk(text)).is_err() {
                            break;
                        }
                    }
                }
            }
        }
        let _ = err_reader.join();
        let status = handle.0.lock().unwrap().take().map(|mut c| c.wait());
        let result = match status {
            None => Err("Cancelled".to_string()), // killed by cancel()
            Some(Ok(s)) if s.success() => Ok(()),
            Some(Ok(s)) => Err(explain(&err_buf.lock().unwrap(), &s.to_string())),
            Some(Err(e)) => Err(e.to_string()),
        };
        let _ = tx.unbounded_send(Msg::Done(result));
    });
    Ok(running)
}

/// A readable reason from Owl's stderr: the panic message rather than the stack trace,
/// with known problems spelled out.
fn explain(stderr: &str, status: &str) -> String {
    let lines: Vec<&str> = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let main = lines
        .iter()
        .find_map(|l| l.strip_prefix("panic: "))
        .or_else(|| lines.iter().rev().find(|l| !l.starts_with('/') && !l.starts_with("goroutine") && !l.ends_with(')')).copied())
        .unwrap_or("")
        .to_string();
    if main.contains("oauth refresh failed") || main.contains("no codex oauth token") {
        return "Your Codex login has expired. In a terminal, run: owl -auth openai -login".into();
    }
    if main.contains("connection refused") && main.contains("11434") {
        return "Ollama isn't running. Start it (ollama serve) and ask again.".into();
    }
    if main.is_empty() { format!("owl exited with {status}") } else { main }
}

#[cfg(test)]
mod tests {
    #[test]
    fn explains_failures() {
        let trace = "panic: could not resolve openai auth: openai oauth refresh failed: oauth refresh failed with status 401\n\ngoroutine 1 [running]:\nmain.main()\n\t/Users/x/owl/src/main.go:310 +0x18f4\n";
        assert!(super::explain(trace, "exit status: 2").contains("owl -auth openai -login"));
        assert_eq!(super::explain("panic: boom\ngoroutine 1 [running]:\n", "x"), "boom");
        assert_eq!(super::explain("some error\n", "x"), "some error");
        assert_eq!(super::explain("", "exit status: 3"), "owl exited with exit status: 3");
    }

    use super::*;

    #[test]
    fn args_pick_model() {
        let i = args(false, "q");
        assert!(i.windows(2).any(|w| w == ["-model", "ollama"]));
        assert!(i.contains(&"-thinking=false".to_string()));
        let e = args(true, "q");
        assert!(e.windows(2).any(|w| w == ["-model", "codex"]));
        for a in [i, e] {
            assert!(a.windows(2).any(|w| w == ["-agent", "classifier"]), "no tools");
            assert!(a.windows(2).any(|w| w == ["-history", "0"]));
            assert_eq!(a.last().unwrap(), "q");
        }
    }

    #[test]
    fn off_unless_configured() {
        assert_eq!(configured(None), None);
        assert_eq!(configured(Some("  ")), None);
        assert_eq!(configured(Some("/no/such/owl")), None);
        let me = std::env::current_exe().unwrap();
        assert_eq!(configured(Some(me.to_str().unwrap())), Some(me));
    }

    #[test]
    #[cfg(unix)] // uses a shell script as a fake owl
    fn streams_and_reports_errors() {
        // a stand-in "owl" script: prints in two writes with a split UTF-8 char, then fails
        let dir = std::env::temp_dir().join(format!("pdf-review-owl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("owl");
        std::fs::write(&fake, "#!/bin/sh\nprintf 'h\\303'\nsleep 0.1\nprintf '\\245j'\necho 'boom' >&2\nexit 3\n").unwrap();
        std::process::Command::new("chmod").arg("+x").arg(&fake).status().unwrap();
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        spawn(&fake, false, "q", tx).unwrap();
        let mut text = String::new();
        let result = futures::executor::block_on(async {
            use futures::StreamExt;
            loop {
                match rx.next().await {
                    Some(Msg::Chunk(c)) => text.push_str(&c),
                    Some(Msg::Done(r)) => break r,
                    None => panic!("channel closed"),
                }
            }
        });
        assert_eq!(text, "håj");
        assert_eq!(result, Err("boom".to_string()));
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Real owl + local Ollama. Run with: cargo test owl::live -- --ignored
    #[test]
    #[ignore]
    fn internal_round_trip() {
        let owl = find().expect("owl installed");
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let p = prompt("test.pdf", 1, "Door readers have a 4G backup.", "What does the backup protect against? One short sentence.");
        spawn(&owl, false, &p, tx).unwrap();
        let mut text = String::new();
        let r = futures::executor::block_on(async {
            use futures::StreamExt;
            loop {
                match rx.next().await {
                    Some(Msg::Chunk(c)) => text.push_str(&c),
                    Some(Msg::Done(r)) => break r,
                    None => panic!("closed"),
                }
            }
        });
        println!("answer: {text}");
        assert_eq!(r, Ok(()));
        assert!(!text.trim().is_empty());
    }
}
