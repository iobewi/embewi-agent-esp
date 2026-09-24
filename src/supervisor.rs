//! Application service supervisor.
//!
//! Transport managers report capabilities becoming ready; this supervisor
//! decides which Embewi services to start for those capabilities. Keeping
//! this ownership outside Wi-Fi means a future Serial/Ethernet connector can
//! be attached without teaching the transport about HTTP, heartbeat, logs or
//! other application policy.

use embassy_executor::Spawner;
use embassy_net::Stack;
use esp_hal::peripherals::LPWR;
use log::warn;

use crate::storage::SharedStorage;

/// Owns application-level service lifecycle, independently from whichever
/// connector made a capability available.
pub struct ApplicationSupervisor {
    spawner: Spawner,
    lpwr: Option<LPWR<'static>>,
    tls: crate::tls::TlsReferenceStatic,
    agent_config: &'static crate::agent::AgentConfigSpace,
    app_config: &'static crate::app_config::AppConfigSpace,
    hardware_config: &'static crate::hardware::HardwareConfigSpace,
    tls_config: &'static crate::tls::TlsConfigSpace,
    runtime_config: &'static crate::runtime_config::RuntimeConfig,
    lifecycle_config: &'static crate::lifecycle::LifecycleConfigSpace,
    ota_config: &'static crate::ota::OtaConfigSpace,
    ip_services_started: bool,
}

impl ApplicationSupervisor {
    pub fn new(
        spawner: Spawner,
        lpwr: LPWR<'static>,
        tls: crate::tls::TlsReferenceStatic,
        agent_config: &'static crate::agent::AgentConfigSpace,
        app_config: &'static crate::app_config::AppConfigSpace,
        hardware_config: &'static crate::hardware::HardwareConfigSpace,
        tls_config: &'static crate::tls::TlsConfigSpace,
        runtime_config: &'static crate::runtime_config::RuntimeConfig,
        lifecycle_config: &'static crate::lifecycle::LifecycleConfigSpace,
        ota_config: &'static crate::ota::OtaConfigSpace,
    ) -> Self {
        Self {
            spawner,
            lpwr: Some(lpwr),
            tls,
            agent_config,
            app_config,
            hardware_config,
            tls_config,
            runtime_config,
            lifecycle_config,
            ota_config,
            ip_services_started: false,
        }
    }

    /// Announces that an IP-capable connector is ready.
    ///
    /// The current firmware has one IP connector (Wi-Fi), but the contract is
    /// deliberately capability-based: Ethernet can call the same method later,
    /// while a Serial connector can expose a different capability without
    /// pretending to own an `embassy_net::Stack`.
    pub fn on_ip_ready(
        &mut self,
        stack: Stack<'static>,
        storage: &'static SharedStorage,
    ) {
        if self.ip_services_started {
            log::info!("supervisor: IP services already started, ignoring duplicate readiness");
            return;
        }

        let Some(lpwr) = self.lpwr.take() else {
            warn!("supervisor: IP services requested without LPWR resource");
            return;
        };

        // Publish the one-shot state before spawning. Every task below is a
        // singleton application service; retrying only a subset after a spawn
        // failure would create a much less well-defined state than failing
        // visibly during development.
        self.ip_services_started = true;

        // Admin/config server. It internally selects provisioning UI or API.
        self.spawner
            .spawn(crate::http::run(stack, storage, self.agent_config, self.app_config, self.hardware_config, self.tls_config, self.runtime_config, self.lifecycle_config, self.ota_config, self.spawner, lpwr, self.tls).unwrap());

        // Services that require an IP stack. Heartbeat/log-stream remain
        // silent until their own application configuration is available.
        self.spawner.spawn(crate::time::sync_task(stack).unwrap());
        self.spawner
            .spawn(crate::heartbeat::run(stack, storage, self.agent_config, self.runtime_config, self.ota_config, self.tls_config, self.tls).unwrap());
        self.spawner
            .spawn(crate::log_stream::run(stack, self.agent_config, self.tls_config, self.tls).unwrap());
    }
}
