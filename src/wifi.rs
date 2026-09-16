//! Wi-Fi station: the radio is only brought up the first time it's needed.

use alloc::string::String;
use alloc::vec::Vec;

use embassy_executor::Spawner;
use embassy_net::{Runner, Stack, StackResources};
use esp_hal::peripherals::{LPWR, WIFI};
use esp_nvs::Key;
use esp_radio::wifi::{
    AuthenticationMethod, Config, Interface, WifiController, scan::ScanConfig, sta::StationConfig,
};
use log::{info, warn};
use static_cell::StaticCell;

use crate::storage::SharedStorage;

const NAMESPACE: Key = Key::from_str("wifi");
const KEY_SSID: Key = Key::from_str("ssid");
const KEY_PASSWORD: Key = Key::from_str("password");

/// An access point found by [`WifiManager::scan`].
pub struct Network {
    pub ssid: String,
    pub signal_strength: i8,
    pub secured: bool,
}

struct Radio {
    controller: WifiController<'static>,
    stack: Stack<'static>,
}

pub struct WifiManager {
    peripheral: Option<WIFI<'static>>,
    /// Handed to the HTTP config server the first time it's spawned (see
    /// `connect`), which needs it to reset the board via the RTC watchdog
    /// instead of `esp_hal::system::software_reset()` -- see
    /// `http::reboot_after_delay` for why that matters for native USB.
    lpwr: Option<LPWR<'static>>,
    spawner: Spawner,
    radio: Option<Radio>,
    /// Strongest BSSID seen per SSID in the last [`Self::scan`], so
    /// [`Self::connect`] can pin to that specific access point instead of
    /// letting the radio associate with any AP sharing the same SSID (common
    /// with mesh/multi-AP setups repeating one network per floor).
    strongest_bssid: Vec<(String, [u8; 6])>,
}

impl WifiManager {
    pub fn new(peripheral: WIFI<'static>, lpwr: LPWR<'static>, spawner: Spawner) -> Self {
        Self {
            peripheral: Some(peripheral),
            lpwr: Some(lpwr),
            spawner,
            radio: None,
            strongest_bssid: Vec::new(),
        }
    }

    /// Reconnects using previously saved credentials, if any. Meant to be
    /// called once at boot, before serving Improv, so a reboot (losing the
    /// RAM-only radio state) recovers on its own instead of sitting
    /// unprovisioned until the browser reconnects.
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
        self.connect(storage, &ssid, password).await
    }

    /// Connects with newly-provided credentials and, on success, saves them
    /// so [`Self::reconnect_saved`] can use them after a reboot.
    pub async fn provision(
        &mut self,
        storage: &'static SharedStorage,
        ssid: &str,
        password: String,
    ) -> bool {
        if self.connect(storage, ssid, password.clone()).await {
            let mut storage = storage.lock().await;
            storage.set_string(&NAMESPACE, &KEY_SSID, ssid);
            storage.set_string(&NAMESPACE, &KEY_PASSWORD, &password);
            true
        } else {
            false
        }
    }

    /// The device's current IPv4 address, if online.
    pub fn ip(&self) -> Option<embassy_net::Ipv4Address> {
        Some(self.radio.as_ref()?.stack.config_v4()?.address.address())
    }

    /// Starts the radio in station mode on first use, so that it can scan.
    /// Failures are logged and return `None` rather than panicking, which
    /// would halt the chip and take provisioning down with it.
    fn radio(&mut self) -> Option<&mut Radio> {
        if self.radio.is_none() {
            let (mut controller, interfaces) =
                match esp_radio::wifi::new(self.peripheral.take()?, Default::default()) {
                    Ok(parts) => parts,
                    Err(e) => {
                        warn!("Wi-Fi init failed: {e:?}");
                        return None;
                    }
                };
            if let Err(e) = controller.set_config(&Config::Station(StationConfig::default())) {
                warn!("Wi-Fi start failed: {e:?}");
                return None;
            }

            // DHCP (1) + HTTP server's TcpSocket (1) + SNTP's UdpSocket (1)
            // + a transient socket for `Stack::dns_query`/reqwless's DNS
            // lookups (1) + the heartbeat's TcpClient pool (1) + the log
            // stream's long-lived WS TcpConnect pool (1) -- 3 was enough
            // before SNTP, panicked ("adding a socket to a full SocketSet")
            // once it needed a 4th concurrently. +1 headroom for OTA next.
            static RESOURCES: StaticCell<StackResources<8>> = StaticCell::new();
            let seed = esp_hal::time::Instant::now().duration_since_epoch().as_micros() as u64;
            let (stack, runner) = embassy_net::new(
                interfaces.station,
                embassy_net::Config::dhcpv4(Default::default()),
                RESOURCES.init(StackResources::new()),
                seed,
            );
            self.spawner.spawn(net_task(runner).unwrap());

            self.radio = Some(Radio { controller, stack });
        }
        self.radio.as_mut()
    }

    /// Scans for networks, one entry per SSID (the strongest signal, when
    /// several access points share an SSID -- common with mesh/multi-AP
    /// setups repeating one network per floor). Empty if the radio is
    /// unavailable or the scan failed.
    pub async fn scan(&mut self) -> Vec<Network> {
        let Some(radio) = self.radio() else {
            return Vec::new();
        };
        let access_points = match radio
            .controller
            .scan_async(&ScanConfig::default().with_max(20))
            .await
        {
            Ok(access_points) => access_points,
            Err(e) => {
                warn!("Wi-Fi scan failed: {e:?}");
                return Vec::new();
            }
        };

        // One entry per SSID, keeping whichever access point has the best
        // signal; this also becomes `strongest_bssid`, so `connect` can pin
        // to that specific access point instead of letting the radio
        // associate with any AP sharing the same SSID.
        let mut strongest: Vec<(String, [u8; 6], i8, bool)> = Vec::new();
        for ap in &access_points {
            let ssid = ap.ssid.as_str();
            if ssid.is_empty() {
                continue;
            }
            let secured = !matches!(ap.auth_method, None | Some(AuthenticationMethod::None));
            match strongest.iter_mut().find(|(known_ssid, ..)| known_ssid == ssid) {
                Some((_, _, signal_strength, _)) if *signal_strength >= ap.signal_strength => {}
                Some(entry) => *entry = (String::from(ssid), ap.bssid, ap.signal_strength, secured),
                None => strongest.push((String::from(ssid), ap.bssid, ap.signal_strength, secured)),
            }
        }

        self.strongest_bssid = strongest
            .iter()
            .map(|(ssid, bssid, ..)| (ssid.clone(), *bssid))
            .collect();

        strongest
            .into_iter()
            .map(|(ssid, _, signal_strength, secured)| Network {
                ssid,
                signal_strength,
                secured,
            })
            .collect()
    }

    /// Connects and waits for DHCP. `false` if the radio is unavailable or
    /// the access point refused us. Pins to the strongest BSSID seen for
    /// this SSID in the last [`Self::scan`], if any. Doesn't persist
    /// credentials -- see [`Self::provision`] and [`Self::reconnect_saved`].
    /// Spawns the HTTP config server on first success.
    async fn connect(&mut self, storage: &'static SharedStorage, ssid: &str, password: String) -> bool {
        let bssid = self
            .strongest_bssid
            .iter()
            .find(|(known_ssid, _)| known_ssid == ssid)
            .map(|(_, bssid)| *bssid);
        if bssid.is_none() {
            warn!("Wi-Fi: no scan result for {ssid}, letting the radio pick an access point");
        }

        let Some(radio) = self.radio() else {
            return false;
        };
        let mut config = StationConfig::default()
            .with_ssid(ssid)
            .with_password(password);
        if let Some(bssid) = bssid {
            config = config.with_bssid(bssid);
        }
        let config = Config::Station(config);
        if radio.controller.set_config(&config).is_err()
            || radio.controller.connect_async().await.is_err()
        {
            warn!("Wi-Fi: connection to {ssid} failed");
            return false;
        }

        radio.stack.wait_config_up().await;
        info!("Wi-Fi connected, ip = {:?}", radio.stack.config_v4());
        let stack = radio.stack;

        if let Some(lpwr) = self.lpwr.take() {
            // contrat §4, `POST /app/port`: whatever was last saved (80 if
            // never touched) -- fetched here rather than threaded in from
            // main.rs, since `connect` already has `storage` in hand.
            let port = crate::agent::app_port(storage).await;
            self.spawner
                .spawn(crate::http::run(stack, storage, self.spawner, lpwr, port).unwrap());
            // SNTP (contrat §5): starts as soon as the network is up, same
            // one-shot guard as the HTTP server above.
            self.spawner.spawn(crate::time::sync_task(stack).unwrap());
            // Heartbeat (contrat §5): same guard, silent on its own until
            // ctrl_url is provisioned.
            self.spawner
                .spawn(crate::heartbeat::run(stack, storage).unwrap());
            // ESP_LOGx streaming (contrat §5): same guard.
            self.spawner
                .spawn(crate::log_stream::run(stack, storage).unwrap());
        }

        true
    }

    pub fn is_online(&self) -> bool {
        self.radio
            .as_ref()
            .is_some_and(|radio| radio.stack.config_v4().is_some())
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) -> ! {
    runner.run().await
}
