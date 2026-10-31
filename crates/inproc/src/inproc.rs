//! In-process replacement for child processes (CLAUDE.md section 6).
//!
//! iOS forbids starting processes, so every program Zed would launch runs as a
//! thread inside the app. A program is a function registered under a name
//! (`rust-analyzer`, `git`, `ls`, ...). It gets private stdin, stdout and
//! stderr (real pipe file descriptors, so C tools can use them too), its own
//! arguments, environment and working directory, and a cancellation flag.
//! Panics become exit status 101 instead of taking down the editor.
//!
//! The crate builds and is tested on macOS and Linux as well as iOS.

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fs::File,
    future::Future,
    io,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    pin::Pin,
    process::ExitStatus,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    task::{Context, Poll, Waker},
};

use parking_lot::{Mutex, RwLock};

/// Exit status for a program that panicked, as Rust uses for test failures.
pub const PANIC_EXIT_CODE: i32 = 101;
/// Exit status for a program stopped by `kill`, matching a shell's 128 + SIGKILL.
pub const KILLED_EXIT_CODE: i32 = 137;
/// Large enough for language servers' deep recursion; threads default to 512 KiB
/// on iOS.
const PROGRAM_STACK_SIZE: usize = 64 * 1024 * 1024;

/// What a running program receives.
pub struct ProgramContext {
    pub program: String,
    pub args: Vec<OsString>,
    pub env: HashMap<OsString, OsString>,
    pub cwd: PathBuf,
    pub stdin: File,
    pub stdout: File,
    pub stderr: File,
    cancelled: Arc<AtomicBool>,
}

impl ProgramContext {
    /// Set when the parent called `kill`. Threads cannot be killed, so long
    /// running programs must check this and return.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn cancellation(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }
}

pub type Program = Arc<dyn Fn(ProgramContext) -> i32 + Send + Sync>;

fn registry() -> &'static RwLock<HashMap<String, Program>> {
    static REGISTRY: OnceLock<RwLock<HashMap<String, Program>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// Makes `name` runnable. Later registrations replace earlier ones.
pub fn register(name: &str, program: impl Fn(ProgramContext) -> i32 + Send + Sync + 'static) {
    registry().write().insert(name.to_string(), Arc::new(program));
}

pub fn unregister(name: &str) {
    registry().write().remove(name);
}

/// Registered program names, sorted.
pub fn registered() -> Vec<String> {
    let mut names: Vec<String> = registry().read().keys().cloned().collect();
    names.sort();
    names
}

/// The registry key for a program path: `/usr/bin/git` and `git` both run
/// `git`, because callers often resolve absolute paths that do not exist here.
pub fn program_key(program: &OsStr) -> String {
    Path::new(program)
        .file_name()
        .unwrap_or(program)
        .to_string_lossy()
        .into_owned()
}

pub fn is_registered(program: &OsStr) -> bool {
    registry().read().contains_key(&program_key(program))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stdio {
    #[default]
    Piped,
    Inherit,
    Null,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub program: OsString,
    pub args: Vec<OsString>,
    /// The complete environment of the program.
    pub env: HashMap<OsString, OsString>,
    pub cwd: PathBuf,
    pub stdin: Stdio,
    pub stdout: Stdio,
    pub stderr: Stdio,
}

impl Request {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            env: std::env::vars_os().collect(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            stdin: Stdio::Piped,
            stdout: Stdio::Piped,
            stderr: Stdio::Piped,
        }
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        self
    }
}

#[derive(Default)]
struct ExitState {
    status: Option<i32>,
    wakers: Vec<Waker>,
}

/// A running in-process program, the counterpart of a child process.
pub struct Child {
    id: u32,
    program: String,
    exit: Arc<Mutex<ExitState>>,
    cancelled: Arc<AtomicBool>,
    pub stdin: Option<File>,
    pub stdout: Option<File>,
    pub stderr: Option<File>,
}

impl std::fmt::Debug for Child {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Child")
            .field("id", &self.id)
            .field("program", &self.program)
            .finish_non_exhaustive()
    }
}

fn exit_status(code: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt as _;
    // Wait-status encoding: the exit code lives in the second byte.
    ExitStatus::from_raw((code & 0xff) << 8)
}

impl Child {
    /// A pseudo process id, unique within this app run.
    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    /// Asks the program to stop. Its pipes stay open until it returns.
    pub fn kill(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn try_status(&self) -> Option<ExitStatus> {
        self.exit.lock().status.map(exit_status)
    }

    /// Resolves when the program returns.
    pub fn status(&self) -> StatusFuture {
        StatusFuture {
            exit: self.exit.clone(),
        }
    }

    /// Blocks until the program returns.
    pub fn wait(&self) -> ExitStatus {
        futures::executor::block_on(self.status())
    }
}

pub struct StatusFuture {
    exit: Arc<Mutex<ExitState>>,
}

impl Future for StatusFuture {
    type Output = ExitStatus;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<ExitStatus> {
        let mut exit = self.exit.lock();
        match exit.status {
            Some(code) => Poll::Ready(exit_status(code)),
            None => {
                exit.wakers.push(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

fn pipe() -> io::Result<(File, File)> {
    let (reader, writer) = io::pipe()?;
    Ok((File::from(std::os::fd::OwnedFd::from(reader)), File::from(std::os::fd::OwnedFd::from(writer))))
}

fn null(write: bool) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(!write)
        .write(write)
        .open("/dev/null")
}

/// Duplicates the app's own descriptor, never replacing fds 0/1/2.
fn inherit(fd: i32) -> io::Result<File> {
    use std::os::fd::{AsFd as _, BorrowedFd};
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    Ok(File::from(borrowed.as_fd().try_clone_to_owned()?))
}

/// The parent's end (if piped) and the program's end of one stream.
fn stream(mode: Stdio, fd: i32, parent_reads: bool) -> io::Result<(Option<File>, File)> {
    match mode {
        Stdio::Piped => {
            let (reader, writer) = pipe()?;
            Ok(if parent_reads {
                (Some(reader), writer)
            } else {
                (Some(writer), reader)
            })
        }
        Stdio::Null => Ok((None, null(parent_reads)?)),
        Stdio::Inherit => Ok((None, inherit(fd)?)),
    }
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1_000_000);

/// Starts a registered program on its own thread.
pub fn spawn(request: Request) -> io::Result<Child> {
    let key = program_key(&request.program);
    let registered = registry().read().get(&key).cloned();
    #[cfg(target_os = "ios")]
    let registered = registered.or_else(|| ios_system_program(&key));
    let Some(program) = registered else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{} is not available: iOS cannot start programs, and it is not one of the built-in tools",
                request.program.to_string_lossy()
            ),
        ));
    };

    let (stdin_parent, stdin_child) = stream(request.stdin, 0, false)?;
    let (stdout_parent, stdout_child) = stream(request.stdout, 1, true)?;
    let (stderr_parent, stderr_child) = stream(request.stderr, 2, true)?;
    let exit = Arc::new(Mutex::new(ExitState::default()));
    let cancelled = Arc::new(AtomicBool::new(false));
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);

    let context = ProgramContext {
        program: key.clone(),
        args: request.args,
        env: request.env,
        cwd: request.cwd,
        stdin: stdin_child,
        stdout: stdout_child,
        stderr: stderr_child,
        cancelled: cancelled.clone(),
    };
    let thread_exit = exit.clone();
    let thread_cancelled = cancelled.clone();
    let name = key.clone();
    std::thread::Builder::new()
        .name(format!("inproc:{name}"))
        .stack_size(PROGRAM_STACK_SIZE)
        .spawn(move || {
            log::debug!("inproc: {name} (#{id}) started");
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| program(context)));
            let code = match result {
                Ok(code) if thread_cancelled.load(Ordering::SeqCst) && code == 0 => KILLED_EXIT_CODE,
                Ok(code) => code,
                Err(panic) => {
                    let message = panic
                        .downcast_ref::<&str>()
                        .map(|message| message.to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_string());
                    log::error!("inproc: {name} (#{id}) panicked: {message}");
                    PANIC_EXIT_CODE
                }
            };
            log::debug!("inproc: {name} (#{id}) exited with {code}");
            // The program's pipe ends were dropped with its context, so
            // readers see end-of-file before the exit status is published.
            let wakers = {
                let mut exit = thread_exit.lock();
                exit.status = Some(code);
                std::mem::take(&mut exit.wakers)
            };
            for waker in wakers {
                waker.wake();
            }
        })?;

    Ok(Child {
        id,
        program: key,
        exit,
        cancelled,
        stdin: stdin_parent,
        stdout: stdout_parent,
        stderr: stderr_parent,
    })
}

/// Commands that ios_system provides (`ls`, `grep`, `curl`, `tar`, ...) run
/// through it when no Rust program is registered under the name.
#[cfg(target_os = "ios")]
fn ios_system_program(key: &str) -> Option<Program> {
    if !ios_system_sys::is_executable(key) {
        return None;
    }
    Some(Arc::new(run_with_ios_system))
}

#[cfg(target_os = "ios")]
fn run_with_ios_system(context: ProgramContext) -> i32 {
    use std::os::fd::AsRawFd as _;

    static NEXT_SESSION: AtomicU32 = AtomicU32::new(1);
    // ios_system keeps per-session state keyed by an opaque pointer value.
    let session =
        ios_system_sys::Session(0x2000_0000 + NEXT_SESSION.fetch_add(1, Ordering::SeqCst) as usize);

    let open = |file: &File, mode: &std::ffi::CStr| unsafe {
        let fd = libc::dup(file.as_raw_fd());
        if fd < 0 {
            return std::ptr::null_mut();
        }
        libc::fdopen(fd, mode.as_ptr())
    };
    let stdin = open(&context.stdin, c"r");
    let stdout = open(&context.stdout, c"w");
    let stderr = open(&context.stderr, c"w");
    if stdin.is_null() || stdout.is_null() || stderr.is_null() {
        return 126;
    }

    session.activate();
    for (name, value) in &context.env {
        if let (Some(name), Some(value)) = (name.to_str(), value.to_str()) {
            ios_system_sys::set_env(name, value);
        }
    }
    unsafe { ios_system_sys::set_streams(stdin, stdout, stderr) };
    let cwd = context.cwd.to_string_lossy();
    ios_system_sys::system(&format!("cd {}", ios_system_sys::quote(&cwd)));
    let mut command_line = ios_system_sys::quote(&context.program);
    for arg in &context.args {
        command_line.push(' ');
        command_line.push_str(&ios_system_sys::quote(&arg.to_string_lossy()));
    }
    let code = ios_system_sys::system(&command_line);
    unsafe {
        libc::fflush(stdout);
        libc::fflush(stderr);
        libc::fclose(stdin);
        libc::fclose(stdout);
        libc::fclose(stderr);
    }
    session.close();
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    fn register_test_programs() {
        register("test-echo", |mut context| {
            let line = context
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ");
            writeln!(context.stdout, "{line}").map_or(1, |()| 0)
        });
        register("test-cat", |mut context| {
            let mut input = Vec::new();
            if context.stdin.read_to_end(&mut input).is_err() {
                return 1;
            }
            context.stdout.write_all(&input).map_or(1, |()| 0)
        });
        register("test-env", |mut context| {
            let value = context
                .env
                .get(OsStr::new("INPROC_TEST"))
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_default();
            let cwd = context.cwd.display().to_string();
            write!(context.stdout, "{value} {cwd}").map_or(1, |()| 0)
        });
        register("test-panic", |_| panic!("boom"));
        register("test-wait-for-kill", |context| {
            while !context.is_cancelled() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            0
        });
        register("test-exit-code", |mut context| {
            writeln!(context.stderr, "failing").ok();
            3
        });
    }

    fn read_all(file: &mut Option<File>) -> String {
        let mut output = String::new();
        file.take()
            .expect("piped")
            .read_to_string(&mut output)
            .unwrap();
        output
    }

    #[test]
    fn runs_a_program_with_arguments() {
        register_test_programs();
        let mut child = spawn(Request::new("test-echo").args(["hello", "world"])).unwrap();
        assert_eq!(read_all(&mut child.stdout), "hello world\n");
        assert_eq!(child.wait().code(), Some(0));
    }

    #[test]
    fn absolute_paths_resolve_by_file_name() {
        register_test_programs();
        let child = spawn(Request::new("/usr/bin/test-echo").args(["x"])).unwrap();
        assert_eq!(child.program(), "test-echo");
        assert!(child.wait().success());
    }

    #[test]
    fn stdin_reaches_the_program() {
        register_test_programs();
        let mut child = spawn(Request::new("test-cat")).unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"piped input").unwrap();
        drop(stdin);
        assert_eq!(read_all(&mut child.stdout), "piped input");
        assert!(child.wait().success());
    }

    #[test]
    fn environment_and_directory_are_private() {
        register_test_programs();
        let mut request = Request::new("test-env");
        request.env.insert("INPROC_TEST".into(), "private".into());
        request.cwd = PathBuf::from("/tmp");
        let mut child = spawn(request).unwrap();
        assert_eq!(read_all(&mut child.stdout), "private /tmp");
        // The app's own environment is untouched.
        assert!(std::env::var_os("INPROC_TEST").is_none());
        child.wait();
    }

    #[test]
    fn a_panic_becomes_an_exit_status() {
        register_test_programs();
        let child = spawn(Request::new("test-panic")).unwrap();
        assert_eq!(child.wait().code(), Some(PANIC_EXIT_CODE));
    }

    #[test]
    fn kill_is_cooperative() {
        register_test_programs();
        let child = spawn(Request::new("test-wait-for-kill")).unwrap();
        assert!(child.try_status().is_none());
        child.kill();
        assert_eq!(child.wait().code(), Some(KILLED_EXIT_CODE));
    }

    #[test]
    fn exit_codes_and_stderr() {
        register_test_programs();
        let mut child = spawn(Request::new("test-exit-code")).unwrap();
        assert_eq!(read_all(&mut child.stderr), "failing\n");
        assert_eq!(child.wait().code(), Some(3));
    }

    #[test]
    fn unknown_programs_are_not_found() {
        let error = spawn(Request::new("definitely-not-registered")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn status_future_resolves_asynchronously() {
        register_test_programs();
        let child = spawn(Request::new("test-echo").args(["async"])).unwrap();
        let status = futures::executor::block_on(child.status());
        assert!(status.success());
    }
}
