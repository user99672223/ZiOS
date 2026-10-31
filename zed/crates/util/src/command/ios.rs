//! iOS backend for [`Command`]: iOS cannot start processes, so programs run
//! in-process through the `inproc` registry (CLAUDE.md section 6). The builder
//! API matches the macOS backend so call sites need no iOS branches; spawning an
//! unregistered program fails with `NotFound` instead of starting a process.

use smol::Async;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stdio {
    /// A new pipe should be arranged to connect the parent and child processes.
    #[default]
    Piped,
    /// The child inherits from the corresponding parent descriptor.
    Inherit,
    /// This stream will be ignored (redirected to `/dev/null`).
    Null,
}

impl Stdio {
    pub fn piped() -> Self {
        Self::Piped
    }

    pub fn inherit() -> Self {
        Self::Inherit
    }

    pub fn null() -> Self {
        Self::Null
    }
}

impl From<Stdio> for inproc::Stdio {
    fn from(stdio: Stdio) -> Self {
        match stdio {
            Stdio::Piped => inproc::Stdio::Piped,
            Stdio::Inherit => inproc::Stdio::Inherit,
            Stdio::Null => inproc::Stdio::Null,
        }
    }
}

#[derive(Debug)]
pub struct Command {
    program: OsString,
    args: Vec<OsString>,
    envs: BTreeMap<OsString, Option<OsString>>,
    env_clear: bool,
    current_dir: Option<PathBuf>,
    stdin_cfg: Option<Stdio>,
    stdout_cfg: Option<Stdio>,
    stderr_cfg: Option<Stdio>,
    kill_on_drop: bool,
}

impl Command {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            envs: BTreeMap::new(),
            env_clear: false,
            current_dir: None,
            stdin_cfg: None,
            stdout_cfg: None,
            stderr_cfg: None,
            kill_on_drop: false,
        }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    pub fn get_args(&self) -> impl Iterator<Item = &OsStr> {
        self.args.iter().map(|s| s.as_os_str())
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> &mut Self {
        self.envs
            .insert(key.as_ref().to_owned(), Some(val.as_ref().to_owned()));
        self
    }

    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        for (key, val) in vars {
            self.envs
                .insert(key.as_ref().to_owned(), Some(val.as_ref().to_owned()));
        }
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        let key = key.as_ref().to_owned();
        if self.env_clear {
            self.envs.remove(&key);
        } else {
            self.envs.insert(key, None);
        }
        self
    }

    pub fn env_clear(&mut self) -> &mut Self {
        self.env_clear = true;
        self.envs.clear();
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.current_dir = Some(dir.as_ref().to_owned());
        self
    }

    pub fn stdin(&mut self, cfg: Stdio) -> &mut Self {
        self.stdin_cfg = Some(cfg);
        self
    }

    pub fn stdout(&mut self, cfg: Stdio) -> &mut Self {
        self.stdout_cfg = Some(cfg);
        self
    }

    pub fn stderr(&mut self, cfg: Stdio) -> &mut Self {
        self.stderr_cfg = Some(cfg);
        self
    }

    pub fn kill_on_drop(&mut self, kill_on_drop: bool) -> &mut Self {
        self.kill_on_drop = kill_on_drop;
        self
    }

    pub fn spawn(&mut self) -> io::Result<Child> {
        let mut env: std::collections::HashMap<OsString, OsString> = if self.env_clear {
            Default::default()
        } else {
            std::env::vars_os().collect()
        };
        for (key, value) in &self.envs {
            match value {
                Some(value) => {
                    env.insert(key.clone(), value.clone());
                }
                None => {
                    env.remove(key);
                }
            }
        }
        let cwd = match &self.current_dir {
            Some(directory) => directory.clone(),
            None => std::env::current_dir()?,
        };
        let child = inproc::spawn(inproc::Request {
            program: self.program.clone(),
            args: self.args.clone(),
            env,
            cwd,
            stdin: self.stdin_cfg.unwrap_or_default().into(),
            stdout: self.stdout_cfg.unwrap_or_default().into(),
            stderr: self.stderr_cfg.unwrap_or_default().into(),
        })?;
        Child::new(child, self.kill_on_drop)
    }

    pub async fn output(&mut self) -> io::Result<Output> {
        self.stdin_cfg.get_or_insert(Stdio::null());
        self.stdout_cfg.get_or_insert(Stdio::piped());
        self.stderr_cfg.get_or_insert(Stdio::piped());

        let child = self.spawn()?;
        child.output().await
    }

    pub async fn status(&mut self) -> io::Result<ExitStatus> {
        let mut child = self.spawn()?;
        child.status().await
    }

    pub fn get_program(&self) -> &OsStr {
        self.program.as_os_str()
    }
}

#[derive(Debug)]
pub struct Child {
    inner: inproc::Child,
    kill_on_drop: bool,
    pub stdin: Option<Async<std::fs::File>>,
    pub stdout: Option<Async<std::fs::File>>,
    pub stderr: Option<Async<std::fs::File>>,
}

impl Child {
    fn new(mut inner: inproc::Child, kill_on_drop: bool) -> io::Result<Self> {
        let stdin = inner.stdin.take().map(Async::new).transpose()?;
        let stdout = inner.stdout.take().map(Async::new).transpose()?;
        let stderr = inner.stderr.take().map(Async::new).transpose()?;
        Ok(Self {
            inner,
            kill_on_drop,
            stdin,
            stdout,
            stderr,
        })
    }

    pub fn id(&self) -> u32 {
        self.inner.id()
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.inner.kill();
        Ok(())
    }

    pub fn try_status(&mut self) -> io::Result<Option<ExitStatus>> {
        Ok(self.inner.try_status())
    }

    pub fn status(
        &mut self,
    ) -> impl std::future::Future<Output = io::Result<ExitStatus>> + Send + 'static {
        self.stdin.take();
        let status = self.inner.status();
        async move { Ok(status.await) }
    }

    pub async fn output(mut self) -> io::Result<Output> {
        use futures_lite::AsyncReadExt;

        let status = self.status();

        let stdout = self.stdout.take();
        let stdout_future = async move {
            let mut data = Vec::new();
            if let Some(mut stdout) = stdout {
                stdout.read_to_end(&mut data).await?;
            }
            io::Result::Ok(data)
        };

        let stderr = self.stderr.take();
        let stderr_future = async move {
            let mut data = Vec::new();
            if let Some(mut stderr) = stderr {
                stderr.read_to_end(&mut data).await?;
            }
            io::Result::Ok(data)
        };

        let (stdout_data, stderr_data) =
            futures_lite::future::try_zip(stdout_future, stderr_future).await?;
        let status = status.await?;

        Ok(Output {
            status,
            stdout: stdout_data,
            stderr: stderr_data,
        })
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if self.kill_on_drop {
            self.inner.kill();
        }
    }
}
