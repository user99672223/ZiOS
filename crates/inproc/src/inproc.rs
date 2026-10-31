//! In-process replacement for child processes (CLAUDE.md section 6).
//!
//! iOS forbids spawning processes, so every program Zed would launch runs as a
//! thread inside the app. The spawn API and program registry arrive in phase 7.
