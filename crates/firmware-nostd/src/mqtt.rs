//! MQTT client + Home Assistant discovery.
//!
//! One embassy task owns a `rust-mqtt` client over an embassy-net TCP
//! socket. On every (re)connect it publishes the HA discovery configs
//! and an `online` availability marker, subscribes to the per-switch
//! command topics, then loops: publish device state every 5 s and
//! route inbound command messages through `core::mqtt_dispatch`.
//!
//! Broker host/port/credentials come from `.env` via build.rs — never
//! hard-coded. Topic structure + discovery payloads + command parsing
//! all come from `watercontroller-core`, shared verbatim with the IDF
//! firmware.

use alloc::string::String;

use embassy_net::{tcp::TcpSocket, IpAddress, IpEndpoint, Stack};

/// Bridge embassy-net's `embedded-io-async 0.7` sockets to the
/// `0.6` traits rust-mqtt 0.3 expects. The read/write/flush async
/// signatures are identical across the two versions; only the error
/// type differs, so we collapse all transport errors to one opaque
/// `CompatErr` (rust-mqtt only needs "an error happened", not which).
mod eio_compat {
    use embedded_io_async as eio07;
    use embedded_io_async_06 as eio06;

    pub struct Compat<T>(pub T);

    #[derive(Debug)]
    pub struct CompatErr;
    impl eio06::Error for CompatErr {
        fn kind(&self) -> eio06::ErrorKind {
            eio06::ErrorKind::Other
        }
    }
    impl<T> eio06::ErrorType for Compat<T> {
        type Error = CompatErr;
    }
    impl<T: eio07::Read> eio06::Read for Compat<T> {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, CompatErr> {
            self.0.read(buf).await.map_err(|_| CompatErr)
        }
    }
    impl<T: eio07::Write> eio06::Write for Compat<T> {
        async fn write(&mut self, buf: &[u8]) -> Result<usize, CompatErr> {
            self.0.write(buf).await.map_err(|_| CompatErr)
        }
        async fn flush(&mut self) -> Result<(), CompatErr> {
            self.0.flush().await.map_err(|_| CompatErr)
        }
    }
}
use eio_compat::Compat;
use embassy_time::{Duration, Timer};
use embassy_futures::select::{select, Either};
use esp_println::println;
use rust_mqtt::{
    client::{client::MqttClient, client_config::ClientConfig},
    packet::v5::{publish_packet::QualityOfService, reason_codes::ReasonCode},
    utils::rng_generator::CountingRng,
};
use watercontroller_core::{
    app::App,
    ha_discovery::{all_messages, DeviceContext},
    mqtt_dispatch::parse_command,
    state::WaterControlState,
};

const MQTT_HOST: &str = env!("MQTT_HOST");
const MQTT_PORT: &str = env!("MQTT_PORT");
const MQTT_USER: &str = env!("MQTT_USER");
const MQTT_PASS: &str = env!("MQTT_PASS");
const FW_VERSION: &str = "wc-nostd";

/// Parse a dotted IPv4 string into an `IpAddress`. Panics on a
/// malformed `.env` — a deploy-time error, fine to fail loud.
fn parse_host(s: &str) -> IpAddress {
    let mut octets = [0u8; 4];
    let mut i = 0;
    for part in s.split('.') {
        octets[i] = part.parse().expect("MQTT_HOST octet");
        i += 1;
    }
    assert_eq!(i, 4, "MQTT_HOST must be dotted IPv4");
    IpAddress::v4(octets[0], octets[1], octets[2], octets[3])
}

#[embassy_executor::task]
pub async fn mqtt_task(app: App, stack: Stack<'static>) {
    let endpoint = IpEndpoint::new(parse_host(MQTT_HOST), MQTT_PORT.parse().expect("MQTT_PORT"));

    loop {
        if let Err(e) = run_session(&app, stack, endpoint).await {
            println!("mqtt: session ended ({:?}), reconnecting in 5 s", e);
        }
        Timer::after(Duration::from_secs(5)).await;
    }
}

/// One connect→serve→error cycle. Returns Err on any MQTT/socket error
/// so the supervisor reconnects.
async fn run_session(
    app: &App,
    stack: Stack<'static>,
    endpoint: IpEndpoint,
) -> Result<(), ReasonCode> {
    let mut rx = [0u8; 1536];
    let mut tx = [0u8; 1536];
    let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
    socket.set_timeout(Some(Duration::from_secs(30)));

    println!("mqtt: connecting to {}", MQTT_HOST);
    socket
        .connect(endpoint)
        .await
        .map_err(|_| ReasonCode::NetworkError)?;

    // rust-mqtt buffers — sized for the largest HA discovery payload
    // (a sensor config is ~300-400 B; 4 KiB is comfortable).
    let mut write_buf = [0u8; 4096];
    let mut recv_buf = [0u8; 4096];
    let mut config = ClientConfig::new(
        rust_mqtt::client::client_config::MqttVersion::MQTTv5,
        CountingRng(0x5eed_1234),
    );
    config.add_max_subscribe_qos(QualityOfService::QoS1);
    config.add_client_id("doremorwater-nostd");
    config.add_username(MQTT_USER);
    config.add_password(MQTT_PASS);
    config.max_packet_size = 4096;

    let mut client = MqttClient::<_, 5, _>::new(
        Compat(socket),
        &mut write_buf,
        4096,
        &mut recv_buf,
        4096,
        config,
    );

    client.connect_to_broker().await?;
    println!("mqtt: connected to broker");

    // ----- on-connect: HA discovery + availability + subscribe ------
    let cfg = app.config();
    let ctx = DeviceContext {
        base_topic: &cfg.mqtt.base_topic,
        discovery_prefix: &cfg.mqtt.ha_discovery_prefix,
        device_id: &cfg.wifi.hostname,
        friendly_name: "Doremorwater",
        sw_version: FW_VERSION,
        manufacturer: "homebrew",
        model: "watercontroller",
    };

    for msg in all_messages(&ctx) {
        client
            .send_message(&msg.topic, &msg.payload, QualityOfService::QoS1, true)
            .await?;
    }
    let availability = {
        let mut s = String::from(cfg.mqtt.base_topic.as_str());
        s.push_str("/status");
        s
    };
    client
        .send_message(&availability, b"online", QualityOfService::QoS1, true)
        .await?;

    for key in ["sprinkler_1", "sprinkler_2", "water_control"] {
        client.subscribe_to_topic(&ctx.switch_command_topic(key)).await?;
    }
    println!("mqtt: discovery published, subscribed to command topics");

    // ----- serve loop: publish state every 5 s, route commands ------
    loop {
        match select(
            Timer::after(Duration::from_secs(5)),
            client.receive_message(),
        )
        .await
        {
            Either::First(()) => {
                publish_state(&mut client, &ctx, app).await?;
            }
            Either::Second(msg) => {
                let (topic, payload) = msg?;
                if let Some(cmd) = parse_command(&cfg.mqtt.base_topic, topic, payload) {
                    let _ = app.switch_command(cmd);
                }
            }
        }
    }
}

/// Publish the per-entity state topics. Mirrors
/// `core::mqtt_dispatch::MqttIntegration::publish_state` — kept inline
/// because that method routes through the sync `Mqtt` trait which we
/// don't bridge in the no_std build.
async fn publish_state(
    client: &mut MqttClient<'_, Compat<TcpSocket<'_>>, 5, CountingRng>,
    ctx: &DeviceContext<'_>,
    app: &App,
) -> Result<(), ReasonCode> {
    use core::fmt::Write as _;
    let snap = app.snapshot();

    // uptime
    {
        let mut s: heapless::String<16> = heapless::String::new();
        let _ = write!(s, "{}", snap.uptime_ms / 1000);
        client
            .send_message(&ctx.sensor_state_topic("uptime"), s.as_bytes(), QualityOfService::QoS1, true)
            .await?;
    }
    // switches
    let sw = |on: bool| -> &'static [u8] { if on { b"ON" } else { b"OFF" } };
    client
        .send_message(&ctx.switch_state_topic("sprinkler_1"), sw(snap.switches.sprinkler_1), QualityOfService::QoS1, true)
        .await?;
    client
        .send_message(&ctx.switch_state_topic("sprinkler_2"), sw(snap.switches.sprinkler_2), QualityOfService::QoS1, true)
        .await?;
    let wc_on = matches!(
        snap.switches.water_control,
        WaterControlState::On | WaterControlState::Transitioning
    );
    client
        .send_message(&ctx.switch_state_topic("water_control"), sw(wc_on), QualityOfService::QoS1, true)
        .await?;
    // flow alarm
    client
        .send_message(&ctx.sensor_state_topic("flow_alarm"), sw(snap.alarm.active), QualityOfService::QoS1, true)
        .await?;
    Ok(())
}
