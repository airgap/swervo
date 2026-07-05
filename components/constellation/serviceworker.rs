/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use ipc_channel::IpcError;
use serde::{Deserialize, Serialize};
use servo_base::generic_channel::GenericSender;
use servo_base::id::PipelineNamespace;
use servo_config::opts::{self, Opts};
use servo_config::prefs;
use servo_config::prefs::Preferences;
use servo_constellation_traits::{SWManagerSenders, ServiceWorkerManagerFactory};
use servo_url::ImmutableOrigin;

use crate::process_manager::Process;
use crate::sandboxing::{UnprivilegedContent, spawn_multiprocess};

/// Conceptually, this is glue to start an agent-cluster for a service worker agent.
/// <https://html.spec.whatwg.org/multipage/#obtain-a-service-worker-agent>
#[derive(Deserialize, Serialize)]
pub struct ServiceWorkerUnprivilegedContent {
    pub opts: Opts,
    pub prefs: Box<Preferences>,
    senders: SWManagerSenders,
    origin: ImmutableOrigin,
    lifeline_sender: Option<GenericSender<()>>,
}

impl ServiceWorkerUnprivilegedContent {
    pub fn new(
        senders: SWManagerSenders,
        origin: ImmutableOrigin,
        lifeline_sender: Option<GenericSender<()>>,
    ) -> ServiceWorkerUnprivilegedContent {
        ServiceWorkerUnprivilegedContent {
            opts: (*opts::get()).clone(),
            prefs: Box::new(prefs::get().clone()),
            senders,
            origin,
            lifeline_sender,
        }
    }

    /// Start the agent-cluster.
    pub fn start<SWF>(self)
    where
        SWF: ServiceWorkerManagerFactory,
    {
        let multiprocess = opts::get().multiprocess;
        if multiprocess {
            // This is the dedicated service-worker process's main: prime the pipeline-namespace
            // installer so id allocation requests reach the constellation (without this the
            // manager thread panics on its first id), then keep the process alive until the
            // manager exits. In single-process mode the installer is already primed and the
            // manager thread just shares the process.
            PipelineNamespace::set_installer_sender(
                self.senders.namespace_request_sender.clone(),
            );
        }
        let join_handle = SWF::create(self.senders, self.origin);
        if multiprocess && let Some(join_handle) = join_handle {
            let _ = join_handle.join();
        }
    }

    /// Start the agent-cluster in it's own process.
    pub fn spawn_multiprocess(self) -> Result<Process, IpcError> {
        spawn_multiprocess(UnprivilegedContent::ServiceWorker(self))
    }
}
