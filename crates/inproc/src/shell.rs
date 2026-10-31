//! An interactive shell for the terminal, without a real PTY (CLAUDE.md §7).
//!
//! iOS apps cannot start a shell process, and a PTY's line discipline lives in
//! the kernel's TTY layer, which pipes do not have. This module provides both
//! halves in-process, the way Blink Shell does on top of ios_system:
//!
//! - a line discipline (cooked mode): echo, backspace, Ctrl-C, Ctrl-D, Ctrl-U,
//!   history on the arrow keys, and `\n` → `\r\n` on output;
//! - a shell loop that prints a prompt with the current folder name, handles
//!   `cd`, `pwd`, `exit`, and runs everything else through an [`Executor`]
//!   (ios_system on iOS, registered in-process programs in tests).
//!
//! The terminal sees two pipe ends: it writes keystrokes to `input` and reads
//! screen output from `output`, exactly like the master side of a PTY.

use std::{
    collections::VecDeque,
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
};

use parking_lot::Mutex;

/// Runs one command line. Implementations must return when the command ends,
/// and stop it when [`Executor::interrupt`] is called.
pub trait Executor: Send + Sync + 'static {
    fn run(
        &self,
        command_line: &str,
        cwd: &Path,
        stdin: File,
        stdout: File,
        stderr: File,
    ) -> i32;

    /// Ctrl-C while a command runs.
    fn interrupt(&self);

    fn resize(&self, _columns: u16, _rows: u16) {}
}

pub struct ShellConfig {
    pub cwd: PathBuf,
    /// Shown as `~` in the prompt.
    pub home: PathBuf,
    pub columns: u16,
    pub rows: u16,
}

/// The terminal's side of a running shell.
pub struct ShellHandle {
    /// Keystrokes from the terminal go here.
    pub input: File,
    /// Screen output for the terminal comes from here.
    pub output: File,
    /// Receives the exit status once the shell ends.
    pub exited: Receiver<i32>,
    executor: Arc<dyn Executor>,
    size: Arc<Mutex<(u16, u16)>>,
}

impl ShellHandle {
    pub fn resize(&self, columns: u16, rows: u16) {
        *self.size.lock() = (columns, rows);
        self.executor.resize(columns, rows);
    }
}

fn pipe() -> io::Result<(File, File)> {
    let (reader, writer) = io::pipe()?;
    Ok((
        File::from(std::os::fd::OwnedFd::from(reader)),
        File::from(std::os::fd::OwnedFd::from(writer)),
    ))
}

/// What the line discipline hands to the shell loop.
enum LineEvent {
    Line(String),
    Interrupt,
    EndOfFile,
}

#[derive(Default)]
struct Discipline {
    line: String,
    history: Vec<String>,
    history_index: Option<usize>,
    escape: Vec<u8>,
    utf8: Vec<u8>,
}

/// What a keystroke means, decided without I/O so it can be tested.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Echo(String),
    Submit(String),
    Interrupt,
    EndOfFile,
    Nothing,
}

const ERASE_CHARACTER: &str = "\x08 \x08";

impl Discipline {
    fn erase_line(&self) -> String {
        ERASE_CHARACTER.repeat(self.line.chars().count())
    }

    fn feed(&mut self, byte: u8) -> Action {
        if !self.escape.is_empty() || byte == 0x1b {
            self.escape.push(byte);
            return self.feed_escape();
        }
        match byte {
            b'\r' | b'\n' => {
                let line = std::mem::take(&mut self.line);
                self.history_index = None;
                if !line.trim().is_empty() && self.history.last() != Some(&line) {
                    self.history.push(line.clone());
                }
                Action::Submit(line)
            }
            0x03 => {
                self.line.clear();
                self.history_index = None;
                Action::Interrupt
            }
            0x04 if self.line.is_empty() => Action::EndOfFile,
            0x04 => Action::Nothing,
            0x7f | 0x08 => {
                if self.line.pop().is_some() {
                    Action::Echo(ERASE_CHARACTER.into())
                } else {
                    Action::Nothing
                }
            }
            0x15 => {
                let erase = self.erase_line();
                self.line.clear();
                Action::Echo(erase)
            }
            byte if byte < 0x20 => Action::Nothing,
            byte => {
                self.utf8.push(byte);
                match std::str::from_utf8(&self.utf8) {
                    Ok(text) => {
                        let text = text.to_string();
                        self.utf8.clear();
                        self.line.push_str(&text);
                        Action::Echo(text)
                    }
                    Err(error) if error.error_len().is_none() => Action::Nothing,
                    Err(_) => {
                        self.utf8.clear();
                        Action::Nothing
                    }
                }
            }
        }
    }

    fn feed_escape(&mut self) -> Action {
        // CSI sequences end with a byte in 0x40..=0x7e after "ESC [".
        let sequence = self.escape.clone();
        if sequence.len() < 2 {
            return Action::Nothing;
        }
        if sequence[1] != b'[' && sequence[1] != b'O' {
            self.escape.clear();
            return Action::Nothing;
        }
        let Some(&last) = sequence.last() else {
            return Action::Nothing;
        };
        if sequence.len() == 2 || !(0x40..=0x7e).contains(&last) {
            return Action::Nothing;
        }
        self.escape.clear();
        match last {
            b'A' => self.recall(-1),
            b'B' => self.recall(1),
            _ => Action::Nothing,
        }
    }

    fn recall(&mut self, direction: isize) -> Action {
        if self.history.is_empty() {
            return Action::Nothing;
        }
        let next = match (self.history_index, direction) {
            (None, -1) => Some(self.history.len() - 1),
            (None, _) => return Action::Nothing,
            (Some(0), -1) => Some(0),
            (Some(index), -1) => Some(index - 1),
            (Some(index), _) if index + 1 < self.history.len() => Some(index + 1),
            (Some(_), _) => None,
        };
        let erase = self.erase_line();
        self.history_index = next;
        self.line = next
            .and_then(|index| self.history.get(index).cloned())
            .unwrap_or_default();
        Action::Echo(format!("{erase}{}", self.line))
    }
}

/// Converts lone `\n` to `\r\n`, as a TTY's ONLCR output flag does.
fn translate_newlines(bytes: &[u8], previous_was_cr: &mut bool) -> Vec<u8> {
    let mut translated = Vec::with_capacity(bytes.len() + bytes.len() / 8);
    for &byte in bytes {
        if byte == b'\n' && !*previous_was_cr {
            translated.push(b'\r');
        }
        translated.push(byte);
        *previous_was_cr = byte == b'\r';
    }
    translated
}

pub fn prompt(cwd: &Path, home: &Path) -> String {
    let name = if cwd == home {
        "~".to_string()
    } else {
        cwd.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| cwd.display().to_string())
    };
    format!("\x1b[1m{name}\x1b[0m $ ")
}

/// Starts the shell on background threads.
pub fn spawn(config: ShellConfig, executor: Arc<dyn Executor>) -> io::Result<ShellHandle> {
    let (input_reader, input_writer) = pipe()?;
    let (output_reader, output_writer) = pipe()?;
    // Everything shown goes through `raw` so newline translation and ordering
    // are the same for echo, prompts, and command output.
    let (raw_reader, raw_writer) = pipe()?;
    let (lines_sender, lines) = mpsc::channel::<LineEvent>();
    let (exit_sender, exited) = mpsc::channel();
    let command_stdin: Arc<Mutex<Option<File>>> = Arc::new(Mutex::new(None));
    let size = Arc::new(Mutex::new((config.columns, config.rows)));
    let echo = Arc::new(Mutex::new(raw_writer.try_clone()?));

    std::thread::Builder::new()
        .name("shell-output".into())
        .spawn(move || copy_translating(raw_reader, output_writer))?;

    std::thread::Builder::new().name("shell-input".into()).spawn({
        let echo = echo.clone();
        let command_stdin = command_stdin.clone();
        let executor = executor.clone();
        move || read_keystrokes(input_reader, echo, command_stdin, lines_sender, executor)
    })?;

    std::thread::Builder::new().name("shell".into()).spawn({
        let executor = executor.clone();
        move || {
            let code = shell_loop(config, raw_writer, lines, command_stdin, executor);
            exit_sender.send(code).ok();
        }
    })?;

    Ok(ShellHandle {
        input: input_writer,
        output: output_reader,
        exited,
        executor,
        size,
    })
}

fn copy_translating(mut raw: File, mut output: File) {
    let mut buffer = [0u8; 8192];
    let mut previous_was_cr = false;
    loop {
        match raw.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let translated = translate_newlines(&buffer[..count], &mut previous_was_cr);
                if output.write_all(&translated).is_err() {
                    break;
                }
            }
        }
    }
}

fn read_keystrokes(
    mut input: File,
    echo: Arc<Mutex<File>>,
    command_stdin: Arc<Mutex<Option<File>>>,
    lines: Sender<LineEvent>,
    executor: Arc<dyn Executor>,
) {
    let mut discipline = Discipline::default();
    let mut buffer = [0u8; 1024];
    let write_echo = |text: &str| {
        let mut echo = echo.lock();
        echo.write_all(text.as_bytes()).ok();
    };
    loop {
        let count = match input.read(&mut buffer) {
            Ok(0) | Err(_) => {
                lines.send(LineEvent::EndOfFile).ok();
                return;
            }
            Ok(count) => count,
        };
        for &byte in &buffer[..count] {
            let running = command_stdin.lock().is_some();
            match discipline.feed(byte) {
                Action::Echo(text) => write_echo(&text),
                Action::Submit(line) => {
                    write_echo("\n");
                    if running {
                        if let Some(stdin) = command_stdin.lock().as_mut() {
                            stdin.write_all(format!("{line}\n").as_bytes()).ok();
                        }
                    } else if lines.send(LineEvent::Line(line)).is_err() {
                        return;
                    }
                }
                Action::Interrupt => {
                    write_echo("^C\n");
                    if running {
                        executor.interrupt();
                    } else if lines.send(LineEvent::Interrupt).is_err() {
                        return;
                    }
                }
                Action::EndOfFile => {
                    if running {
                        // Ctrl-D ends the command's input.
                        command_stdin.lock().take();
                    } else if lines.send(LineEvent::EndOfFile).is_err() {
                        return;
                    }
                }
                Action::Nothing => {}
            }
        }
    }
}

fn shell_loop(
    config: ShellConfig,
    mut screen: File,
    lines: Receiver<LineEvent>,
    command_stdin: Arc<Mutex<Option<File>>>,
    executor: Arc<dyn Executor>,
) -> i32 {
    let mut cwd = config.cwd;
    let home = config.home;
    let mut status = 0;
    let mut pending: VecDeque<String> = VecDeque::new();
    loop {
        if screen.write_all(prompt(&cwd, &home).as_bytes()).is_err() {
            return status;
        }
        let line = match pending.pop_front() {
            Some(line) => line,
            None => match lines.recv() {
                Ok(LineEvent::Line(line)) => line,
                Ok(LineEvent::Interrupt) => continue,
                Ok(LineEvent::EndOfFile) | Err(_) => {
                    screen.write_all(b"exit\n").ok();
                    return status;
                }
            },
        };
        let command = line.trim();
        if command.is_empty() {
            continue;
        }
        let mut words = command.split_whitespace();
        match words.next() {
            Some("exit") => {
                return words.next().and_then(|code| code.parse().ok()).unwrap_or(status);
            }
            Some("cd") => {
                let target = words.next().map_or_else(|| home.clone(), |path| {
                    if path == "~" {
                        home.clone()
                    } else if let Some(rest) = path.strip_prefix("~/") {
                        home.join(rest)
                    } else {
                        cwd.join(path)
                    }
                });
                match target.canonicalize() {
                    Ok(target) if target.is_dir() => {
                        cwd = target;
                        status = 0;
                    }
                    _ => {
                        writeln!(screen, "cd: no such directory: {}", target.display()).ok();
                        status = 1;
                    }
                }
                continue;
            }
            Some("pwd") => {
                writeln!(screen, "{}", cwd.display()).ok();
                status = 0;
                continue;
            }
            _ => {}
        }

        let streams = (|| -> io::Result<(File, File, File, File)> {
            let (stdin_reader, stdin_writer) = pipe()?;
            Ok((stdin_reader, stdin_writer, screen.try_clone()?, screen.try_clone()?))
        })();
        let (stdin_reader, stdin_writer, stdout, stderr) = match streams {
            Ok(streams) => streams,
            Err(error) => {
                writeln!(screen, "shell: {error}").ok();
                continue;
            }
        };
        *command_stdin.lock() = Some(stdin_writer);
        status = executor.run(command, &cwd, stdin_reader, stdout, stderr);
        command_stdin.lock().take();
    }
}

/// Runs command lines as registered in-process programs (no pipes or
/// redirection). Used where ios_system is unavailable, such as tests.
pub struct InprocExecutor {
    running: Mutex<Option<crate::Child>>,
}

impl InprocExecutor {
    pub fn new() -> Self {
        Self {
            running: Mutex::new(None),
        }
    }
}

impl Default for InprocExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl Executor for InprocExecutor {
    fn run(&self, command_line: &str, cwd: &Path, stdin: File, stdout: File, stderr: File) -> i32 {
        let mut words = command_line.split_whitespace();
        let Some(program) = words.next() else {
            return 0;
        };
        let mut request = crate::Request::new(program).args(words);
        request.cwd = cwd.to_path_buf();
        request.stdin = crate::Stdio::Piped;
        request.stdout = crate::Stdio::Piped;
        request.stderr = crate::Stdio::Piped;
        let mut child = match crate::spawn(request) {
            Ok(child) => child,
            Err(error) => {
                let mut stderr = stderr;
                writeln!(stderr, "{program}: {error}").ok();
                return 127;
            }
        };
        let forward = |from: Option<File>, mut to: File| {
            std::thread::spawn(move || {
                if let Some(mut from) = from {
                    io::copy(&mut from, &mut to).ok();
                }
            })
        };
        let mut stdin = stdin;
        let child_stdin = child.stdin.take();
        let stdin_thread = std::thread::spawn(move || {
            if let Some(mut child_stdin) = child_stdin {
                io::copy(&mut stdin, &mut child_stdin).ok();
            }
        });
        let stdout_thread = forward(child.stdout.take(), stdout);
        let stderr_thread = forward(child.stderr.take(), stderr);
        let id = child.id();
        *self.running.lock() = Some(child);
        let status = {
            let running = self.running.lock();
            running.as_ref().map(|child| child.status())
        };
        let code = status
            .map(|status| futures::executor::block_on(status).code().unwrap_or(1))
            .unwrap_or(1);
        log::debug!("shell: #{id} exited with {code}");
        self.running.lock().take();
        stdout_thread.join().ok();
        stderr_thread.join().ok();
        drop(stdin_thread);
        code
    }

    fn interrupt(&self) {
        if let Some(child) = self.running.lock().as_ref() {
            child.kill();
        }
    }
}

/// The terminal's executor on iOS: built-in Rust programs (language tools,
/// `git`, `python3`, ...) run in-process; everything else, including pipelines
/// and redirections, goes to ios_system, as in Blink Shell.
#[cfg(target_os = "ios")]
pub struct IosSystemExecutor {
    session: ios_system_sys::Session,
    programs: InprocExecutor,
}

#[cfg(target_os = "ios")]
impl IosSystemExecutor {
    pub fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
        Self {
            session: ios_system_sys::Session(
                0x1000_0000 + NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
            ),
            programs: InprocExecutor::new(),
        }
    }
}

#[cfg(target_os = "ios")]
impl Executor for IosSystemExecutor {
    fn run(&self, command_line: &str, cwd: &Path, stdin: File, stdout: File, stderr: File) -> i32 {
        use std::os::fd::IntoRawFd as _;

        let simple = !command_line.contains(['|', '>', '<', ';', '&', '$', '`']);
        let first = command_line.split_whitespace().next().unwrap_or_default();
        if simple && crate::registry().read().contains_key(first) {
            return self.programs.run(command_line, cwd, stdin, stdout, stderr);
        }
        let open = |file: File, mode: &std::ffi::CStr| unsafe {
            libc::fdopen(file.into_raw_fd(), mode.as_ptr())
        };
        let (stdin, stdout, stderr) = (open(stdin, c"r"), open(stdout, c"w"), open(stderr, c"w"));
        if stdin.is_null() || stdout.is_null() || stderr.is_null() {
            return 126;
        }
        self.session.activate();
        unsafe { ios_system_sys::set_streams(stdin, stdout, stderr) };
        ios_system_sys::system(&format!(
            "cd {}",
            ios_system_sys::quote(&cwd.to_string_lossy())
        ));
        let code = ios_system_sys::system(command_line);
        unsafe {
            libc::fflush(stdout);
            libc::fflush(stderr);
            libc::fclose(stdin);
            libc::fclose(stdout);
            libc::fclose(stderr);
        }
        code
    }

    fn interrupt(&self) {
        ios_system_sys::kill();
        self.programs.interrupt();
    }

    fn resize(&self, columns: u16, rows: u16) {
        self.session.set_window_size(columns, rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(discipline: &mut Discipline, bytes: &[u8]) -> Vec<Action> {
        bytes.iter().map(|&byte| discipline.feed(byte)).collect()
    }

    #[test]
    fn echoes_and_submits_lines() {
        let mut discipline = Discipline::default();
        let actions = feed_all(&mut discipline, b"ls\r");
        assert_eq!(
            actions,
            vec![
                Action::Echo("l".into()),
                Action::Echo("s".into()),
                Action::Submit("ls".into())
            ]
        );
    }

    #[test]
    fn backspace_and_kill_line() {
        let mut discipline = Discipline::default();
        feed_all(&mut discipline, b"lx");
        assert_eq!(discipline.feed(0x7f), Action::Echo(ERASE_CHARACTER.into()));
        assert_eq!(discipline.feed(b's'), Action::Echo("s".into()));
        assert_eq!(discipline.feed(b'\r'), Action::Submit("ls".into()));
        assert_eq!(discipline.feed(0x7f), Action::Nothing);
        feed_all(&mut discipline, b"abc");
        assert_eq!(discipline.feed(0x15), Action::Echo(ERASE_CHARACTER.repeat(3)));
        assert_eq!(discipline.feed(b'\r'), Action::Submit(String::new()));
    }

    #[test]
    fn control_keys() {
        let mut discipline = Discipline::default();
        feed_all(&mut discipline, b"sleep");
        assert_eq!(discipline.feed(0x03), Action::Interrupt);
        assert_eq!(discipline.feed(0x04), Action::EndOfFile);
        feed_all(&mut discipline, b"x");
        assert_eq!(discipline.feed(0x04), Action::Nothing);
    }

    #[test]
    fn multibyte_characters() {
        let mut discipline = Discipline::default();
        let actions = feed_all(&mut discipline, "é".as_bytes());
        assert_eq!(actions, vec![Action::Nothing, Action::Echo("é".into())]);
        assert_eq!(discipline.feed(0x7f), Action::Echo(ERASE_CHARACTER.into()));
        assert!(discipline.line.is_empty());
    }

    #[test]
    fn history_recall() {
        let mut discipline = Discipline::default();
        feed_all(&mut discipline, b"one\rtwo\r");
        assert_eq!(
            feed_all(&mut discipline, b"\x1b[A").last(),
            Some(&Action::Echo("two".into()))
        );
        assert_eq!(
            feed_all(&mut discipline, b"\x1b[A").last(),
            Some(&Action::Echo(format!("{}one", ERASE_CHARACTER.repeat(3))))
        );
        assert_eq!(
            feed_all(&mut discipline, b"\x1b[B").last(),
            Some(&Action::Echo(format!("{}two", ERASE_CHARACTER.repeat(3))))
        );
        assert_eq!(discipline.feed(b'\r'), Action::Submit("two".into()));
    }

    #[test]
    fn output_newlines_become_crlf() {
        let mut previous = false;
        assert_eq!(translate_newlines(b"a\nb\r\nc\n", &mut previous), b"a\r\nb\r\nc\r\n");
        // A CR at the end of one chunk pairs with an LF at the start of the next.
        let mut previous = false;
        translate_newlines(b"x\r", &mut previous);
        assert_eq!(translate_newlines(b"\ny", &mut previous), b"\ny");
    }

    #[test]
    fn prompt_shows_the_folder_name() {
        let home = Path::new("/var/mobile/Documents");
        assert!(prompt(home, home).contains("~ $"));
        assert!(prompt(&home.join("demo"), home).contains("demo"));
    }

    fn read_until(output: &mut File, needle: &str) -> String {
        let mut collected = String::new();
        let mut buffer = [0u8; 256];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !collected.contains(needle) {
            assert!(std::time::Instant::now() < deadline, "timed out; got {collected:?}");
            let count = output.read(&mut buffer).unwrap();
            assert!(count > 0, "output closed; got {collected:?}");
            collected.push_str(&String::from_utf8_lossy(&buffer[..count]));
        }
        collected
    }

    #[test]
    fn interactive_session() {
        crate::register("shell-test-echo", |mut context| {
            let line = context
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ");
            writeln!(context.stdout, "{line}").map_or(1, |()| 0)
        });
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("project")).unwrap();
        let home = directory.path().canonicalize().unwrap();
        let mut handle = spawn(
            ShellConfig {
                cwd: home.clone(),
                home: home.clone(),
                columns: 80,
                rows: 24,
            },
            Arc::new(InprocExecutor::new()),
        )
        .unwrap();

        read_until(&mut handle.output, "~ $ ");
        handle.input.write_all(b"shell-test-echo hello\r").unwrap();
        let output = read_until(&mut handle.output, "~ $ ");
        assert!(output.contains("hello\r\n"), "{output:?}");

        handle.input.write_all(b"cd project\r").unwrap();
        read_until(&mut handle.output, "project");
        handle.input.write_all(b"pwd\r").unwrap();
        let output = read_until(&mut handle.output, "project\x1b[0m $ ");
        assert!(output.contains(&home.join("project").display().to_string()));

        handle.input.write_all(b"no-such-tool\r").unwrap();
        let output = read_until(&mut handle.output, " $ ");
        assert!(output.contains("no-such-tool"), "{output:?}");

        handle.input.write_all(b"exit 3\r").unwrap();
        assert_eq!(
            handle.exited.recv_timeout(std::time::Duration::from_secs(10)),
            Ok(3)
        );
    }
}
