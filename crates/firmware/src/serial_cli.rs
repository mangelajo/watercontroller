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
//!   log <level>                 — set log verbosity (off/error/warn/info/debug/trace)
//!   tasks                       — tabulated task list (name, state, priority, stack free)
//!   mem                         — heap statistics (free, allocated, largest block, min-ever)
//!   alarm status                — show flow alarm config + latched state
//!   alarm clear                 — reset the latched flow alarm
//!   reset                       — soft reboot
//!   factory_reset               — wipe NVS config and reboot
//!
//! Output prefix `>>` so the human reading the pipe can distinguish CLI
//! responses from regular log lines.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;
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
    // Observed peak ~3.5 KiB (dispatch + format helpers + line
    // buffer). 8 KiB gives ~4 KiB headroom for the worst command
    // path (`tasks`, `webhook fire`, `wifi list`). Previous 20 KiB
    // was overkill — the earlier crashes were the task_util bug
    // capping everything at ~10 KiB regardless of request.
    crate::task_util::spawn_named(c"serial-cli", 8 * 1024, move || {
        run(app, nvs, wifi);
    });
}

fn run(app: App, nvs: Arc<dyn NvsStore>, wifi: Arc<dyn Wifi>) {
    println!(">> serial CLI ready — type `help`");
    // ESP-IDF's UART stdio has no line discipline: `read_line` returns
    // whatever bytes the VFS driver flushed since the last poll, with no
    // guarantee a `\n` is included. Slow typing means each keystroke
    // returns alone and `read_line` treats every byte as a complete
    // (empty-suffixed) line, dispatching `s`, `t`, `a`, … as separate
    // commands. And the driver doesn't echo by default — the user
    // can't see what they typed.
    //
    // So we do our own line discipline: read 1 byte at a time, echo it,
    // accumulate into a `String`, dispatch on CR/LF. Backspace deletes
    // the last char and emits a destructive sequence to the terminal.
    let mut buf = String::with_capacity(128);
    let mut byte = [0u8; 1];
    let mut stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "> ");
    let _ = stdout.flush();
    loop {
        match stdin.read(&mut byte) {
            Ok(1) => {}
            Ok(_) | Err(_) => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        }
        match byte[0] {
            b'\r' | b'\n' => {
                let _ = writeln!(stdout);
                let _ = stdout.flush();
                if !buf.is_empty() {
                    dispatch(buf.trim(), &app, &nvs, &wifi);
                    buf.clear();
                }
                let _ = write!(stdout, "> ");
                let _ = stdout.flush();
            }
            0x08 | 0x7f => {
                // backspace / DEL — destructive: erase last char on
                // the user's terminal too.
                if buf.pop().is_some() {
                    let _ = stdout.write_all(b"\x08 \x08");
                    let _ = stdout.flush();
                }
            }
            c if (0x20..=0x7e).contains(&c) => {
                buf.push(c as char);
                // Echo printable. Most terminals expect this.
                let _ = stdout.write_all(&[c]);
                let _ = stdout.flush();
            }
            // Ignore other control bytes (tab, escape sequences, etc.).
            _ => {}
        }
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
        Some("log") => {
            match it.next() {
                Some(level) => set_log_level(level),
                None => println!(">> usage: log <off|error|warn|info|debug|trace>"),
            }
        }
        Some("tasks") => print_tasks(),
        Some("mem") => print_mem(),
        Some("alarm") => match it.next() {
            Some("status") => print_alarm_status(app),
            Some("clear") => {
                app.clear_flow_alarm();
                println!(">> alarm cleared");
            }
            _ => println!(">> usage: alarm <status|clear>"),
        },
        Some("webhook") => match it.next() {
            Some("list") => print_webhook_list(app),
            Some("fire") => match it.next() {
                Some(kind_str) => fire_webhook(app, kind_str),
                None => println!(">> usage: webhook fire <event_kind>"),
            },
            _ => println!(">> usage: webhook <list|fire <event_kind>>"),
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
>>   log <level>                 set log verbosity (off/error/warn/info/debug/trace)\n\
>>   tasks                       tabulated task list (name/state/pri/stack free)\n\
>>   mem                         heap stats (free/allocated/largest/min-ever)\n\
>>   alarm status                show flow alarm config + latched state\n\
>>   alarm clear                 reset the latched flow alarm\n\
>>   webhook list                show configured webhooks\n\
>>   webhook fire <event>        emit a webhook event (e.g. flow_alarm.fire)\n\
>>   reset                       reboot\n\
>>   factory_reset               wipe NVS config + reboot";
    println!("{help}");
}

/// Set both the Rust-side `log` crate max level and the ESP-IDF
/// C-side log level (wildcard tag). The C-side is what's flooding
/// the console at runtime — wifi events, https handshakes, heartbeat
/// `alive` lines — so without flipping the C side too, `log off` only
/// silences our own Rust modules and leaves the noise.
fn set_log_level(level_str: &str) {
    use esp_idf_svc::sys as sys;
    let (rust_level, c_level) = match level_str.to_ascii_lowercase().as_str() {
        "off" | "none" => (log::LevelFilter::Off, sys::esp_log_level_t_ESP_LOG_NONE),
        "error"        => (log::LevelFilter::Error, sys::esp_log_level_t_ESP_LOG_ERROR),
        "warn" | "warning" => (log::LevelFilter::Warn, sys::esp_log_level_t_ESP_LOG_WARN),
        "info"         => (log::LevelFilter::Info, sys::esp_log_level_t_ESP_LOG_INFO),
        "debug"        => (log::LevelFilter::Debug, sys::esp_log_level_t_ESP_LOG_DEBUG),
        "trace" | "verbose" => (log::LevelFilter::Trace, sys::esp_log_level_t_ESP_LOG_VERBOSE),
        _ => {
            println!(">> usage: log <off|error|warn|info|debug|trace>");
            return;
        }
    };
    log::set_max_level(rust_level);
    unsafe {
        // "*" applies to every tag the IDF logger emits.
        let wildcard = c"*".as_ptr() as *const _;
        sys::esp_log_level_set(wildcard, c_level);
    }
    println!(">> log level set to {level_str}");
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

/// Flow alarm status — config + latched state. Each `println!` is
/// kept to a single dynamic argument because `core::fmt` grows the
/// stack by ~700 B per `{}` placeholder, and serial-cli's 8 KiB task
/// stack has only a few hundred bytes of headroom. The previous
/// 5-arg implementation panicked on real hardware (LoadProhibited
/// after the line printed — likely a stack-overflow corruption that
/// surfaced one tick later in the wifi-probe path).
#[inline(never)]
#[inline(never)]
fn print_webhook_list(app: &App) {
    let cfg = app.config();
    if cfg.webhooks.is_empty() {
        println!(">> no webhooks configured");
        return;
    }
    for (i, wh) in cfg.webhooks.iter().enumerate() {
        let on = if wh.enabled { "on" } else { "off" };
        let kind = match wh.kind {
            watercontroller_core::webhook::WebhookKind::Generic => "generic",
            watercontroller_core::webhook::WebhookKind::Slack => "slack",
            watercontroller_core::webhook::WebhookKind::Discord => "discord",
            watercontroller_core::webhook::WebhookKind::HomeAssistant => "ha",
        };
        let n_events = wh.events.len();
        println!(">> [{i}] {on} kind={kind} events={n_events}");
        let url = wh.url.as_str();
        println!(">>     url: {url}");
    }
}

#[inline(never)]
fn fire_webhook(app: &App, kind_str: &str) {
    // Reuse serde to parse the dotted form. Accept input both quoted
    // and unquoted by stuffing quotes around the bare token.
    let quoted = format!("\"{kind_str}\"");
    match serde_json::from_str::<watercontroller_core::webhook::EventKind>(&quoted) {
        Ok(kind) => {
            app.emit_event(watercontroller_core::webhook::WebhookEvent::new(kind));
            println!(">> webhook event {kind_str} emitted");
        }
        Err(_) => {
            println!(">> unknown event kind: {kind_str}");
            println!(">> known kinds:");
            for k in watercontroller_core::webhook::EventKind::all() {
                let s = k.as_str();
                println!(">>   {s}");
            }
        }
    }
}

fn print_alarm_status(app: &App) {
    let cfg = app.config();
    let snap = app.snapshot();
    let active = if snap.alarm.active { "ACTIVE" } else { "idle" };
    println!(">> flow alarm: {active}");
    let enabled = cfg.flow_alarm.enabled;
    println!(">>   enabled       : {enabled}");
    let threshold = cfg.flow_alarm.threshold_lph as i32;
    println!(">>   threshold     : {threshold} L/h");
    let duration = cfg.flow_alarm.duration_secs;
    println!(">>   duration      : {duration} s");
    let elapsed = snap.alarm.elapsed_secs;
    println!(">>   elapsed       : {elapsed} s");
}

/// Tabulated task list — same data as `/api/diag`. Columns sized for a
/// typical 80-col terminal; long task names are truncated to keep the
/// alignment. `#[inline(never)]` so the snapshot's Vec + per-row format
/// frame doesn't bloat the dispatch parent.
#[inline(never)]
#[inline(never)]
fn fmt_task_header() -> String {
    format!(
        "{:<16} {:<10} {:>3} {:>10} {:>12}",
        "NAME", "STATE", "PRI", "STACK_FREE", "RUNTIME"
    )
}

#[inline(never)]
fn fmt_task_row(t: &crate::diag::TaskInfo) -> String {
    let name: String = t.name.chars().take(16).collect();
    format!(
        "{:<16} {:<10} {:>3} {:>10} {:>12}",
        name, t.state, t.priority, t.stack_min_free_bytes, t.run_time
    )
}

fn print_tasks() {
    let snap = crate::diag::snapshot();
    // Each row is formatted in an #[inline(never)] helper so the 5-arg
    // `format!` machinery (~3.5 KB transient frame on this task) lives
    // only while the helper is on the stack, not as part of this
    // function's permanent locals or the caller's. The println! then
    // takes a single &String argument, which the format machinery
    // collapses to one ~700 B `&dyn Display` frame. See CLAUDE.md.
    let header = fmt_task_header();
    println!(">> {header}");
    let sep = "-".repeat(56);
    println!(">> {sep}");
    let mut rows = snap.tasks;
    rows.sort_by_key(|t| t.stack_min_free_bytes);
    for t in rows.iter() {
        let line = fmt_task_row(t);
        println!(">> {line}");
    }
}

/// Heap statistics — same data as `/api/diag`. Numbers are aligned right
/// with thousands separators for readability.
#[inline(never)]
fn print_mem() {
    let snap = crate::diag::snapshot();
    let h = &snap.heap;
    println!(">> heap:");
    println!(">>   total free        : {:>12} B", with_commas(h.total_free_bytes));
    println!(">>   total allocated   : {:>12} B", with_commas(h.total_allocated_bytes));
    println!(">>   largest free block: {:>12} B", with_commas(h.largest_free_block));
    println!(">>   min-ever free     : {:>12} B", with_commas(h.min_ever_free_bytes));
}

/// Tiny stack-only thousands separator. Avoids pulling in heavyweight
/// `format!` paths on the serial-cli task: builds a `String` from the
/// digits in reverse, inserting `,` every 3.
fn with_commas(mut n: usize) -> String {
    if n == 0 {
        return "0".into();
    }
    let mut digits = String::with_capacity(20);
    let mut i = 0;
    while n > 0 {
        if i > 0 && i % 3 == 0 {
            digits.push(',');
        }
        digits.push((b'0' + (n % 10) as u8) as char);
        n /= 10;
        i += 1;
    }
    digits.chars().rev().collect()
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
