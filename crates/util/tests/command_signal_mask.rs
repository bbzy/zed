#![cfg(target_os = "macos")]

use std::{io, process::Output, time::Duration};
use util::command::{Command, new_std_command};

struct SignalMask(libc::sigset_t);

impl SignalMask {
    fn block_child_signals() -> io::Result<Self> {
        let mut signals = unsafe { std::mem::zeroed() };
        if unsafe { libc::sigemptyset(&mut signals) } == -1
            || unsafe { libc::sigaddset(&mut signals, libc::SIGCHLD) } == -1
            || unsafe { libc::sigaddset(&mut signals, libc::SIGUSR1) } == -1
        {
            return Err(io::Error::last_os_error());
        }

        let mut previous = unsafe { std::mem::zeroed() };
        let result = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &signals, &mut previous) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }

        Ok(Self(previous))
    }
}

impl Drop for SignalMask {
    fn drop(&mut self) {
        let result =
            unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut()) };
        assert_eq!(result, 0, "restore test thread signal mask");
    }
}

fn current_mask() -> io::Result<libc::sigset_t> {
    let mut signals = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut signals) };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result));
    }

    Ok(signals)
}

async fn bounded_output(output: impl Future<Output = io::Result<Output>>) -> io::Result<Output> {
    smol::future::or(output, async {
        smol::Timer::after(Duration::from_secs(10)).await;
        Err(io::Error::new(io::ErrorKind::TimedOut, "signal mask probe"))
    })
    .await
}

fn check_spawn(standard: bool) -> anyhow::Result<()> {
    let previous = current_mask()?;
    {
        let _mask = SignalMask::block_child_signals()?;
        let blocked = current_mask()?;
        assert_eq!(unsafe { libc::sigismember(&blocked, libc::SIGCHLD) }, 1);
        assert_eq!(unsafe { libc::sigismember(&blocked, libc::SIGUSR1) }, 1);

        let executable = std::env::current_exe()?;
        let arguments = ["--exact", "signal_mask_child", "--ignored", "--nocapture"];
        let output = if standard {
            let mut command = smol::process::Command::from(new_std_command(&executable));
            command.args(arguments).kill_on_drop(true);
            smol::block_on(bounded_output(command.output()))?
        } else {
            let mut command = Command::new(&executable);
            command.args(arguments).kill_on_drop(true);
            smol::block_on(bounded_output(command.output()))?
        };
        assert!(
            output.status.success(),
            "probe failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(current_mask()?, blocked);

        let missing = executable.with_file_name("zed-nonexistent-signal-mask-probe");
        let error = if standard {
            new_std_command(missing).spawn().err()
        } else {
            Command::new(missing).spawn().err()
        }
        .expect("missing executable must fail");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(current_mask()?, blocked);
    }
    assert_eq!(current_mask()?, previous);

    Ok(())
}

#[test]
fn standard_command_resets_child_signal_mask() -> anyhow::Result<()> {
    check_spawn(true)
}

#[test]
fn asynchronous_command_resets_child_signal_mask() -> anyhow::Result<()> {
    check_spawn(false)
}

#[test]
#[ignore = "subprocess helper for signal mask regression tests"]
fn signal_mask_child() -> anyhow::Result<()> {
    assert_eq!(current_mask()?, 0, "child inherited blocked signals");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        // Use Tokio's unmodified launcher, as Watchman's connection discovery does.
        let mut child = tokio::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "delayed_child", "--ignored"])
            .kill_on_drop(true)
            .spawn()?;
        assert!(child.try_wait()?.is_none());
        let status = tokio::time::timeout(Duration::from_secs(3), child.wait()).await??;
        assert!(status.success());
        anyhow::Ok(())
    })
}

#[test]
#[ignore = "subprocess helper for Tokio child exit notification"]
fn delayed_child() {
    std::thread::sleep(Duration::from_millis(250));
}
