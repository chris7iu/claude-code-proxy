//! One owned nonblocking terminal, no stdin worker or general escape parser.
use super::super::{
    auth::{Auth, Store},
    config::Config,
    replay::Result,
};
use super::{INPUT_LIMIT, INVALID};
use std::{
    fs::{File, OpenOptions},
    io::{self, IsTerminal, Read, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    time::Duration,
};
use tokio::{
    signal::unix::{SignalKind, signal},
    time::Instant,
};

const CANCELLED: &str = "Manual OAuth login cancelled";
const TIMED_OUT: &str = "Manual OAuth input timed out";
const CLOSED: &str = "Manual OAuth terminal closed or unavailable";
const RESTORE_FAILED: &str = "Manual OAuth terminal cleanup failed; no further exchange attempted";
const PROMPT: &[u8] =
    b"\nPaste the displayed authorization code (hidden), then Enter; Ctrl-C cancels: ";
const NOTICE: &[u8] = b"Experimental manual Antigravity login. Open this URL in your browser and choose the intended account.\nThe configured OAuth client must permit https://antigravity.google/oauth-callback.\nCopy the CODE DISPLAYED by the page, not the address bar (the page removes the code from its URL).\nCancel if the browser reports an error (including redirect_uri_mismatch) or shows no code.\nNo browser is opened automatically; no callback listener, credential import, or fallback is used.\n";

pub(crate) enum CliOutcome {
    Success,
    HandledFailure,
}

/// Early refusal remains an ordinary CLI error. After terminal acquisition,
/// every failure is handled here, never by Rust main's blocking stderr report.
/// All auth/runtime/terminal guards are gone before returning the typed outcome.
pub(crate) fn run_cli() -> Result<CliOutcome> {
    let mut terminal = Terminal::open()?;
    let result = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => {
            let result = runtime.block_on(async {
                let auth = Auth::new(Config::load()?, Store::configured())?;
                login_on_terminal(&auth, &mut terminal, Duration::from_secs(180)).await
            });
            // No reactor-dependent reporting or async RAII survives this point.
            drop(runtime);
            result
        }
        Err(_) => Err("Manual OAuth runtime initialization failed"),
    };
    let outcome = match result {
        Ok(()) => CliOutcome::Success,
        Err(error) => {
            // Cleanup has already been attempted by the input/login core. A
            // retry is best effort; a failed report must not fall back to stderr.
            let error = terminal.restore().err().unwrap_or(error);
            terminal.report_error(error);
            CliOutcome::HandledFailure
        }
    };
    drop(terminal);
    Ok(outcome)
}

#[cfg(test)]
pub(crate) async fn login(auth: &Auth, timeout: Duration) -> Result<()> {
    let mut terminal = Terminal::open()?;
    login_on_terminal(auth, &mut terminal, timeout).await
}

async fn login_on_terminal(auth: &Auth, terminal: &mut Terminal, timeout: Duration) -> Result<()> {
    // Tokio registrations persist process-wide. Keep both receivers in the
    // select through exchange and confirmation, not just terminal input. The
    // post-runtime diagnostic uses its own synchronous deadline, not receivers.
    // This path is never installed by serve or automatic callback login.
    let mut interrupt =
        signal(SignalKind::interrupt()).map_err(|_| "Manual OAuth signal setup failed")?;
    let mut terminate =
        signal(SignalKind::terminate()).map_err(|_| "Manual OAuth signal setup failed")?;
    let result = tokio::select! {
        biased;
        _ = interrupt.recv() => Err(CANCELLED),
        _ = terminate.recv() => Err(CANCELLED),
        result = auth.login_manual_with_completion(|authorization, _| {
            terminal.complete(authorization, timeout)
        }) => result,
    };
    // Also covers cancellation while complete() was suspended. Drop is backup,
    // not the success path: a cleanup error must never authorize an exchange.
    terminal.restore()?;
    result?;

    // The credential is durably committed now. Confirmation is bounded, best
    // effort only: a signal, closed tty, or backpressure must not relabel a
    // committed profile as a cancelled/failed login and invite a second login.
    let deadline = Instant::now() + Duration::from_secs(2);
    tokio::select! {
        biased;
        _ = interrupt.recv() => (),
        _ = terminate.recv() => (),
        _ = async {
            terminal.output(super::super::auth::LOGIN_SAVED.as_bytes(), deadline).await?;
            terminal.output(b"\n", deadline).await
        } => (),
    }
    Ok(())
}

struct Terminal {
    file: File,
    saved: libc::termios,
    active: bool,
}
impl Terminal {
    fn open() -> Result<Self> {
        if !io::stdin().is_terminal() {
            return Err("Manual OAuth login requires terminal stdin; pipes are unsupported");
        }
        // A fresh open file description, NOT dup(stdin): flags cannot affect
        // the shell's shared stdin descriptor. /dev/tty is the input authority.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
            .open("/dev/tty")
            .map_err(|_| CLOSED)?;
        foreground(&file)?;
        let mut saved = std::mem::MaybeUninit::uninit();
        // SAFETY: owned live tty fd and writable termios storage.
        if unsafe { libc::tcgetattr(file.as_raw_fd(), saved.as_mut_ptr()) } != 0 {
            return Err(CLOSED);
        }
        Ok(Self {
            file,
            saved: unsafe { saved.assume_init() },
            active: false,
        })
    }
    fn report_error(&mut self, message: &'static str) {
        // The runtime has shut down, so neither Tokio time nor its persistent
        // signal registrations may be used to drive this best-effort report.
        // The fd is our original independent O_NONBLOCK /dev/tty description.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let diagnostic = format!("\nError: {message}\n");
        let mut bytes = diagnostic.as_bytes();
        let Ok(_mask) = TtouMask::block() else {
            return;
        };
        while !bytes.is_empty() {
            let now = std::time::Instant::now();
            if now >= deadline || foreground(&self.file).is_err() {
                return;
            }
            let mut descriptor = libc::pollfd {
                fd: self.file.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            // One absolute budget across partial writes, EAGAIN and EINTR.
            let remaining = deadline.saturating_duration_since(now);
            let wait_ms = remaining.as_millis().clamp(1, 20) as libc::c_int;
            let ready = unsafe { libc::poll(&mut descriptor, 1, wait_ms) };
            if ready < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            if descriptor.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return;
            }
            if std::time::Instant::now() >= deadline {
                return;
            }
            if descriptor.revents & libc::POLLOUT == 0 {
                continue;
            }
            match self.file.write(bytes) {
                Ok(0) => return,
                Ok(n) => bytes = &bytes[n..],
                Err(error) if retryable(&error) => (),
                Err(_) => return,
            }
        }
    }
    fn hide_input(&mut self) -> Result<()> {
        foreground(&self.file)?;
        let mut raw = self.saved;
        raw.c_iflag &= !(libc::IGNBRK
            | libc::BRKINT
            | libc::PARMRK
            | libc::ISTRIP
            | libc::INLCR
            | libc::IGNCR
            | libc::ICRNL
            | libc::IXON
            | libc::IXOFF
            | libc::IXANY
            | libc::INPCK);
        raw.c_lflag &= !(libc::ECHO
            | libc::ECHOE
            | libc::ECHOK
            | libc::ECHONL
            | libc::ECHOCTL
            | libc::ECHOPRT
            | libc::ECHOKE
            | libc::ICANON
            | libc::IEXTEN
            | libc::ISIG);
        raw.c_cflag = (raw.c_cflag & !(libc::CSIZE | libc::PARENB)) | libc::CS8;
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        // In particular preserve c_oflag/OPOST/ONLCR: authorization URL output
        // must remain readable rather than stair-stepping across the terminal.
        let _mask = TtouMask::block()?;
        self.active = true;
        if unsafe { libc::tcsetattr(self.file.as_raw_fd(), libc::TCSANOW, &raw) } != 0 {
            return Err("Manual OAuth terminal setup failed");
        }
        Ok(())
    }
    fn restore(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        let _mask = TtouMask::block().map_err(|_| RESTORE_FAILED)?;
        // Discard queued input BEFORE restoring echo. Never drain a producer;
        // this cannot promise disposal of future bytes arriving after exit.
        let flushed = unsafe { libc::tcflush(self.file.as_raw_fd(), libc::TCIFLUSH) } == 0;
        // TCSADRAIN/TCSAFLUSH can wait indefinitely for output: use TCSANOW.
        let restored =
            unsafe { libc::tcsetattr(self.file.as_raw_fd(), libc::TCSANOW, &self.saved) } == 0;
        if restored {
            self.active = false;
        }
        if flushed && restored {
            Ok(())
        } else {
            Err(RESTORE_FAILED)
        }
    }
    async fn complete(&mut self, authorization: url::Url, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        let result = async {
            self.hide_input()?;
            self.output(NOTICE, deadline).await?;
            self.output(authorization.as_str().as_bytes(), deadline)
                .await?;
            self.output(b"\n", deadline).await?;
            self.output(PROMPT, deadline).await?;
            self.input(deadline).await
        }
        .await;
        self.restore()?;
        result
    }
    async fn output(&mut self, mut bytes: &[u8], deadline: Instant) -> Result<()> {
        while !bytes.is_empty() {
            self.ready(libc::POLLOUT, deadline).await?;
            match self.file.write(bytes) {
                Ok(0) => return Err(CLOSED),
                Ok(n) => bytes = &bytes[n..],
                Err(error) if retryable(&error) => (),
                Err(_) => return Err(CLOSED),
            }
        }
        Ok(())
    }
    async fn input(&mut self, deadline: Instant) -> Result<String> {
        let mut line = Line::new();
        let mut buffer = [0u8; 256];
        loop {
            self.ready(libc::POLLIN, deadline).await?;
            match self.file.read(&mut buffer) {
                Ok(0) => return Err(CLOSED),
                Ok(n) => {
                    for byte in &buffer[..n] {
                        if Instant::now() >= deadline {
                            return Err(TIMED_OUT);
                        }
                        match line.feed(*byte)? {
                            Input::Pending => (),
                            Input::Empty => {
                                self.output(b"\nEmpty input; paste the displayed code: ", deadline)
                                    .await?
                            }
                            Input::Complete(code) => return Ok(code),
                        }
                    }
                }
                Err(error) if retryable(&error) => (),
                Err(_) => return Err(CLOSED),
            }
        }
    }
    async fn ready(&self, events: libc::c_short, deadline: Instant) -> Result<()> {
        loop {
            // A suspension on EVERY iteration gives signal delivery priority
            // even under a continuously readable/writable tty or EINTR storm.
            tokio::time::sleep_until((Instant::now() + Duration::from_millis(2)).min(deadline))
                .await;
            if Instant::now() >= deadline {
                return Err(TIMED_OUT);
            }
            foreground(&self.file)?;
            let mut descriptor = libc::pollfd {
                fd: self.file.as_raw_fd(),
                events,
                revents: 0,
            };
            // SAFETY: one live descriptor; timeout zero never blocks a runtime thread.
            let polled = unsafe { libc::poll(&mut descriptor, 1, 0) };
            if polled < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(CLOSED);
            }
            if descriptor.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err(CLOSED);
            }
            if descriptor.revents & events != 0 {
                return Ok(());
            }
        }
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
fn foreground(file: &File) -> Result<()> {
    // SAFETY: live owned descriptor; neither call mutates process/terminal state.
    let group = unsafe { libc::tcgetpgrp(file.as_raw_fd()) };
    if group < 0 || group != unsafe { libc::getpgrp() } {
        Err("Manual OAuth login requires a foreground controlling terminal")
    } else {
        Ok(())
    }
}
fn retryable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

// The foreground group can change after validation. Block SIGTTOU only on the
// current thread around synchronous termios/flush calls, restoring its exact
// prior mask before any await. Cleanup must not stop the process in the kernel.
struct TtouMask(libc::sigset_t);
impl TtouMask {
    fn block() -> Result<Self> {
        let mut set = std::mem::MaybeUninit::uninit();
        let mut old = std::mem::MaybeUninit::uninit();
        unsafe {
            if libc::sigemptyset(set.as_mut_ptr()) != 0 {
                return Err(RESTORE_FAILED);
            }
            let mut set = set.assume_init();
            if libc::sigaddset(&mut set, libc::SIGTTOU) != 0
                || libc::pthread_sigmask(libc::SIG_BLOCK, &set, old.as_mut_ptr()) != 0
            {
                return Err(RESTORE_FAILED);
            }
            Ok(Self(old.assume_init()))
        }
    }
}
impl Drop for TtouMask {
    fn drop(&mut self) {
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut());
        }
    }
}

enum Input {
    Pending,
    Empty,
    Complete(String),
}
struct Line {
    bytes: [u8; INPUT_LIMIT],
    len: usize,
    escape: usize,
    paste: bool,
    empty: u8,
}
impl Line {
    fn new() -> Self {
        Self {
            bytes: [0; INPUT_LIMIT],
            len: 0,
            escape: 0,
            paste: false,
            empty: 0,
        }
    }
    fn feed(&mut self, byte: u8) -> Result<Input> {
        if matches!(byte, 3 | 4 | 26 | 28) {
            return Err(CANCELLED);
        }
        if self.escape != 0 {
            let expected = if self.paste {
                b"\x1b[201~"
            } else {
                b"\x1b[200~"
            };
            if byte != expected[self.escape] {
                return Err(INVALID);
            }
            self.escape += 1;
            if self.escape == expected.len() {
                self.escape = 0;
                self.paste = !self.paste;
            }
            return Ok(Input::Pending);
        }
        match byte {
            0x1b => self.escape = 1,
            b'\r' | b'\n' if !self.paste => {
                if self.len != 0 {
                    // Only printable ASCII entered the fixed buffer.
                    return Ok(Input::Complete(
                        String::from_utf8(self.bytes[..self.len].to_vec()).map_err(|_| INVALID)?,
                    ));
                }
                self.empty += 1;
                if self.empty > 3 {
                    return Err("Manual OAuth empty input limit exceeded");
                }
                return Ok(Input::Empty);
            }
            8 | 127 => self.len = self.len.saturating_sub(1),
            0x21..=0x7e if self.len < INPUT_LIMIT => {
                self.bytes[self.len] = byte;
                self.len += 1;
            }
            _ => return Err(INVALID),
        }
        Ok(Input::Pending)
    }
}

#[cfg(test)]
#[path = "manual_terminal_tests.rs"]
mod tests;
