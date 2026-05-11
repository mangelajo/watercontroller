//! Line-based serial CLI for recovery + manual WiFi reconfig.
//!
//! Why this exists: when the device is in AP mode and the AP is unreachable
//! (wrong password baked in NVS, captive portal misbehaving, browser can't
//! get to 192.168.4.1, etc.) the only remaining channel is UART. This task
//! reads `\n`-terminated commands from stdin and dispatches them, echoing
//! the result back. It is intentionally simple: no readline, no history,
//! one command per line, ASCII-only.
//!
//! Commands (case-sensitive, whitespace-separated):
//!   help                        — list commands
//!   state                       — current wifi state + IP
//!   wifi list                   — saved networks (passwords redacted)
//!   wifi add <ssid> [password]  — append a network and rescan
//!   wifi del <ssid>             — remove a network and rescan
//!   wifi clear                  — wipe all networks (forces AP mode)
//!   wifi connect                — kick supervisor to retry
//!   wifi scan                   — discover nearby APs
//!   ap-info                     — current AP fallback SSID
//!   reset                       — soft reboot
//!   factory_reset               — wipe NVS config and reboot
//!
//! Output prefix `>>` so the human reading the pipe can distinguish CLI
//! responses from regular log lines.

use std::io::{BufRead, BufReader};
use std::sync::Arc;
use watercontroller_core::app::App;
use watercontroller_core::config::Config;
use watercontroller_core::traits::{NvsStore, Wifi, WifiCreds};

pub fn spawn(
    app: App,
    nvs: Arc<dyn NvsStore>,
    wifi: Arc<dyn Wifi>,
) {
    // 8 KiB. Read-only paths (`wifi list`, `state`) now read through
    // an `Arc<Config>` returned by `App::config()` — no full Config
    // clone, just a refcount bump. Mutation paths (`wifi add/del/
    // clear`) still do an explicit `(*app.config()).clone()` to get
    // an owned Config for the mutation, but those run on a flat call
    // chain (no nested format machinery), so they fit comfortably.
    crate::task_util::spawn_named(c"serial-cli", 8 * 1024, move || {
        run(app, nvs, wifi);
    });
}

fn run(app: App, nvs: Arc<dyn NvsStore>, wifi: Arc<dyn Wifi>) {
    println!(">> serial CLI ready — type `help`");
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // EOF on stdin — happens on some terminal disconnects.
                // Sleep and retry rather than spinning.
                std::thread::sleep(std::time::Duration::from_millis(500));
                continue;
            }
            Ok(_) => {}
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(500));
                continue;
            }
        }
        let cmd = line.trim();
        if cmd.is_empty() {
            continue;
        }
        dispatch(cmd, &app, &nvs, &wifi);
    }
}

fn dispatch(cmd: &str, app: &App, nvs: &Arc<dyn NvsStore>, wifi: &Arc<dyn Wifi>) {
    let mut it = cmd.split_whitespace();
    match it.next() {
        Some("help") => print_help(),
        Some("state") => {
            let s = wifi.state();
            println!(">> wifi state: {s:?}");
        }
        Some("wifi") => match it.next() {
            Some("list") => list_networks(app),
            Some("add") => {
                let ssid = it.next().unwrap_or("").to_string();
                if ssid.is_empty() {
                    println!(">> usage: wifi add <ssid> [password]");
                    return;
                }
                let password = it.next().unwrap_or("").to_string();
                modify_networks(app, nvs, wifi, |nets| {
                    nets.retain(|n: &WifiCreds| n.ssid != ssid);
                    nets.push(WifiCreds { ssid: ssid.clone(), password });
                    format!("added {ssid}")
                });
            }
            Some("del") => {
                let ssid = it.next().unwrap_or("").to_string();
                if ssid.is_empty() {
                    println!(">> usage: wifi del <ssid>");
                    return;
                }
                modify_networks(app, nvs, wifi, |nets| {
                    let before = nets.len();
                    nets.retain(|n: &WifiCreds| n.ssid != ssid);
                    if nets.len() == before {
                        format!("no such network: {ssid}")
                    } else {
                        format!("removed {ssid}")
                    }
                });
            }
            Some("clear") => {
                modify_networks(app, nvs, wifi, |nets| {
                    let n = nets.len();
                    nets.clear();
                    format!("cleared {n} network(s) — supervisor will enter AP mode")
                });
            }
            Some("connect") => {
                wifi.reconnect();
                println!(">> reconnect signaled");
            }
            Some("scan") => match wifi.scan() {
                Ok(list) => {
                    println!(">> {} network(s):", list.len());
                    let mut sorted = list;
                    sorted.sort_by(|a, b| b.rssi_dbm.cmp(&a.rssi_dbm));
                    for n in sorted {
                        println!(
                            ">>   {} rssi={} auth={} ch={}",
                            n.ssid, n.rssi_dbm, n.auth, n.channel
                        );
                    }
                }
                Err(e) => println!(">> scan failed: {e}"),
            },
            _ => println!(">> usage: wifi <list|add|del|clear|connect|scan>"),
        },
        Some("ap-info") => {
            let cfg = app.config();
            println!(">> ap_ssid: {} (password set: {})", cfg.wifi.ap_ssid, !cfg.wifi.ap_password.is_empty());
        }
        Some("reset") => {
            println!(">> rebooting...");
            std::thread::sleep(std::time::Duration::from_millis(200));
            unsafe { esp_idf_svc::sys::esp_restart() };
        }
        Some("factory_reset") => {
            println!(">> wiping NVS config + rebooting...");
            if let Err(e) = Config::factory_reset(&**nvs) {
                println!(">> factory_reset failed: {e}");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
            unsafe { esp_idf_svc::sys::esp_restart() };
        }
        Some(other) => println!(">> unknown command: {other} — type `help`"),
        None => {}
    }
}

fn print_help() {
    let help = "\
>> commands:\n\
>>   help                        list commands\n\
>>   state                       wifi state + IP\n\
>>   wifi list                   show saved networks\n\
>>   wifi add <ssid> [password]  add or replace a saved network\n\
>>   wifi del <ssid>             remove a saved network\n\
>>   wifi clear                  wipe networks (forces AP mode)\n\
>>   wifi connect                kick supervisor\n\
>>   wifi scan                   discover nearby APs\n\
>>   ap-info                     show AP fallback SSID\n\
>>   reset                       reboot\n\
>>   factory_reset               wipe NVS config + reboot";
    println!("{help}");
}

fn list_networks(app: &App) {
    let cfg = app.config();
    if cfg.wifi.networks.is_empty() {
        println!(">> no networks configured (device falls back to AP mode)");
        return;
    }
    for (i, n) in cfg.wifi.networks.iter().enumerate() {
        let pw = if n.password.is_empty() { "<open>" } else { "<set>" };
        println!(">>   [{i}] ssid={} password={pw}", n.ssid);
    }
}

/// Apply a mutation to the network list, persist, and signal the supervisor.
/// `f` is given a mutable view of the network list and returns a one-line
/// summary printed back to the user.
fn modify_networks<F>(
    app: &App,
    nvs: &Arc<dyn NvsStore>,
    wifi: &Arc<dyn Wifi>,
    f: F,
)
where
    F: FnOnce(&mut Vec<WifiCreds>) -> String,
{
    let mut cfg = (*app.config()).clone();
    let summary = f(&mut cfg.wifi.networks);
    if let Err(e) = cfg.save(&**nvs) {
        println!(">> nvs save failed: {e}");
        return;
    }
    let new_networks = cfg.wifi.networks.clone();
    app.replace_config(cfg);
    wifi.connect(&new_networks);
    println!(">> {summary}");
}
