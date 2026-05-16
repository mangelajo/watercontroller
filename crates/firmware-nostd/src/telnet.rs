//! Raw-TCP log streaming on port 23.
//!
//! Mirrors the IDF firmware's `log_telnet` port: any client that opens a
//! TCP connection gets a live feed of every `log::*!` line. It shares
//! the `logbuf` `PubSubChannel` with `/ws/logs`, so both can stream at
//! once. One client at a time — the post-incident healthcheck only ever
//! opens a single short-lived connection.

use embassy_net::{tcp::TcpSocket, Stack};
use embassy_time::Duration;
use embedded_io_async::Write as _;

const PORT: u16 = 23;

#[embassy_executor::task]
pub async fn telnet_task(stack: Stack<'static>) {
    let mut rx_buf = [0u8; 256];
    let mut tx_buf = [0u8; 1024];
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_timeout(Some(Duration::from_secs(30)));
        if socket.accept(PORT).await.is_err() {
            continue;
        }
        log::info!("telnet: client connected");
        // A fresh subscriber per connection; dropped at loop end so the
        // slot is returned even if the client never sends anything.
        let mut sub = match crate::logbuf::subscriber() {
            Some(s) => s,
            None => {
                let _ = socket.write_all(b"log subscriber slots full\r\n").await;
                socket.close();
                continue;
            }
        };
        let _ = socket.write_all(b"wc-nostd log stream\r\n").await;
        loop {
            let line = sub.next_message_pure().await;
            if socket.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if socket.write_all(b"\r\n").await.is_err() {
                break;
            }
        }
        log::info!("telnet: client disconnected");
        socket.abort();
        let _ = socket.flush().await;
    }
}
