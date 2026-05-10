//! Brings up ESP-IDF Ethernet over QEMU's `open_eth` virtual NIC so the lwIP
//! stack has a usable netif. Without this, HTTPD listens but no packets reach
//! the guest — `hostfwd` on the QEMU side has nothing to deliver to.
//!
//! Pattern is from `rust-esp32-std-demo`:
//!   `EthDriver::new_openeth(peripherals.mac, sys_loop) -> EspEth -> BlockingEth -> start -> wait_netif_up`
//!
//! Returns a `BlockingEth` wrapping the driver. The caller must keep it alive
//! for the lifetime of the program — dropping it tears down the netif.

use anyhow::Result;
use esp_idf_svc::eth::{BlockingEth, EspEth, EthDriver};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::mac::MAC;

pub fn start(
    mac: MAC,
    sys_loop: EspSystemEventLoop,
) -> Result<BlockingEth<EspEth<'static, esp_idf_svc::eth::OpenEth>>> {
    let driver = EthDriver::new_openeth(mac, sys_loop.clone())?;
    let eth = EspEth::wrap(driver)?;
    let mut blocking = BlockingEth::wrap(eth, sys_loop)?;
    log::info!("qemu eth: starting open_eth driver");
    blocking.start()?;
    blocking.wait_netif_up()?;
    if let Ok(ip) = blocking.eth().netif().get_ip_info() {
        log::info!("qemu eth: netif up, ip={:?}", ip);
    }
    Ok(blocking)
}
