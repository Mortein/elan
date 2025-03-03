use regex::Regex;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::process::{self, Command, Stdio};
use std::time::Instant;
use tempfile::tempfile;

use elan_utils;
use errors::*;
use notifications::*;
use telemetry::{Telemetry, TelemetryEvent};
use Cfg;

pub fn run_command_for_dir<S: AsRef<OsStr>>(
    cmd: Command,
    arg0: &str,
    args: &[S],
    cfg: &Cfg,
) -> Result<()> {
    if (arg0 == "lean" || arg0 == "lean.exe") && cfg.telemetry_enabled()? {
        return telemetry_lean(cmd, arg0, args, cfg);
    }

    exec_command_for_dir_without_telemetry(cmd, arg0, args)
}

fn telemetry_lean<S: AsRef<OsStr>>(
    mut cmd: Command,
    arg0: &str,
    args: &[S],
    cfg: &Cfg,
) -> Result<()> {
    #[cfg(unix)]
    fn file_as_stdio(file: &File) -> Stdio {
        use std::os::unix::io::{AsRawFd, FromRawFd};
        unsafe { Stdio::from_raw_fd(file.as_raw_fd()) }
    }

    #[cfg(windows)]
    fn file_as_stdio(file: &File) -> Stdio {
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        unsafe { Stdio::from_raw_handle(file.as_raw_handle()) }
    }

    let now = Instant::now();

    cmd.args(args);

    let has_color_args = args.iter().any(|e| {
        let e = e.as_ref().to_str().unwrap_or("");
        e.starts_with("--color")
    });

    if stderr_isatty() && !has_color_args {
        cmd.arg("--color");
        cmd.arg("always");
    }

    let mut cmd_err_file = tempfile().unwrap();
    let cmd_err_stdio = file_as_stdio(&cmd_err_file);

    // FIXME rust-lang/rust#32254. It's not clear to me
    // when and why this is needed.
    let mut cmd = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(cmd_err_stdio)
        .spawn()
        .unwrap();

    let status = cmd.wait();

    let duration = now.elapsed();

    let ms = (duration.as_secs() as u64 * 1000) + (duration.subsec_nanos() as u64 / 1000 / 1000);

    let t = Telemetry::new(cfg.elan_dir.join("telemetry"));

    match status {
        Ok(status) => {
            let exit_code = status.code().unwrap_or(1);

            let re = Regex::new(r"\[(?P<error>E.{4})\]").unwrap();

            let mut buffer = String::new();
            // Chose a HashSet instead of a Vec to avoid calls to sort() and dedup().
            // The HashSet should be faster if there are a lot of errors, too.
            let mut errors: Vec<String> = Vec::new();

            let stderr = io::stderr();
            let mut handle = stderr.lock();

            cmd_err_file.seek(SeekFrom::Start(0)).unwrap();

            let mut buffered_stderr = BufReader::new(cmd_err_file);

            while buffered_stderr.read_line(&mut buffer).unwrap() > 0 {
                let b = buffer.to_owned();
                buffer.clear();
                let _ = handle.write(b.as_bytes());

                if let Some(caps) = re.captures(&b) {
                    if caps.len() > 0 {
                        errors.push(
                            caps.name("error")
                                .map(|m| m.as_str())
                                .unwrap_or("")
                                .to_owned(),
                        );
                    }
                };
            }

            let e = if errors.is_empty() {
                None
            } else {
                Some(errors)
            };

            let te = TelemetryEvent::LeanRun {
                duration_ms: ms,
                exit_code: exit_code,
                errors: e,
            };

            let _ = t.log_telemetry(te).map_err(|xe| {
                (cfg.notify_handler)(Notification::TelemetryCleanupError(&xe));
            });

            process::exit(exit_code);
        }
        Err(e) => {
            let exit_code = e.raw_os_error().unwrap_or(1);
            let te = TelemetryEvent::LeanRun {
                duration_ms: ms,
                exit_code: exit_code,
                errors: None,
            };

            let _ = t.log_telemetry(te).map_err(|xe| {
                (cfg.notify_handler)(Notification::TelemetryCleanupError(&xe));
            });

            Err(e).chain_err(|| elan_utils::ErrorKind::RunningCommand {
                name: OsStr::new(arg0).to_owned(),
            })
        }
    }
}

fn exec_command_for_dir_without_telemetry<S: AsRef<OsStr>>(
    mut cmd: Command,
    arg0: &str,
    args: &[S],
) -> Result<()> {
    cmd.args(args);

    // FIXME rust-lang/rust#32254. It's not clear to me
    // when and why this is needed.
    cmd.stdin(process::Stdio::inherit());

    return exec(&mut cmd).chain_err(|| elan_utils::ErrorKind::RunningCommand {
        name: OsStr::new(arg0).to_owned(),
    });

    #[cfg(unix)]
    fn exec(cmd: &mut Command) -> io::Result<()> {
        use std::os::unix::prelude::*;
        Err(cmd.exec())
    }

    #[cfg(windows)]
    fn exec(cmd: &mut Command) -> io::Result<()> {
        let status = cmd.status()?;
        process::exit(status.code().unwrap());
    }
}

#[cfg(unix)]
fn stderr_isatty() -> bool {
    unsafe { libc::isatty(libc::STDERR_FILENO) != 0 }
}

#[cfg(windows)]
fn stderr_isatty() -> bool {
    type DWORD = u32;
    type BOOL = i32;
    type HANDLE = *mut u8;
    const STD_ERROR_HANDLE: DWORD = -12i32 as DWORD;
    extern "system" {
        fn GetStdHandle(which: DWORD) -> HANDLE;
        fn GetConsoleMode(hConsoleHandle: HANDLE, lpMode: *mut DWORD) -> BOOL;
    }
    unsafe {
        let handle = GetStdHandle(STD_ERROR_HANDLE);
        let mut out = 0;
        GetConsoleMode(handle, &mut out) != 0
    }
}
