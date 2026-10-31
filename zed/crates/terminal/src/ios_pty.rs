//! The terminal's PTY on iOS (ZiOS, CLAUDE.md section 7).
//!
//! iOS cannot start a shell process. `inproc::shell` runs the shell on threads
//! behind a pair of pipes and emulates the TTY line discipline; this type gives
//! alacritty's event loop the same interface as its Unix `Pty`.

use std::{
    fs::File,
    io,
    os::fd::AsRawFd as _,
    path::PathBuf,
    sync::{Arc, mpsc::TryRecvError},
};

use alacritty_terminal::{
    event::{OnResize, WindowSize},
    tty::{ChildEvent, EventedPty, EventedReadWrite},
};
use polling::{Event, PollMode, Poller};

// alacritty's Unix event loop dispatches on these keys (`tty/unix.rs`).
const PTY_READ_WRITE_TOKEN: usize = 0;
const PTY_CHILD_EVENT_TOKEN: usize = 1;

pub struct IosPty {
    reader: File,
    writer: File,
    /// Readable when the shell exits, so the event loop wakes up for it.
    exit_signal: File,
    shell: inproc::shell::ShellHandle,
    id: u32,
}

impl IosPty {
    pub fn new(working_directory: Option<PathBuf>, window_size: WindowSize) -> io::Result<Self> {
        let home = std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Documents"))
            .unwrap_or_else(|| PathBuf::from("/"));
        let cwd = working_directory
            .filter(|directory| directory.is_dir())
            .unwrap_or_else(|| home.clone());
        let mut shell = inproc::shell::spawn(
            inproc::shell::ShellConfig {
                cwd,
                home,
                columns: window_size.num_cols,
                rows: window_size.num_lines,
            },
            Arc::new(inproc::shell::IosSystemExecutor::new()),
        )?;
        let reader = shell.output.try_clone()?;
        let writer = shell.input.try_clone()?;
        for file in [&reader, &writer] {
            set_nonblocking(file)?;
        }
        let (exit_signal, exit_writer) = {
            let (reader, writer) = io::pipe()?;
            (
                File::from(std::os::fd::OwnedFd::from(reader)),
                File::from(std::os::fd::OwnedFd::from(writer)),
            )
        };
        set_nonblocking(&exit_signal)?;
        // Forward the shell's exit status into a pollable byte.
        let exited = std::mem::replace(&mut shell.exited, std::sync::mpsc::channel().1);
        let (status_sender, status_receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("shell-exit".into())
            .spawn(move || {
                use std::io::Write as _;
                let status = exited.recv().unwrap_or(0);
                status_sender.send(status).ok();
                let mut exit_writer = exit_writer;
                exit_writer.write_all(&[1]).ok();
            })?;
        shell.exited = status_receiver;
        static NEXT_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
        Ok(Self {
            reader,
            writer,
            exit_signal,
            shell,
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        })
    }

    /// The reader's descriptor; process lookups on it fail, which the terminal
    /// treats as "no foreground process".
    pub fn raw_fd(&self) -> i32 {
        self.reader.as_raw_fd()
    }

    pub fn id(&self) -> u32 {
        self.id
    }
}

fn read_interest(interest: Event) -> Event {
    let mut event = Event::none(PTY_READ_WRITE_TOKEN);
    event.readable = interest.readable;
    event
}

fn write_interest(interest: Event) -> Event {
    let mut event = Event::none(PTY_READ_WRITE_TOKEN);
    event.writable = interest.writable;
    event
}

fn set_nonblocking(file: &File) -> io::Result<()> {
    let fd = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl EventedReadWrite for IosPty {
    type Reader = File;
    type Writer = File;

    unsafe fn register(
        &mut self,
        poll: &Arc<Poller>,
        interest: Event,
        mode: PollMode,
    ) -> io::Result<()> {
        // Reads and writes use separate pipes, unlike a PTY's single fd; both
        // report under alacritty's read/write key.
        unsafe {
            poll.add_with_mode(&self.reader, read_interest(interest), mode)?;
            poll.add_with_mode(&self.writer, write_interest(interest), mode)?;
            poll.add_with_mode(
                &self.exit_signal,
                Event::readable(PTY_CHILD_EVENT_TOKEN),
                PollMode::Level,
            )
        }
    }

    fn reregister(
        &mut self,
        poll: &Arc<Poller>,
        interest: Event,
        mode: PollMode,
    ) -> io::Result<()> {
        poll.modify_with_mode(&self.reader, read_interest(interest), mode)?;
        poll.modify_with_mode(&self.writer, write_interest(interest), mode)?;
        poll.modify_with_mode(
            &self.exit_signal,
            Event::readable(PTY_CHILD_EVENT_TOKEN),
            PollMode::Level,
        )
    }

    fn deregister(&mut self, poll: &Arc<Poller>) -> io::Result<()> {
        poll.delete(&self.reader)?;
        poll.delete(&self.writer)?;
        poll.delete(&self.exit_signal)
    }

    fn reader(&mut self) -> &mut File {
        &mut self.reader
    }

    fn writer(&mut self) -> &mut File {
        &mut self.writer
    }
}

impl EventedPty for IosPty {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        use std::io::Read as _;
        let mut byte = [0u8; 1];
        if self.exit_signal.read(&mut byte).is_err() {
            return None;
        }
        match self.shell.exited.try_recv() {
            Ok(code) => {
                use std::os::unix::process::ExitStatusExt as _;
                Some(ChildEvent::Exited(Some(std::process::ExitStatus::from_raw(
                    (code & 0xff) << 8,
                ))))
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(ChildEvent::Exited(None)),
        }
    }
}

impl OnResize for IosPty {
    fn on_resize(&mut self, window_size: WindowSize) {
        self.shell.resize(window_size.num_cols, window_size.num_lines);
    }
}
