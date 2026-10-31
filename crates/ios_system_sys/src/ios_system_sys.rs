//! Bindings to ios_system (https://github.com/holzschu/ios_system, v3.0.6),
//! which runs Unix commands as threads inside an iOS app.

#![cfg_attr(ios_system_missing, allow(unused_imports, dead_code))]

use std::ffi::{CString, c_char, c_int, c_void};

#[cfg(not(ios_system_missing))]
mod ffi {
    use super::*;
    unsafe extern "C" {
        pub fn ios_system(command: *const c_char) -> c_int;
        pub fn ios_executable(command: *const c_char) -> c_int;
        pub fn ios_kill() -> c_int;
        pub fn ios_switchSession(session: *const c_void);
        pub fn ios_closeSession(session: *const c_void);
        pub fn ios_setStreams(stdin: *mut libc::FILE, stdout: *mut libc::FILE, stderr: *mut libc::FILE);
        pub fn ios_setWindowSize(width: c_int, height: c_int, session: *const c_void);
        pub fn ios_setenv(name: *const c_char, value: *const c_char, overwrite: c_int) -> c_int;
        pub fn initializeEnvironment();
        pub fn replaceCommand(name: *const c_void, function_name: *const c_void, all: bool);
    }
}

/// Whether ios_system was linked into this build.
pub const AVAILABLE: bool = cfg!(not(ios_system_missing));

/// Must be called once, on the main thread, before running commands.
pub fn initialize() {
    #[cfg(not(ios_system_missing))]
    unsafe {
        ffi::initializeEnvironment()
    }
}

/// Whether `command` names an executable ios_system knows.
pub fn is_executable(command: &str) -> bool {
    #[cfg(not(ios_system_missing))]
    {
        let Ok(command) = CString::new(command) else {
            return false;
        };
        unsafe { ffi::ios_executable(command.as_ptr()) != 0 }
    }
    #[cfg(ios_system_missing)]
    {
        let _ = command;
        false
    }
}

/// A session groups the working directory and environment of one terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Session(pub usize);

impl Session {
    fn pointer(self) -> *const c_void {
        self.0 as *const c_void
    }

    /// Makes this session current for the calling thread's commands.
    pub fn activate(self) {
        #[cfg(not(ios_system_missing))]
        unsafe {
            ffi::ios_switchSession(self.pointer())
        }
    }

    pub fn close(self) {
        #[cfg(not(ios_system_missing))]
        unsafe {
            ffi::ios_closeSession(self.pointer())
        }
    }

    pub fn set_window_size(self, columns: u16, rows: u16) {
        #[cfg(not(ios_system_missing))]
        unsafe {
            ffi::ios_setWindowSize(c_int::from(columns), c_int::from(rows), self.pointer())
        }
        #[cfg(ios_system_missing)]
        let _ = (columns, rows);
    }
}

/// Streams for commands started from the calling thread. The files are owned
/// by the caller and must stay open while commands run.
///
/// # Safety
/// The pointers must be valid `FILE*`s for as long as commands use them.
pub unsafe fn set_streams(stdin: *mut libc::FILE, stdout: *mut libc::FILE, stderr: *mut libc::FILE) {
    #[cfg(not(ios_system_missing))]
    unsafe {
        ffi::ios_setStreams(stdin, stdout, stderr)
    }
    #[cfg(ios_system_missing)]
    let _ = (stdin, stdout, stderr);
}

/// Runs a command line (with pipes and redirections) and returns its status.
pub fn system(command: &str) -> i32 {
    #[cfg(not(ios_system_missing))]
    {
        let Ok(command) = CString::new(command) else {
            return 127;
        };
        unsafe { ffi::ios_system(command.as_ptr()) }
    }
    #[cfg(ios_system_missing)]
    {
        let _ = command;
        127
    }
}

/// Interrupts the command running in the current session (Ctrl-C).
pub fn kill() {
    #[cfg(not(ios_system_missing))]
    unsafe {
        ffi::ios_kill();
    }
}

pub fn set_env(name: &str, value: &str) {
    #[cfg(not(ios_system_missing))]
    if let (Ok(name), Ok(value)) = (CString::new(name), CString::new(value)) {
        unsafe {
            ffi::ios_setenv(name.as_ptr(), value.as_ptr(), 1);
        }
    }
    #[cfg(ios_system_missing)]
    let _ = (name, value);
}

/// Quotes one argument for ios_system's command-line parser.
pub fn quote(argument: &str) -> String {
    if !argument.is_empty()
        && argument
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_./=:,+@%".contains(character))
    {
        return argument.to_string();
    }
    format!("'{}'", argument.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote("ls"), "ls");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote(""), "''");
    }
}
