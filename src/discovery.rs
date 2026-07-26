//! mDNS/zeroconf: advertise this daemon + browse LAN peers.
//! Browsers can't do mDNS, so the daemon discovers peers and reports them to the phone.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::Serialize;

const TY: &str = "_remoat._tcp.local.";

pub fn platform_label() -> String {
    if cfg!(target_os = "macos") { "mac" } else if cfg!(target_os = "linux") { "linux" } else { "other" }.into()
}

#[derive(Clone, Serialize)]
pub struct Device {
    pub name: String,
    pub platform: String,
    pub host: String,
    pub port: u16,
    #[serde(rename = "self")]
    pub is_self: bool,
}

pub struct Discovery {
    peers: Arc<Mutex<HashMap<String, Device>>>,
    _daemon: Option<ServiceDaemon>,
}

impl Discovery {
    /// mDNS is best-effort — never let discovery take down the daemon. Self is always in
    /// the list so the app works without it.
    pub fn start(name: String, ip: String, port: u16) -> Arc<Discovery> {
        let peers = Arc::new(Mutex::new(HashMap::new()));
        peers.lock().unwrap().insert(
            name.clone(),
            Device { name: name.clone(), platform: platform_label(), host: ip.clone(), port, is_self: true },
        );

        let daemon = match ServiceDaemon::new() {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("[mdns] disabled ({e}) — self-only device list");
                None
            }
        };

        if let Some(d) = &daemon {
            let host_name = format!("{name}.local.");
            let mut props = HashMap::new();
            props.insert("platform".to_string(), platform_label());
            props.insert("name".to_string(), name.clone());
            match ServiceInfo::new(TY, &name, &host_name, ip.as_str(), port, props) {
                Ok(info) => { let _ = d.register(info); }
                Err(e) => eprintln!("[mdns] service info error: {e}"),
            }

            if let Ok(rx) = d.browse(TY) {
                let peers2 = peers.clone();
                let self_name = name.clone();
                std::thread::spawn(move || {
                    while let Ok(ev) = rx.recv() {
                        match ev {
                            ServiceEvent::ServiceResolved(info) => {
                                let short = info.get_fullname().replace(&format!(".{TY}"), "");
                                let host = info.get_addresses().iter().next().map(|a| a.to_string()).unwrap_or_default();
                                let props = info.get_properties();
                                let plat = props.get_property_val_str("platform").unwrap_or("?").to_string();
                                let dn = props.get_property_val_str("name").unwrap_or(&short).to_string();
                                let is_self = short == self_name;
                                peers2.lock().unwrap().insert(
                                    short.clone(),
                                    Device { name: dn, platform: plat, host, port: info.get_port(), is_self },
                                );
                            }
                            ServiceEvent::ServiceRemoved(_ty, full) => {
                                let short = full.replace(&format!(".{TY}"), "");
                                if short != self_name {
                                    peers2.lock().unwrap().remove(&short);
                                }
                            }
                            _ => {}
                        }
                    }
                });
            }
            eprintln!("[mdns] advertising {name} at {ip}:{port}");
        }

        Arc::new(Discovery { peers, _daemon: daemon })
    }

    pub fn devices(&self) -> Vec<Device> {
        self.peers.lock().unwrap().values().cloned().collect()
    }
}
