//! On-device self-signed certificate generation for HTTPS.
//!
//! Generates an ECDSA P-256 keypair + self-signed X.509 cert via mbedTLS
//! FFI, returning PEM blobs ready to drop into `Config.https.cert_pem` /
//! `key_pem`. Used to provision a default cert at first boot when the user
//! hasn't pasted their own; the result is persisted to NVS by the caller so
//! the cert (and its public key fingerprint) stays stable across reboots.
//!
//! ECDSA P-256 chosen over RSA-2048 because keygen on Xtensa is ~20× faster
//! and the resulting keys/certs are much smaller, which matters for an NVS
//! blob that ships through the UI's JSON config.

use anyhow::{anyhow, Context, Result};
use esp_idf_svc::sys::*;
use std::ffi::CString;
use std::mem::MaybeUninit;

/// Bundled mbedTLS contexts. Drop frees them in reverse init order, so any
/// early return from the generator path cleans up correctly.
struct CertGenCtx {
    entropy: mbedtls_entropy_context,
    ctr_drbg: mbedtls_ctr_drbg_context,
    key: mbedtls_pk_context,
    crt: mbedtls_x509write_cert,
}

impl CertGenCtx {
    fn new() -> Self {
        unsafe {
            let mut ctx = MaybeUninit::<Self>::zeroed();
            let p = ctx.as_mut_ptr();
            mbedtls_entropy_init(&mut (*p).entropy);
            mbedtls_ctr_drbg_init(&mut (*p).ctr_drbg);
            mbedtls_pk_init(&mut (*p).key);
            mbedtls_x509write_crt_init(&mut (*p).crt);
            ctx.assume_init()
        }
    }
}

impl Drop for CertGenCtx {
    fn drop(&mut self) {
        unsafe {
            mbedtls_x509write_crt_free(&mut self.crt);
            mbedtls_pk_free(&mut self.key);
            mbedtls_ctr_drbg_free(&mut self.ctr_drbg);
            mbedtls_entropy_free(&mut self.entropy);
        }
    }
}

/// Generate an ECDSA P-256 self-signed cert with `CN=<common_name>` and
/// return `(cert_pem, key_pem)`.
pub fn generate_self_signed(common_name: &str) -> Result<(String, String)> {
    const PERS: &[u8] = b"watercontroller-tls-certgen";

    let mut ctx = CertGenCtx::new();

    unsafe {
        check(
            mbedtls_ctr_drbg_seed(
                &mut ctx.ctr_drbg,
                Some(mbedtls_entropy_func),
                &mut ctx.entropy as *mut _ as *mut _,
                PERS.as_ptr(),
                PERS.len(),
            ),
            "ctr_drbg_seed",
        )?;

        let pk_info = mbedtls_pk_info_from_type(mbedtls_pk_type_t_MBEDTLS_PK_ECKEY);
        if pk_info.is_null() {
            return Err(anyhow!("mbedtls_pk_info_from_type returned null"));
        }
        check(mbedtls_pk_setup(&mut ctx.key, pk_info), "pk_setup")?;

        // After pk_setup with PK_ECKEY, pk_ctx is an mbedtls_ecp_keypair*.
        let ec_keypair = ctx.key.private_pk_ctx as *mut mbedtls_ecp_keypair;
        check(
            mbedtls_ecp_gen_key(
                mbedtls_ecp_group_id_MBEDTLS_ECP_DP_SECP256R1,
                ec_keypair,
                Some(mbedtls_ctr_drbg_random),
                &mut ctx.ctr_drbg as *mut _ as *mut _,
            ),
            "ecp_gen_key",
        )?;

        // Self-signed → subject == issuer. CN is enough for browsers; we
        // skip SAN since (a) the device's IP isn't known at boot and (b)
        // browsers warn anyway on a self-signed cert, so spending bytes on
        // hostnames doesn't get us a clean lock icon.
        let dn = CString::new(format!("CN={common_name}")).context("CN: nul byte")?;
        check(
            mbedtls_x509write_crt_set_subject_name(&mut ctx.crt, dn.as_ptr()),
            "set_subject_name",
        )?;
        check(
            mbedtls_x509write_crt_set_issuer_name(&mut ctx.crt, dn.as_ptr()),
            "set_issuer_name",
        )?;
        mbedtls_x509write_crt_set_subject_key(&mut ctx.crt, &mut ctx.key);
        mbedtls_x509write_crt_set_issuer_key(&mut ctx.crt, &mut ctx.key);
        mbedtls_x509write_crt_set_md_alg(&mut ctx.crt, mbedtls_md_type_t_MBEDTLS_MD_SHA256);
        // Not a CA, no path constraint.
        check(
            mbedtls_x509write_crt_set_basic_constraints(&mut ctx.crt, 0, -1),
            "set_basic_constraints",
        )?;

        // ESP32 has no battery-backed RTC and SNTP hasn't sync'd at first
        // boot, so absolute "now" is unknown. A wide window (2026 → 2046)
        // means the cert is always considered in-validity by browsers.
        let from = CString::new("20260101000000").unwrap();
        let to = CString::new("20460101000000").unwrap();
        check(
            mbedtls_x509write_crt_set_validity(&mut ctx.crt, from.as_ptr(), to.as_ptr()),
            "set_validity",
        )?;

        // 16-byte random serial. RFC 5280 says positive integer; clear top bit.
        let mut serial = [0u8; 16];
        check(
            mbedtls_ctr_drbg_random(
                &mut ctx.ctr_drbg as *mut _ as *mut _,
                serial.as_mut_ptr(),
                serial.len(),
            ),
            "drbg_random for serial",
        )?;
        serial[0] &= 0x7f;
        if serial[0] == 0 {
            serial[0] = 1;
        }
        check(
            mbedtls_x509write_crt_set_serial_raw(
                &mut ctx.crt,
                serial.as_mut_ptr(),
                serial.len(),
            ),
            "set_serial_raw",
        )?;

        // Serialise the cert to PEM. mbedtls writes a NUL-terminated string.
        let mut cert_buf = vec![0u8; 4096];
        let rc = mbedtls_x509write_crt_pem(
            &mut ctx.crt,
            cert_buf.as_mut_ptr(),
            cert_buf.len(),
            Some(mbedtls_ctr_drbg_random),
            &mut ctx.ctr_drbg as *mut _ as *mut _,
        );
        if rc < 0 {
            return Err(anyhow!("x509write_crt_pem: {rc:#x}"));
        }
        let cert_len = cert_buf.iter().position(|&b| b == 0).unwrap_or(cert_buf.len());
        let cert_pem =
            String::from_utf8(cert_buf[..cert_len].to_vec()).context("cert pem utf8")?;

        let mut key_buf = vec![0u8; 2048];
        let rc = mbedtls_pk_write_key_pem(&mut ctx.key, key_buf.as_mut_ptr(), key_buf.len());
        if rc < 0 {
            return Err(anyhow!("pk_write_key_pem: {rc:#x}"));
        }
        let key_len = key_buf.iter().position(|&b| b == 0).unwrap_or(key_buf.len());
        let key_pem =
            String::from_utf8(key_buf[..key_len].to_vec()).context("key pem utf8")?;

        Ok((cert_pem, key_pem))
    }
}

fn check(rc: i32, what: &str) -> Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(anyhow!("mbedtls {what}: {rc:#x}"))
    }
}
