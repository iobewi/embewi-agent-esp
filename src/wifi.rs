//! Wi-Fi station: the radio is only brought up the first time it's needed.

use alloc::string::String;
use alloc::vec::Vec;

use embassy_executor::Spawner;
use embassy_net::{Runner, Stack, StackResources};
use esp_hal::peripherals::WIFI;
use esp_radio::wifi::{
    AuthenticationMethod, Config, Interface, WifiController, scan::ScanConfig, sta::StationConfig,
};
use log::{info, warn};
use static_cell::StaticCell;

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
    spawner: Spawner,
    radio: Option<Radio>,
}

impl WifiManager {
    pub fn new(peripheral: WIFI<'static>, spawner: Spawner) -> Self {
        Self {
            peripheral: Some(peripheral),
            spawner,
            radio: None,
        }
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

            static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
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

    /// Scans for networks, one entry per SSID. Empty if the radio is
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

        let mut networks: Vec<Network> = Vec::new();
        for ap in access_points {
            let ssid = ap.ssid.as_str();
            if ssid.is_empty() || networks.iter().any(|n| n.ssid == ssid) {
                continue;
            }
            networks.push(Network {
                ssid: String::from(ssid),
                signal_strength: ap.signal_strength,
                secured: !matches!(ap.auth_method, None | Some(AuthenticationMethod::None)),
            });
        }
        networks
    }

    /// Connects and waits for DHCP. `false` if the radio is unavailable or
    /// the access point refused us.
    pub async fn connect(&mut self, ssid: &str, password: String) -> bool {
        let Some(radio) = self.radio() else {
            return false;
        };
        let config = Config::Station(
            StationConfig::default()
                .with_ssid(ssid)
                .with_password(password),
        );
        if radio.controller.set_config(&config).is_err()
            || radio.controller.connect_async().await.is_err()
        {
            warn!("Wi-Fi: connection to {ssid} failed");
            return false;
        }

        radio.stack.wait_config_up().await;
        info!("Wi-Fi connected, ip = {:?}", radio.stack.config_v4());
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
