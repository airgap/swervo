/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::ffi::OsStr;
use std::{env, process};

#[cfg(target_os = "macos")]
use gaol::profile::{Operation, PathPattern, Profile};
use ipc_channel::IpcError;
use serde::{Deserialize, Serialize};
use servo_config::opts::Opts;
use servo_config::prefs::Preferences;

use crate::event_loop::NewScriptEventLoopProcessInfo;
use crate::process_manager::Process;
use crate::serviceworker::ServiceWorkerUnprivilegedContent;

#[derive(Deserialize, Serialize)]
#[expect(clippy::large_enum_variant)]
pub enum UnprivilegedContent {
    ScriptEventLoop(NewScriptEventLoopProcessInfo),
    ServiceWorker(ServiceWorkerUnprivilegedContent),
}

impl UnprivilegedContent {
    pub fn opts(&self) -> Opts {
        match self {
            UnprivilegedContent::ScriptEventLoop(content) => content.opts.clone(),
            UnprivilegedContent::ServiceWorker(content) => content.opts.clone(),
        }
    }

    pub fn prefs(&self) -> &Preferences {
        match self {
            UnprivilegedContent::ScriptEventLoop(content) => &content.prefs,
            UnprivilegedContent::ServiceWorker(content) => &content.prefs,
        }
    }
}

/// Our content process sandbox profile on Mac. As restrictive as possible.
#[cfg(target_os = "macos")]
pub fn content_process_sandbox_profile() -> Profile {
    use std::path::PathBuf;

    use embedder_traits::resources;
    use gaol::platform;

    let mut operations = vec![
        Operation::FileReadAll(PathPattern::Literal(PathBuf::from("/dev/urandom"))),
        Operation::FileReadAll(PathPattern::Subpath(PathBuf::from("/Library/Fonts"))),
        Operation::FileReadAll(PathPattern::Subpath(PathBuf::from("/System/Library/Fonts"))),
        Operation::FileReadAll(PathPattern::Subpath(PathBuf::from(
            "/System/Library/Frameworks/ApplicationServices.framework",
        ))),
        Operation::FileReadAll(PathPattern::Subpath(PathBuf::from(
            "/System/Library/Frameworks/CoreGraphics.framework",
        ))),
        Operation::FileReadMetadata(PathPattern::Literal(PathBuf::from("/"))),
        Operation::FileReadMetadata(PathPattern::Literal(PathBuf::from("/Library"))),
        Operation::FileReadMetadata(PathPattern::Literal(PathBuf::from("/System"))),
        Operation::FileReadMetadata(PathPattern::Literal(PathBuf::from("/etc"))),
        Operation::SystemInfoRead,
        Operation::PlatformSpecific(platform::macos::Operation::MachLookup(
            b"com.apple.FontServer".to_vec(),
        )),
    ];

    operations.extend(
        resources::sandbox_access_files()
            .into_iter()
            .map(|p| Operation::FileReadAll(PathPattern::Literal(p))),
    );
    operations.extend(
        resources::sandbox_access_files_dirs()
            .into_iter()
            .map(|p| Operation::FileReadAll(PathPattern::Subpath(p))),
    );

    Profile::new(operations).expect("Failed to create sandbox profile!")
}

// The Linux content-process sandbox profile is no longer gaol-based; the
// policy is built by `sandbox_backend::content_process_policy()` and applied
// (Landlock + seccomp) in the child. See `crate::sandbox_backend`.

#[cfg(any(
    target_os = "windows",
    target_os = "android",
    target_env = "ohos",
    target_arch = "arm",
    all(target_arch = "aarch64", not(target_os = "macos")),
    target_arch = "riscv32",
    target_arch = "riscv64"
))]
pub fn spawn_multiprocess(content: UnprivilegedContent) -> Result<Process, IpcError> {
    use ipc_channel::ipc::{IpcOneShotServer, IpcSender};
    // Note that this function can panic, due to process creation,
    // avoiding this panic would require a mechanism for dealing
    // with low-resource scenarios.
    let (server, token) = IpcOneShotServer::<IpcSender<UnprivilegedContent>>::new()
        .expect("Failed to create IPC one-shot server.");

    let path_to_self = env::current_exe().expect("Failed to get current executor.");
    let mut child_process = process::Command::new(path_to_self);
    setup_common(&mut child_process, token, content.opts().sandbox);

    #[allow(clippy::zombie_processes)]
    let child = child_process
        .spawn()
        .expect("Failed to start unsandboxed child process!");

    let (_receiver, sender) = server.accept().expect("Server failed to accept.");
    sender.send(content)?;

    Ok(Process::Unsandboxed(child))
}

// macOS keeps the gaol child-process sandbox (parent starts the sandboxed child).
#[cfg(target_os = "macos")]
pub fn spawn_multiprocess(content: UnprivilegedContent) -> Result<Process, IpcError> {
    use gaol::sandbox::{self, Sandbox, SandboxMethods};
    use ipc_channel::ipc::{IpcOneShotServer, IpcSender};

    // TODO: Move this impl out of the function. It is only currently here to avoid
    // duplicating the feature flagging.
    #[allow(non_local_definitions)]
    impl CommandMethods for gaol::sandbox::Command {
        fn arg<T>(&mut self, arg: T)
        where
            T: AsRef<OsStr>,
        {
            self.arg(arg);
        }

        fn env<T, U>(&mut self, key: T, val: U)
        where
            T: AsRef<OsStr>,
            U: AsRef<OsStr>,
        {
            self.env(key, val);
        }
    }

    // Note that this function can panic, due to process creation,
    // avoiding this panic would require a mechanism for dealing
    // with low-resource scenarios.
    let (server, token) = IpcOneShotServer::<IpcSender<UnprivilegedContent>>::new()
        .expect("Failed to create IPC one-shot server.");

    // If there is a sandbox, use the `gaol` API to create the child process.
    let process = if content.opts().sandbox {
        let mut command = sandbox::Command::me().expect("Failed to get current sandbox.");
        setup_common(&mut command, token, content.opts().sandbox);

        let profile = content_process_sandbox_profile();
        Process::Sandboxed(
            Sandbox::new(profile)
                .start(&mut command)
                .expect("Failed to start sandboxed child process!")
                .pid as u32,
        )
    } else {
        let path_to_self = env::current_exe().expect("Failed to get current executor.");
        let mut child_process = process::Command::new(path_to_self);
        setup_common(&mut child_process, token, content.opts().sandbox);

        Process::Unsandboxed(
            child_process
                .spawn()
                .expect("Failed to start unsandboxed child process!"),
        )
    };

    let (_receiver, sender) = server.accept().expect("Server failed to accept.");
    sender.send(content)?;

    Ok(process)
}

// Linux (and other non-macOS gaol-free supported targets): Landlock + seccomp
// self-apply in the child (see `create_sandbox` / `crate::sandbox_backend`), so
// the parent just plain-spawns the content process. The child is always
// `Process::Unsandboxed(Child)` — a real `std::process::Child` we can reap.
#[cfg(all(
    not(target_os = "macos"),
    not(target_os = "windows"),
    not(target_os = "ios"),
    not(target_os = "android"),
    not(target_env = "ohos"),
    not(target_arch = "arm"),
    not(target_arch = "aarch64"),
    not(target_arch = "riscv32"),
    not(target_arch = "riscv64")
))]
pub fn spawn_multiprocess(content: UnprivilegedContent) -> Result<Process, IpcError> {
    use ipc_channel::ipc::{IpcOneShotServer, IpcSender};
    // Note that this function can panic, due to process creation,
    // avoiding this panic would require a mechanism for dealing
    // with low-resource scenarios.
    let (server, token) = IpcOneShotServer::<IpcSender<UnprivilegedContent>>::new()
        .expect("Failed to create IPC one-shot server.");

    let path_to_self = env::current_exe().expect("Failed to get current executor.");
    let mut child_process = process::Command::new(path_to_self);
    setup_common(&mut child_process, token.clone(), content.opts().sandbox);

    let mut child = child_process
        .spawn()
        .expect("Failed to start unsandboxed child process!");

    let (_receiver, sender) = accept_child_bootstrap(
        server,
        &token,
        &mut child,
        std::time::Duration::from_secs(30),
    )?;
    if let Err(error) = sender.send(content) {
        reap_failed_bootstrap(&mut child);
        return Err(error);
    }

    Ok(Process::Unsandboxed(child))
}

#[cfg(all(
    not(target_os = "macos"),
    not(target_os = "windows"),
    not(target_os = "ios"),
    not(target_os = "android"),
    not(target_env = "ohos"),
    not(target_arch = "arm"),
    not(target_arch = "aarch64"),
    not(target_arch = "riscv32"),
    not(target_arch = "riscv64")
))]
/// A child can fail confinement before it connects. Never strand the parent in accept().
fn accept_child_bootstrap<T>(
    server: ipc_channel::ipc::IpcOneShotServer<T>,
    token: &str,
    child: &mut process::Child,
    timeout: std::time::Duration,
) -> Result<(ipc_channel::ipc::IpcReceiver<T>, T), IpcError>
where
    T: serde::de::DeserializeOwned + Serialize + Send + 'static,
{
    use std::io::{Error, ErrorKind};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::{Duration, Instant};

    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = match std::thread::Builder::new()
        .name("RendererBootstrap".into())
        .spawn(move || {
            let _ = sender.send(server.accept());
        }) {
        Ok(worker) => worker,
        Err(error) => {
            reap_failed_bootstrap(child);
            return Err(error.into());
        },
    };
    let started = Instant::now();
    let result = loop {
        match receiver.recv_timeout(Duration::from_millis(20)) {
            Ok(result) => break result,
            Err(RecvTimeoutError::Disconnected) => {
                break Err(Error::new(
                    ErrorKind::BrokenPipe,
                    "renderer bootstrap worker disconnected",
                )
                .into());
            },
            Err(RecvTimeoutError::Timeout) => {},
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                break Err(Error::new(
                    ErrorKind::BrokenPipe,
                    format!("renderer exited before IPC bootstrap: {status}"),
                )
                .into());
            },
            Err(error) => break Err(error.into()),
            Ok(None) => {},
        }
        if started.elapsed() >= timeout {
            break Err(Error::new(ErrorKind::TimedOut, "renderer IPC bootstrap timed out").into());
        }
    };
    let wakeup = if result.is_err() {
        reap_failed_bootstrap(child);
        let token = token.to_owned();
        // Unix connect() can block too. Keep cancellation off the parent thread
        // and bound cleanup even if the OS cannot create a wake-up connection.
        match std::thread::Builder::new()
            .name("RendererBootstrapCancel".into())
            .spawn(move || {
                if let Err(error) = ipc_channel::ipc::IpcSender::<()>::connect(token) {
                    log::debug!("Renderer bootstrap wakeup: {error}");
                }
            }) {
            Ok(wakeup) => Some(wakeup),
            Err(error) => {
                log::error!("Could not start renderer bootstrap cleanup: {error}");
                None
            },
        }
    } else {
        None
    };
    let cleanup = Instant::now();
    while (!worker.is_finished() || wakeup.as_ref().is_some_and(|task| !task.is_finished())) &&
        cleanup.elapsed() < Duration::from_millis(250)
    {
        std::thread::sleep(Duration::from_millis(2));
    }
    if worker.is_finished() {
        if worker.join().is_err() {
            return Err(Error::other("renderer bootstrap worker panicked").into());
        }
    } else {
        log::error!(
            "Renderer bootstrap cleanup exceeded its deadline; accept worker remains detached"
        );
    }
    if let Some(wakeup) = wakeup {
        if wakeup.is_finished() {
            if wakeup.join().is_err() {
                log::error!("Renderer bootstrap wakeup worker panicked");
            }
        } else {
            log::error!(
                "Renderer bootstrap cleanup exceeded its deadline; wakeup worker remains detached"
            );
        }
    }
    result
}

#[cfg(all(
    not(target_os = "macos"),
    not(target_os = "windows"),
    not(target_os = "ios"),
    not(target_os = "android"),
    not(target_env = "ohos"),
    not(target_arch = "arm"),
    not(target_arch = "aarch64"),
    not(target_arch = "riscv32"),
    not(target_arch = "riscv64")
))]
fn reap_failed_bootstrap(child: &mut process::Child) {
    use std::time::{Duration, Instant};
    match child.try_wait() {
        Ok(Some(_)) => return,
        Ok(None) => {},
        Err(error) => log::error!("Could not inspect failed renderer {}: {error}", child.id()),
    }
    if let Err(error) = child.kill() {
        log::error!(
            "Could not terminate failed renderer {}: {error}",
            child.id()
        );
    }
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {},
            Err(error) => {
                log::error!("Could not reap failed renderer {}: {error}", child.id());
                return;
            },
        }
        if started.elapsed() >= Duration::from_millis(250) {
            log::error!(
                "Failed renderer {} was not reaped before the cleanup deadline",
                child.id()
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(target_os = "ios")]
pub fn spawn_multiprocess(_content: UnprivilegedContent) -> Result<Process, Error> {
    log::error!("Multiprocess is not supported on iOS.");
    process::exit(1);
}

fn setup_common<C: CommandMethods>(command: &mut C, token: String, sandbox: bool) {
    // The Linux child must enter Landlock while it is still single-threaded,
    // before IPC bootstrap can start helper threads. Confirm this request against
    // the actual serialized options after the IPC handshake.
    C::env(
        command,
        "SERVO_CONTENT_SANDBOX",
        if sandbox { "1" } else { "0" },
    );
    C::arg(command, "--content-process");
    C::arg(command, token);

    if let Ok(value) = env::var("RUST_BACKTRACE") {
        C::env(command, "RUST_BACKTRACE", value);
    }

    if let Ok(value) = env::var("RUST_LOG") {
        C::env(command, "RUST_LOG", value);
    }
}

/// A trait to unify commands launched as multiprocess with or without a sandbox.
trait CommandMethods {
    /// A command line argument.
    fn arg<T>(&mut self, arg: T)
    where
        T: AsRef<OsStr>;

    /// An environment variable.
    fn env<T, U>(&mut self, key: T, val: U)
    where
        T: AsRef<OsStr>,
        U: AsRef<OsStr>;
}

impl CommandMethods for process::Command {
    fn arg<T>(&mut self, arg: T)
    where
        T: AsRef<OsStr>,
    {
        self.arg(arg);
    }

    fn env<T, U>(&mut self, key: T, val: U)
    where
        T: AsRef<OsStr>,
        U: AsRef<OsStr>,
    {
        self.env(key, val);
    }
}

#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
mod bootstrap_tests {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use ipc_channel::ipc::{IpcOneShotServer, IpcSender};

    use super::accept_child_bootstrap;

    #[test]
    fn confinement_exit_before_connect_does_not_hang_accept() {
        let (server, token) = IpcOneShotServer::<usize>::new().expect("server");
        let mut child = Command::new("sh")
            .args(["-c", "exit 78"])
            .stdout(Stdio::null())
            .spawn()
            .expect("child");
        let start = Instant::now();
        assert!(
            accept_child_bootstrap(server, &token, &mut child, Duration::from_secs(2)).is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(child.try_wait().expect("child status").is_some());
    }

    #[test]
    fn stalled_child_is_reaped_and_accept_worker_is_released() {
        let (server, token) = IpcOneShotServer::<usize>::new().expect("server");
        let mut child = Command::new("sh")
            .args(["-c", "exec sleep 60"])
            .spawn()
            .expect("child");
        assert!(
            accept_child_bootstrap(server, &token, &mut child, Duration::from_millis(50)).is_err()
        );
        assert!(child.try_wait().expect("child status").is_some());
    }

    #[test]
    #[ignore = "re-executed as a child by connected_child_without_a_message_times_out"]
    fn connected_child_fixture() {
        let token = std::env::var("SERVO_TEST_BOOTSTRAP_TOKEN").expect("fixture token");
        let marker = std::env::var("SERVO_TEST_BOOTSTRAP_MARKER").expect("fixture marker");
        let _peer = IpcSender::<usize>::connect(token).expect("connect");
        std::fs::write(marker, b"connected").expect("marker");
        std::thread::sleep(Duration::from_secs(60));
    }

    #[test]
    fn connected_child_without_a_message_times_out() {
        let directory = std::env::temp_dir().join(format!(
            "servo-bootstrap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir(&directory).expect("fixture directory");
        let marker = directory.join("connected");
        let (server, token) = IpcOneShotServer::<usize>::new().expect("server");
        let mut child = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--ignored",
                "--exact",
                "sandboxing::bootstrap_tests::connected_child_fixture",
            ])
            .env("SERVO_TEST_BOOTSTRAP_TOKEN", &token)
            .env("SERVO_TEST_BOOTSTRAP_MARKER", &marker)
            .stdout(Stdio::null())
            .spawn()
            .expect("child");
        let started = Instant::now();
        while !marker.exists() && started.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(2));
        }
        if !marker.exists() {
            child.kill().expect("terminate failed fixture");
            child.wait().expect("reap failed fixture");
        }
        assert!(
            marker.exists(),
            "child must connect before the monitored receive starts"
        );
        assert!(
            accept_child_bootstrap(server, &token, &mut child, Duration::from_millis(50)).is_err()
        );
        assert!(child.try_wait().expect("child status").is_some());
        std::fs::remove_file(marker).expect("remove marker");
        std::fs::remove_dir(directory).expect("remove fixture directory");
    }

    #[test]
    fn failed_wakeup_cannot_block_the_parent() {
        let (server, token) = IpcOneShotServer::<usize>::new().expect("server");
        let invalid_token = format!("{token}.not-a-server");
        let mut child = Command::new("sh")
            .args(["-c", "exit 78"])
            .spawn()
            .expect("child");
        let (sender, receiver) = std::sync::mpsc::channel();
        let monitor = std::thread::spawn(move || {
            let started = Instant::now();
            let result = accept_child_bootstrap(
                server,
                &invalid_token,
                &mut child,
                Duration::from_millis(50),
            );
            let reaped = child.try_wait().expect("child status").is_some();
            let _ = sender.send((result.is_err(), reaped, started.elapsed()));
        });
        let result = receiver.recv_timeout(Duration::from_secs(1));
        // Release the real server even when the deadline assertion would fail,
        // so this regression never leaves its fixture accept worker behind.
        drop(IpcSender::<usize>::connect(token.clone()).expect("release real server"));
        monitor.join().expect("monitor");
        let (failed, reaped, elapsed) =
            result.expect("parent must return despite failed cleanup wakeup");
        assert!(failed && reaped);
        assert!(elapsed < Duration::from_secs(1));
        let cleanup = Instant::now();
        while std::path::Path::new(&token).exists() && cleanup.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            !std::path::Path::new(&token).exists(),
            "fixture accept worker must release its owned socket"
        );
    }

    #[test]
    fn successful_handshake_keeps_the_child_alive() {
        let (server, token) = IpcOneShotServer::<usize>::new().expect("server");
        let mut child = Command::new("sh")
            .args(["-c", "exec sleep 60"])
            .spawn()
            .expect("child");
        let peer_token = token.clone();
        let peer = std::thread::spawn(move || {
            IpcSender::connect(peer_token)
                .expect("connect")
                .send(7usize)
                .expect("send")
        });
        let (_, value) = accept_child_bootstrap(server, &token, &mut child, Duration::from_secs(2))
            .expect("bootstrap");
        assert_eq!(value, 7);
        assert!(child.try_wait().expect("child status").is_none());
        child.kill().expect("terminate");
        child.wait().expect("reap");
        peer.join().expect("peer");
    }
}
