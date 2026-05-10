//! Embedded SPA assets — bundled into the firmware binary at compile time.
//! When the SPA grows, consider moving the bytes to a SPIFFS partition (the
//! `spiffs` partition in `partitions.csv` is reserved for that purpose).

pub const INDEX_HTML: &[u8] = include_bytes!("../assets/index.html");
