//! Raw TCP "telnet" log server: clients connect to the configured port and
//! receive a live stream of log records from the in-memory ring buffer.
//!
//! Lossy by design — if a client falls behind, records are dropped for that
//! client (see `LogBuffer::subscribe`). The logger itself never blocks.

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::thread;
use watercontroller_core::log_buffer;

pub fn spawn(port: u16) {
    thread::Builder::new()
        .name("telnet-log".into())
        .stack_size(6 * 1024)
        .spawn(move || {
            let bind = format!("0.0.0.0:{port}");
            let listener = match TcpListener::bind(&bind) {
                Ok(l) => l,
                Err(e) => {
                    log::error!("telnet log server failed to bind {bind}: {e}");
                    return;
                }
            };
            log::info!("telnet log server listening on {bind}");
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => {
                        thread::Builder::new()
                            .name("telnet-log-conn".into())
                            .stack_size(6 * 1024)
                            .spawn(move || handle_client(s))
                            .ok();
                    }
                    Err(e) => log::warn!("telnet accept: {e}"),
                }
            }
        })
        .ok();
}

fn handle_client(mut stream: TcpStream) {
    let _ = stream.set_nodelay(true);

    // Replay recent records first.
    if let Some(buf) = log_buffer::global() {
        for rec in buf.snapshot(200) {
            if writeln!(stream, "{}", rec.formatted()).is_err() {
                return;
            }
        }
    }

    // Then stream live records until the client disconnects.
    let Some(buf) = log_buffer::global() else {
        return;
    };
    let (id, rx) = buf.subscribe(256);
    while let Ok(rec) = rx.recv() {
        if writeln!(stream, "{}", rec.formatted()).is_err() {
            break;
        }
    }
    buf.unsubscribe(id);
}
