/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::process::{Child, ExitStatus};

use crossbeam_channel::{Receiver, Select};
use log::{debug, error, warn};
use profile_traits::mem::{ProfilerChan, ProfilerMsg};
use servo_base::id::ScriptEventLoopId;

pub enum Process {
    Unsandboxed(Child),
    Sandboxed(u32),
}

impl Process {
    fn pid(&self) -> u32 {
        match self {
            Self::Unsandboxed(child) => child.id(),
            Self::Sandboxed(pid) => *pid,
        }
    }

    fn wait(&mut self) -> Option<ExitStatus> {
        match self {
            Self::Unsandboxed(child) => match child.wait() {
                Ok(status) => Some(status),
                Err(error) => {
                    error!("Could not reap process pid={}: {error}", child.id());
                    None
                },
            },
            Self::Sandboxed(_pid) => {
                // TODO: use nix::waitpid() on supported platforms.
                warn!("wait() is not yet implemented for sandboxed processes.");
                None
            },
        }
    }
}

type ProcessReceiver = Receiver<Result<(), ipc_channel::IpcError>>;

/// A process that was reaped after its lifeline closed.
pub(crate) struct ExitedProcess {
    /// The script event loop the process hosted, or `None` for a service worker process.
    pub event_loop_id: Option<ScriptEventLoopId>,
    pub status: Option<ExitStatus>,
}

pub(crate) struct ProcessManager {
    processes: Vec<(Process, ProcessReceiver, Option<ScriptEventLoopId>)>,
    mem_profiler_chan: ProfilerChan,
}

impl ProcessManager {
    pub fn new(mem_profiler_chan: ProfilerChan) -> Self {
        Self {
            processes: vec![],
            mem_profiler_chan,
        }
    }

    pub fn add(
        &mut self,
        receiver: ProcessReceiver,
        process: Process,
        event_loop_id: Option<ScriptEventLoopId>,
    ) {
        debug!("Adding process pid={}", process.pid());
        self.processes.push((process, receiver, event_loop_id));
    }

    pub fn register<'a>(&'a self, select: &mut Select<'a>) {
        for (_, receiver, _) in &self.processes {
            select.recv(receiver);
        }
    }

    pub fn receiver_at(&self, index: usize) -> &ProcessReceiver {
        let (_, receiver, _) = &self.processes[index];
        receiver
    }

    #[servo_tracing::instrument(skip_all)]
    pub fn remove(&mut self, index: usize) -> ExitedProcess {
        let (mut process, _, event_loop_id) = self.processes.swap_remove(index);
        debug!("Removing process pid={}", process.pid());
        // Unregister this process system memory profiler
        self.mem_profiler_chan
            .send(ProfilerMsg::UnregisterReporter(format!(
                "system-content-{}",
                process.pid()
            )));
        let status = process.wait();
        debug!("Process pid={} exited: {status:?}", process.pid());
        ExitedProcess {
            event_loop_id,
            status,
        }
    }
}
