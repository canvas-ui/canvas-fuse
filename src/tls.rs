//! Per-endpoint client identity. Secrets are never included in Debug output.
use anyhow::{bail, Context, Result};
use openssl::{asn1::Asn1Time, pkey::PKey, x509::X509};
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, path::PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsFiles {
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
}
#[derive(Clone)]
pub struct ClientIdentity {
    cert: Vec<u8>,
    key: Vec<u8>,
}
impl std::fmt::Debug for ClientIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClientIdentity([redacted])")
    }
}
impl TlsFiles {
    pub fn load(&self, server: &str) -> Result<ClientIdentity> {
        let url = reqwest::Url::parse(server)?;
        if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
            bail!("client certificates require an HTTPS remote without URL credentials");
        }
        let cert = std::fs::read(&self.cert_file).context("cannot read client certificate file")?;
        let key = std::fs::read(&self.key_file).context("cannot read client private key file")?;
        if String::from_utf8_lossy(&key).contains("ENCRYPTED") {
            bail!("encrypted client keys are unsupported; use a protected unencrypted PEM key");
        }
        let chain = X509::stack_from_pem(&cert).context("invalid PEM certificate chain")?;
        let leaf = chain.first().context("empty client certificate chain")?;
        let key =
            PKey::private_key_from_pem(&key).context("invalid unencrypted PEM private key")?;
        if !leaf.public_key()?.public_eq(&key) {
            bail!("client certificate and key do not match");
        }
        let now = Asn1Time::days_from_now(0)?;
        for (i, c) in chain.iter().enumerate() {
            if c.not_before().compare(&now)? == Ordering::Greater
                || c.not_after().compare(&now)? != Ordering::Greater
            {
                bail!("client certificate chain contains an expired or not-yet-valid certificate");
            }
            if let Some(issuer) = chain.get(i + 1) {
                let public = issuer.public_key()?;
                if !c.verify(&public)? {
                    bail!("client chain must be leaf first followed by issuing intermediates");
                }
            }
        }
        Ok(ClientIdentity {
            cert,
            key: key.private_key_to_pem_pkcs8()?,
        })
    }
}
impl ClientIdentity {
    pub fn http(&self) -> Result<reqwest::Identity> {
        Ok(reqwest::Identity::from_pkcs8_pem(&self.cert, &self.key)?)
    }
    pub fn connector(&self) -> Result<native_tls::TlsConnector> {
        Ok(native_tls::TlsConnector::builder()
            .identity(native_tls::Identity::from_pkcs8(&self.cert, &self.key)?)
            .build()?)
    }
}
pub fn http_builder(
    server: &str,
    identity: Option<&ClientIdentity>,
) -> Result<reqwest::blocking::ClientBuilder> {
    let mut builder = reqwest::blocking::Client::builder();
    if let Some(identity) = identity {
        let origin = reqwest::Url::parse(server)?.origin();
        builder = builder
            .identity(identity.http()?)
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.url().origin() != origin
                    || !attempt.url().username().is_empty()
                    || attempt.url().password().is_some()
                {
                    attempt.error("refusing cross-origin client certificate redirect")
                } else if attempt.previous().len() >= 10 {
                    attempt.error("too many redirects")
                } else {
                    attempt.follow()
                }
            }));
    }
    Ok(builder)
}
