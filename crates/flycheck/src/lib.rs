//! Flycheck provides the functionality needed to run `cargo check` or
//! another compatible command (f.x. clippy) in a background thread and provide
//! LSP diagnostics based on the output of the command.

use std::{
    fmt,
    io::{self, BufRead, BufReader},
    process::{self, Command, Stdio},
    time::Duration,
};

use crossbeam_channel::{never, select, unbounded, Receiver, Sender};
use paths::AbsPathBuf;
use serde::Deserialize;
use stdx::JodChild;

pub use cargo_metadata::diagnostic::{
    Applicability, Diagnostic, DiagnosticCode, DiagnosticLevel, DiagnosticSpan,
    DiagnosticSpanMacroExpansion,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlycheckConfig {
    CargoCommand {
        command: String,
        target_triple: Option<String>,
        all_targets: bool,
        no_default_features: bool,
        all_features: bool,
        features: Vec<String>,
        extra_args: Vec<String>,
    },
    CustomCommand {
        command: String,
        args: Vec<String>,
    },
}

impl fmt::Display for FlycheckConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlycheckConfig::CargoCommand { command, .. } => write!(f, "cargo {}", command),
            FlycheckConfig::CustomCommand { command, args } => {
                write!(f, "{} {}", command, args.join(" "))
            }
        }
    }
}

/// Flycheck wraps the shared state and communication machinery used for
/// running `cargo check` (or other compatible command) and providing
/// diagnostics based on the output.
/// The spawned thread is shut down when this struct is dropped.
#[derive(Debug)]
pub struct FlycheckHandle {
    // XXX: drop order is significant
    sender: Sender<Restart>,
    thread: jod_thread::JoinHandle,
}

impl FlycheckHandle {
    pub fn spawn(
        id: usize,
        sender: Box<dyn Fn(Message) + Send>,
        config: FlycheckConfig,
        workspace_root: AbsPathBuf,
    ) -> FlycheckHandle {
        let actor = FlycheckActor::new(id, sender, config, workspace_root);
        let (sender, receiver) = unbounded::<Restart>();
        let thread = jod_thread::Builder::new()
            .name("Flycheck".to_owned())
            .spawn(move || actor.run(receiver))
            .expect("failed to spawn thread");
        FlycheckHandle { sender, thread }
    }

    /// Schedule a re-start of the cargo check worker.
    pub fn update(&self) {
        self.sender.send(Restart).unwrap();
    }
}

pub enum Message {
    /// Request adding a diagnostic with fixes included to a file
    AddDiagnostic { workspace_root: AbsPathBuf, diagnostic: Diagnostic },

    /// Request check progress notification to client
    Progress {
        /// Flycheck instance ID
        id: usize,
        progress: Progress,
    },
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Message::AddDiagnostic { workspace_root, diagnostic } => f
                .debug_struct("AddDiagnostic")
                .field("workspace_root", workspace_root)
                .field("diagnostic_code", &diagnostic.code.as_ref().map(|it| &it.code))
                .finish(),
            Message::Progress { id, progress } => {
                f.debug_struct("Progress").field("id", id).field("progress", progress).finish()
            }
        }
    }
}

#[derive(Debug)]
pub enum Progress {
    DidStart,
    DidCheckCrate(String),
    DidFinish(io::Result<()>),
    DidCancel,
}

struct Restart;

struct FlycheckActor {
    id: usize,
    sender: Box<dyn Fn(Message) + Send>,
    config: FlycheckConfig,
    workspace_root: AbsPathBuf,
    /// WatchThread exists to wrap around the communication needed to be able to
    /// run `cargo check` without blocking. Currently the Rust standard library
    /// doesn't provide a way to read sub-process output without blocking, so we
    /// have to wrap sub-processes output handling in a thread and pass messages
    /// back over a channel.
    cargo_handle: Option<CargoHandle>,
}

enum Event {
    Restart(Restart),
    CheckEvent(Option<CargoMessage>),
}

impl FlycheckActor {
    fn new(
        id: usize,
        sender: Box<dyn Fn(Message) + Send>,
        config: FlycheckConfig,
        workspace_root: AbsPathBuf,
    ) -> FlycheckActor {
        FlycheckActor { id, sender, config, workspace_root, cargo_handle: None }
    }
    fn progress(&self, progress: Progress) {
        self.send(Message::Progress { id: self.id, progress });
    }
    fn next_event(&self, inbox: &Receiver<Restart>) -> Option<Event> {
        let check_chan = self.cargo_handle.as_ref().map(|cargo| &cargo.receiver);
        select! {
            recv(inbox) -> msg => msg.ok().map(Event::Restart),
            recv(check_chan.unwrap_or(&never())) -> msg => Some(Event::CheckEvent(msg.ok())),
        }
    }
    fn run(mut self, inbox: Receiver<Restart>) {
        while let Some(event) = self.next_event(&inbox) {
            match event {
                Event::Restart(Restart) => {
                    while let Ok(Restart) = inbox.recv_timeout(Duration::from_millis(50)) {}

                    self.cancel_check_process();

                    let mut command = self.check_command();
                    tracing::info!("restart flycheck {:?}", command);
                    command.stdout(Stdio::piped()).stderr(Stdio::null()).stdin(Stdio::null());
                    if let Ok(child) = command.spawn().map(JodChild) {
                        self.cargo_handle = Some(CargoHandle::spawn(child));
                        self.progress(Progress::DidStart);
                    }
                }
                Event::CheckEvent(None) => {
                    // Watcher finished, replace it with a never channel to
                    // avoid busy-waiting.
                    let cargo_handle = self.cargo_handle.take().unwrap();
                    let res = cargo_handle.join();
                    if res.is_err() {
                        tracing::error!(
                            "Flycheck failed to run the following command: {:?}",
                            self.check_command()
                        )
                    }
                    self.progress(Progress::DidFinish(res));
                }
                Event::CheckEvent(Some(message)) => match message {
                    CargoMessage::CompilerArtifact(msg) => {
                        self.progress(Progress::DidCheckCrate(msg.target.name));
                    }

                    CargoMessage::Diagnostic(msg) => {
                        self.send(Message::AddDiagnostic {
                            workspace_root: self.workspace_root.clone(),
                            diagnostic: msg,
                        });
                    }
                },
            }
        }
        // If we rerun the thread, we need to discard the previous check results first
        self.cancel_check_process();
    }
    fn cancel_check_process(&mut self) {
        if self.cargo_handle.take().is_some() {
            self.progress(Progress::DidCancel);
        }
    }
    fn check_command(&self) -> Command {
        let mut cmd = match &self.config {
            FlycheckConfig::CargoCommand {
                command,
                target_triple,
                no_default_features,
                all_targets,
                all_features,
                extra_args,
                features,
            } => {
                let mut cmd = Command::new(toolchain::cargo());
                cmd.arg(command);
                cmd.current_dir(&self.workspace_root);
                cmd.args(&["--workspace", "--message-format=json", "--manifest-path"])
                    .arg(self.workspace_root.join("Cargo.toml").as_os_str());

                if let Some(target) = target_triple {
                    cmd.args(&["--target", target.as_str()]);
                }
                if *all_targets {
                    cmd.arg("--all-targets");
                }
                if *all_features {
                    cmd.arg("--all-features");
                } else {
                    if *no_default_features {
                        cmd.arg("--no-default-features");
                    }
                    if !features.is_empty() {
                        cmd.arg("--features");
                        cmd.arg(features.join(" "));
                    }
                }
                cmd.args(extra_args);
                cmd
            }
            FlycheckConfig::CustomCommand { command, args } => {
                let mut cmd = Command::new(command);
                cmd.args(args);
                cmd
            }
        };
        cmd.current_dir(&self.workspace_root);
        cmd
    }

    fn send(&self, check_task: Message) {
        (self.sender)(check_task)
    }
}

struct CargoHandle {
    child: JodChild,
    #[allow(unused)]
    thread: jod_thread::JoinHandle<io::Result<bool>>,
    receiver: Receiver<CargoMessage>,
}

impl CargoHandle {
    fn spawn(mut child: JodChild) -> CargoHandle {
        let child_stdout = child.stdout.take().unwrap();
        let (sender, receiver) = unbounded();
        let actor = CargoActor::new(child_stdout, sender);
        let thread = jod_thread::Builder::new()
            .name("CargoHandle".to_owned())
            .spawn(move || actor.run())
            .expect("failed to spawn thread");
        CargoHandle { child, thread, receiver }
    }
    fn join(mut self) -> io::Result<()> {
        // It is okay to ignore the result, as it only errors if the process is already dead
        let _ = self.child.kill();
        let exit_status = self.child.wait()?;
        let read_at_least_one_message = self.thread.join()?;
        if !exit_status.success() && !read_at_least_one_message {
            // FIXME: Read the stderr to display the reason, see `read2()` reference in PR comment:
            // https://github.com/rust-analyzer/rust-analyzer/pull/3632#discussion_r395605298
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "Cargo watcher failed, the command produced no valid metadata (exit code: {:?})",
                    exit_status
                ),
            ));
        }
        Ok(())
    }
}

struct CargoActor {
    child_stdout: process::ChildStdout,
    sender: Sender<CargoMessage>,
}

impl CargoActor {
    fn new(child_stdout: process::ChildStdout, sender: Sender<CargoMessage>) -> CargoActor {
        CargoActor { child_stdout, sender }
    }
    fn run(self) -> io::Result<bool> {
        // We manually read a line at a time, instead of using serde's
        // stream deserializers, because the deserializer cannot recover
        // from an error, resulting in it getting stuck, because we try to
        // be resilient against failures.
        //
        // Because cargo only outputs one JSON object per line, we can
        // simply skip a line if it doesn't parse, which just ignores any
        // erroneus output.
        let stdout = BufReader::new(self.child_stdout);
        let mut read_at_least_one_message = false;
        for message in stdout.lines() {
            let message = match message {
                Ok(message) => message,
                Err(err) => {
                    tracing::error!("Invalid json from cargo check, ignoring ({})", err);
                    continue;
                }
            };

            read_at_least_one_message = true;

            // Try to deserialize a message from Cargo or Rustc.
            let mut deserializer = serde_json::Deserializer::from_str(&message);
            deserializer.disable_recursion_limit();
            if let Ok(message) = JsonMessage::deserialize(&mut deserializer) {
                match message {
                    // Skip certain kinds of messages to only spend time on what's useful
                    JsonMessage::Cargo(message) => match message {
                        cargo_metadata::Message::CompilerArtifact(artifact) if !artifact.fresh => {
                            self.sender.send(CargoMessage::CompilerArtifact(artifact)).unwrap()
                        }
                        cargo_metadata::Message::CompilerMessage(msg) => {
                            self.sender.send(CargoMessage::Diagnostic(msg.message)).unwrap()
                        }
                        _ => (),
                    },
                    JsonMessage::Rustc(message) => {
                        self.sender.send(CargoMessage::Diagnostic(message)).unwrap()
                    }
                }
            }
        }
        Ok(read_at_least_one_message)
    }
}

enum CargoMessage {
    CompilerArtifact(cargo_metadata::Artifact),
    Diagnostic(Diagnostic),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum JsonMessage {
    Cargo(cargo_metadata::Message),
    Rustc(Diagnostic),
}
