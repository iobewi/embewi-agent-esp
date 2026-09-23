//! Embewi Wi-Fi connector policy.
//!
//! Reusable radio/network mechanics live in `esp-wifi-manager`. This module
//! retains only application policy: persisted SSID/password and the socket-set
//! size required by the currently enabled Embewi IP services.

use alloc::string::String;
use alloc::vec::Vec;

use embassy_executor::Spawner;
use embassy_net::{Stack, StackResources};
use esp_hal::peripherals::WIFI;
use esp_storage_manager::Key;
use log::{info, warn};
use static_cell::StaticCell;

use crate::storage::SharedStorage;

pub use esp_wifi_manager::Network;

const NAMESPACE: Key = Key::from_str("wifi");
const KEY_SSID: Key = Key::from_str("ssid");
const KEY_PASSWORD: Key = Key::from_str("password");

// DHCP (1) + admin HTTP/HTTPS (1) + SNTP UDP (1) + transient outbound DNS
// (1) + heartbeat TCP (1) + log-stream WS/TCP (1), with headroom for OTA.
// This sizing is Embewi application policy, so it intentionally stays out of
// `esp-wifi-manager`.
const SOCKETS: usize = 8;
static RESOURCES: StaticCell<StackResources<SOCKETS>> = StaticCell::new();

pub struct WifiManager {
    transport: esp_wifi_manager::WifiManager<SOCKETS>,
}

impl WifiManager {
    pub fn new(peripheral: WIFI<'static>, spawner: Spawner) -> Self {
        Self {
            transport: esp_wifi_manager::WifiManager::new(
                peripheral,
                spawner,
                RESOURCES.init(StackResources::new()),
            ),
        }
    }

    /// Reconnects using credentials persisted by this application.
    pub async fn reconnect_saved(&mut self, storage: &'static SharedStorage) -> bool {
        let (ssid, password) = {
            let mut storage = storage.lock().await;
            let Some(ssid) = storage.get_string(&NAMESPACE, &KEY_SSID) else {
                return false;
            };
            let Some(password) = storage.get_string(&NAMESPACE, &KEY_PASSWORD) else {
                return false;
            };
            (ssid, password)
        };

        info!("Wi-Fi: reconnecting to saved SSID={ssid}");
        self.transport.connect(&ssid, password).await
    }

    /// Connects with newly provided credentials and persists them only after
    /// association + DHCP succeeded.
    pub async fn provision(
        &mut self,
        storage: &'static SharedStorage,
        ssid: &str,
        password: String,
    ) -> bool {
        if !self.transport.connect(ssid, password.clone()).await {
            return false;
        }

        let mut storage = storage.lock().await;
        if storage.set_string(&NAMESPACE, &KEY_SSID, ssid).is_err()
            || storage.set_string(&NAMESPACE, &KEY_PASSWORD, &password).is_err()
        {
            warn!("Wi-Fi: connected, but credentials could not be saved to NVS");
        }
        true
    }

    pub async fn scan(&mut self) -> Vec<Network> {
        self.transport.scan().await
    }

    pub fn ip(&self) -> Option<embassy_net::Ipv4Address> {
        self.transport.ip()
    }

    pub fn ip_stack(&self) -> Option<Stack<'static>> {
        self.transport.ip_stack()
    }

    pub fn is_online(&self) -> bool {
        self.transport.is_online()
    }
}
