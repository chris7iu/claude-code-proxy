//! Every terminal mutation is inside a private PTY child, never this test runner's tty.
use super::*;
use crate::providers::antigravity::{auth::Store, config::Config};
use axum::{Json, Router, routing::post};
use serde_json::json;
use std::{
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{fs::OpenOptionsExt, process::CommandExt},
    },
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

const HELPER: &str =
    "providers::antigravity::manual::terminal::tests::antigravity_manual_pty_child";
const CODE: &str = "4/0manual-private-code";
const PROMPT: &str = "Paste the displayed authorization code";

fn termios(fd: libc::c_int) -> libc::termios {
    let mut value = std::mem::MaybeUninit::uninit();
    assert_eq!(unsafe { libc::tcgetattr(fd, value.as_mut_ptr()) }, 0);
    unsafe { value.assume_init() }
}
fn same_termios(a: &libc::termios, b: &libc::termios) -> bool {
    a.c_iflag == b.c_iflag
        && a.c_oflag == b.c_oflag
        && a.c_cflag == b.c_cflag
        && a.c_lflag == b.c_lflag
        && a.c_cc == b.c_cc
        && unsafe {
            libc::cfgetispeed(a) == libc::cfgetispeed(b)
                && libc::cfgetospeed(a) == libc::cfgetospeed(b)
        }
}

#[test]
fn antigravity_manual_pty_child() {
    let Ok(mode) = std::env::var("CCP_MANUAL_TEST_CHILD") else {
        return;
    };
    if mode == "trace" {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .without_time()
            .init();
    }
    if mode == "background" {
        let status = Command::new(std::env::current_exe().unwrap())
            .args([HELPER, "--exact", "--nocapture"])
            .env("CCP_MANUAL_TEST_CHILD", "background-child")
            .process_group(0)
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    if mode == "hangup" {
        // Exercise EOF/EIO rather than the kernel's default SIGHUP termination.
        unsafe {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
        }
    }
    let before = termios(0);
    if mode == "drop" {
        let result = std::panic::catch_unwind(|| {
            let mut terminal = Terminal::open().unwrap();
            terminal.hide_input().unwrap();
            panic!("synthetic terminal unwind");
        });
        assert!(result.is_err());
        assert!(same_termios(&before, &termios(0)));
        eprintln!("manual-helper:drop-restored");
        return;
    }
    let flags = unsafe { libc::fcntl(0, libc::F_GETFL) };
    let timeout = if mode == "deadline" || mode == "continuous" || mode == "blocked-output" {
        Duration::from_millis(300)
    } else {
        Duration::from_secs(4)
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        let hold = mode == "hold";
        let server = crate::providers::antigravity_http_tests::Server::start(
            Router::new().route("/token", post(move |axum::extract::Form(form): axum::extract::Form<std::collections::HashMap<String, String>>| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(form["code"], CODE);
                    assert_eq!(form["redirect_uri"], "https://antigravity.google/oauth-callback");
                    assert!(!form["code_verifier"].is_empty());
                    std::fs::write(std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join("synthetic-verifier"), &form["code_verifier"]).unwrap();
                    assert!(same_termios(&before, &termios(0)), "exchange before terminal restoration");
                    eprintln!("manual-helper:exchange");
                    if hold { std::future::pending::<()>().await; }
                    Json(json!({"access_token":"manual-private-access", "refresh_token":"manual-private-refresh", "expires_in":3600}))
                }
            }))
        ).await;
        let directory = tempfile::tempdir().unwrap();
        let committed = mode.starts_with("committed-");
        let path = if committed {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join("auth.json")
        } else { directory.path().join("auth.json") };
        let mut store = Store::new(path);
        if committed {
            store = store.with_before_save(Arc::new(|| {
                // Fill only this test PTY's output queue before the durable
                // commit. The parent watches auth.json without draining output.
                let mut tty = std::fs::OpenOptions::new().write(true)
                    .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
                    .open("/dev/tty").unwrap();
                for _ in 0..128 {
                    match tty.write(&[b'x'; 1024]) {
                        Ok(_) => (),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) => panic!("test PTY fill failed: {error}"),
                    }
                }
            }));
        }
        let auth = Auth::new(Config {
            token_url: server.url.join("/token").unwrap(),
            client_id: Some(if mode == "blocked-output" || mode == "long-output" { "x".repeat(65536) } else { "synthetic-public-client".into() }),
            project: Some("synthetic-project".into()),
            ..Config::default()
        }, store).unwrap();
        let result = if mode == "cleanup-error" {
            let mut terminal = Terminal::open().unwrap();
            auth.login_manual_with_completion(|_, _| async {
                terminal.hide_input()?;
                let deadline = tokio::time::Instant::now() + timeout;
                terminal.output(PROMPT.as_bytes(), deadline).await?;
                let input = terminal.input(deadline).await?;
                // A real tcflush/tcsetattr failure, confined to this owned fd.
                // Restore the real PTY afterwards so this test cannot strand it.
                let tty = std::mem::replace(&mut terminal.file, File::open("/dev/null").unwrap());
                let failed_cleanup = terminal.restore();
                terminal.file = tty;
                terminal.restore()?;
                failed_cleanup?;
                Ok(input)
            }).await
        } else {
            login(&auth, timeout).await
        };
        if mode == "background-child" {
            assert!(result.unwrap_err().contains("foreground"));
            assert!(!directory.path().join("auth.json").exists());
            eprintln!("manual-helper:background-refused");
        } else {
            match result {
                Ok(()) => {
                    assert_eq!(requests.load(Ordering::SeqCst), 1);
                    assert!(auth.store.load().unwrap().is_some());
                    eprintln!("manual-helper:success");
                }
                Err(error) => {
                    assert!(auth.store.load().unwrap().is_none());
                    assert_eq!(requests.load(Ordering::SeqCst), usize::from(hold));
                    eprintln!("manual-helper:error:{error}");
                }
            }
        }
    });
    if mode != "hangup" {
        assert!(
            same_termios(&before, &termios(0)),
            "terminal mode not restored exactly"
        );
        assert_eq!(
            unsafe { libc::fcntl(0, libc::F_GETFL) },
            flags,
            "stdin file-description flags changed"
        );
        eprintln!("manual-helper:restored");
    }
}

struct PtyChild {
    child: Child,
    master: Option<File>,
    slave: File,
    before: libc::termios,
    output: Vec<u8>,
    _home: tempfile::TempDir,
}
impl Drop for PtyChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl PtyChild {
    fn start(mode: &str) -> Self {
        let (mut master, mut slave) = (-1, -1);
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0,
            "mandatory PTY fixture unavailable"
        );
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        for file in [&master, &slave] {
            assert_eq!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        assert_eq!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        let mut before = termios(slave.as_raw_fd());
        // An unusual prior mode catches "restore default/raw off" shortcuts.
        before.c_lflag |= libc::ECHONL;
        before.c_iflag |= libc::IXOFF;
        before.c_oflag |= libc::OPOST | libc::ONLCR;
        assert_eq!(
            unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &before) },
            0
        );
        let home = tempfile::tempdir().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([HELPER, "--exact", "--nocapture"])
            .env_clear()
            .env("HOME", home.path())
            .env("CCP_CONFIG_DIR", home.path())
            .env("CCP_MANUAL_TEST_CHILD", mode)
            .env("RUST_LOG", "trace")
            .env("CCP_LOG_STDERR", "1")
            .env("CCP_LOG_VERBOSE", "1")
            .env("CCP_TRAFFIC_LOG", "1")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        Self {
            child,
            master: Some(master),
            slave,
            before,
            output: Vec::new(),
            _home: home,
        }
    }
    fn collect(&mut self) {
        let Some(master) = self.master.as_mut() else {
            return;
        };
        let mut buf = [0u8; 4096];
        // Bound each collection turn, including a continuously writing child.
        for _ in 0..16 {
            match master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => self.output.extend_from_slice(&buf[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.raw_os_error() == Some(libc::EIO) =>
                {
                    break;
                }
                Err(e) => panic!("PTY read: {e}"),
            }
        }
        assert!(self.output.len() < 256 * 1024, "unbounded terminal output");
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output).into_owned()
    }
    fn until(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.collect();
            if self.text().contains(text) {
                return;
            }
            assert!(Instant::now() < deadline, "missing {text}: {}", self.text());
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "child exited before {text}: {}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn send(&mut self, bytes: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut rest = bytes;
        while !rest.is_empty() {
            match self.master.as_mut().unwrap().write(rest) {
                Ok(n) if n > 0 => rest = &rest[n..],
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                other => panic!("PTY write failed: {other:?}"),
            }
            assert!(Instant::now() < deadline, "PTY producer blocked");
        }
    }
    fn finish(&mut self, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.collect();
            if let Some(status) = self.child.try_wait().unwrap() {
                self.collect();
                assert!(status.success(), "{}", self.text());
                assert!(self.text().contains(expected), "{}", self.text());
                assert!(same_termios(&self.before, &termios(self.slave.as_raw_fd())));
                for secret in [
                    CODE,
                    "manual-private-access",
                    "manual-private-refresh",
                    "QUEUED_PRIVATE_TAIL",
                ] {
                    assert!(
                        !self.text().contains(secret),
                        "terminal disclosed input or credential"
                    );
                }
                if let Ok(verifier) =
                    std::fs::read_to_string(self._home.path().join("synthetic-verifier"))
                {
                    assert!(
                        !self.text().contains(&verifier),
                        "terminal/log disclosed verifier"
                    );
                }
                let mut pending: libc::c_int = -1;
                assert_eq!(
                    unsafe { libc::ioctl(self.slave.as_raw_fd(), libc::FIONREAD, &mut pending) },
                    0
                );
                assert_eq!(pending, 0, "queued input survived cleanup");
                return;
            }
            assert!(
                Instant::now() < deadline,
                "child/runtime did not exit: {}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn signal(&self, signal: libc::c_int) {
        assert_eq!(
            unsafe { libc::kill(self.child.id() as libc::pid_t, signal) },
            0
        );
    }
}

#[test]
fn antigravity_manual_pty_hidden_code_editing_crlf_and_queued_tail() {
    for input in [
        format!("{CODE}\r\nQUEUED_PRIVATE_TAIL\n").into_bytes(),
        format!("{CODE}x\x08y\x7f\nQUEUED_PRIVATE_TAIL\n").into_bytes(),
        format!("\x1b[200~{CODE}\x1b[201~\rQUEUED_PRIVATE_TAIL\n").into_bytes(),
        format!("\r\r\r{CODE}\r").into_bytes(),
    ] {
        let mut child = PtyChild::start("success");
        child.until(PROMPT);
        let raw = termios(child.slave.as_raw_fd());
        assert_eq!(
            raw.c_lflag & (libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG),
            0
        );
        assert_eq!(raw.c_oflag, child.before.c_oflag);
        child.send(&input);
        child.finish("manual-helper:success");
    }
}

#[test]
fn antigravity_manual_pty_full_url_and_exact_authorization_output() {
    let mut child = PtyChild::start("trace");
    child.until(PROMPT);
    let text = child.text();
    let line = text
        .lines()
        .find(|line| line.starts_with("https://accounts.google.com/"))
        .unwrap();
    let authorization = url::Url::parse(line).unwrap();
    assert_eq!(authorization.as_str(), line);
    let pairs = authorization
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(
        pairs["redirect_uri"],
        "https://antigravity.google/oauth-callback"
    );
    let mut completed = url::Url::parse("https://antigravity.google/oauth-callback").unwrap();
    completed
        .query_pairs_mut()
        .extend_pairs([("state", pairs["state"].as_ref()), ("code", CODE)]);
    child.send(format!("{completed}\n").as_bytes());
    child.finish("manual-helper:success");
}

#[test]
fn antigravity_manual_pty_invalid_input_cancels_and_flushes() {
    let cases = vec![
        vec![3],
        vec![4],
        vec![26],
        vec![28],
        vec![0xff],
        vec![0xc3],
        vec![0],
        b"\r\r\r\r".to_vec(),
        b"\x1b[200~x\x1b[200~".to_vec(),
        b"\x1b[201~".to_vec(),
        b"\x1b[999999999999999999".to_vec(),
        b"\x1b[200~x\x1b[20x".to_vec(),
        vec![b'x'; 8193],
    ];
    for mut input in cases {
        let mut child = PtyChild::start("invalid");
        child.until(PROMPT);
        input.extend_from_slice(b"QUEUED_PRIVATE_TAIL\n");
        child.send(&input);
        child.finish("manual-helper:error:");
    }
}

#[test]
fn antigravity_manual_pty_deadline_partial_escape_and_continuous_input() {
    for input in [b"".as_slice(), b"\x1b[20", b"\x1b[200~"] {
        let mut child = PtyChild::start("deadline");
        child.until(PROMPT);
        child.send(input);
        child.finish("manual-helper:error:Manual OAuth input timed out");
    }
    let mut child = PtyChild::start("continuous");
    child.until(PROMPT);
    let deadline = Instant::now() + Duration::from_secs(2);
    while child.child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        // Constant activity with a bounded logical line must not reset the deadline.
        let _ = child.master.as_mut().unwrap().write(b"x\x08");
        std::thread::sleep(Duration::from_millis(1));
    }
    child.finish("manual-helper:error:Manual OAuth input timed out");
}

#[test]
fn antigravity_manual_pty_signals_cancel_input_and_held_exchange() {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        for hold in [false, true] {
            let mut child = PtyChild::start(if hold { "hold" } else { "invalid" });
            child.until(PROMPT);
            if hold {
                child.send(format!("{CODE}\n").as_bytes());
                child.until("manual-helper:exchange");
            }
            child.signal(signal);
            child.finish("manual-helper:error:Manual OAuth login cancelled");
        }
    }
}

#[test]
fn antigravity_manual_pty_background_refused_before_url_or_mutation() {
    let mut child = PtyChild::start("background");
    child.finish("manual-helper:background-refused");
    assert!(!child.text().contains("accounts.google.com"));
}

#[test]
fn antigravity_manual_pty_output_backpressure_has_absolute_deadline() {
    let mut child = PtyChild::start("blocked-output");
    // Do not consume the PTY output until the short input/output deadline expires.
    std::thread::sleep(Duration::from_millis(600));
    assert!(
        same_termios(&child.before, &termios(child.slave.as_raw_fd())),
        "output backpressure prevented timely cleanup"
    );
    child.finish("manual-helper:error:Manual OAuth input timed out");
}

#[test]
fn antigravity_manual_pty_committed_login_is_success_despite_confirmation_signal_or_timeout() {
    for signal in [Some(libc::SIGINT), Some(libc::SIGTERM), None] {
        let mut child = PtyChild::start("committed-confirmation");
        child.until(PROMPT);
        child.send(format!("{CODE}\n").as_bytes());
        let saved = child._home.path().join("auth.json");
        let deadline = Instant::now() + Duration::from_secs(3);
        while !saved.exists() {
            assert!(
                Instant::now() < deadline,
                "fixture never committed credentials"
            );
            assert!(
                child.child.try_wait().unwrap().is_none(),
                "fixture exited before commit"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        if let Some(signal) = signal {
            child.signal(signal);
        } else {
            std::thread::sleep(Duration::from_millis(2500));
        }
        child.finish("manual-helper:success");
        assert!(!child.text().contains("login cancelled"));
        assert!(!child.text().contains("input timed out"));
        assert!(saved.exists());
    }
}

#[test]
fn antigravity_manual_pty_partial_output_is_complete_and_cleanup_failure_never_exchanges() {
    let mut child = PtyChild::start("long-output");
    child.until(PROMPT);
    let text = child.text();
    let line = text
        .lines()
        .find(|line| line.starts_with("https://accounts.google.com/"))
        .unwrap();
    let url = url::Url::parse(line).unwrap();
    assert_eq!(url.as_str(), line);
    assert_eq!(
        url.query_pairs()
            .find(|(key, _)| key == "client_id")
            .unwrap()
            .1,
        "x".repeat(65536)
    );
    child.send(format!("{CODE}\n").as_bytes());
    child.finish("manual-helper:success");

    let mut child = PtyChild::start("cleanup-error");
    child.until(PROMPT);
    child.send(format!("{CODE}\nQUEUED_PRIVATE_TAIL\n").as_bytes());
    child.finish("manual-helper:error:Manual OAuth terminal cleanup failed");
    assert!(!child.text().contains("manual-helper:exchange"));

    let mut child = PtyChild::start("drop");
    child.finish("manual-helper:drop-restored");
}

#[test]
fn antigravity_manual_pty_hangup_exits_without_reader_or_runtime_hang() {
    let mut child = PtyChild::start("hangup");
    child.until(PROMPT);
    drop(child.master.take());
    let deadline = Instant::now() + Duration::from_secs(3);
    while child.child.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "hangup did not terminate input reader"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
