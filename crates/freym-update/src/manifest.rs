use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, prelude::BASE64_STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path};
use url::Url;

pub const MAX_MANIFEST: u64 = 256 * 1024;
pub const MAX_PAYLOAD: u64 = 512 * 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Trust {
    pub schema_version: u32,
    pub manifest_url: Option<String>,
    pub public_keys: Vec<String>,
    pub allowed_origins: Vec<String>,
    pub channel: String,
    pub package: String,
}
impl Trust {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1,
            "Unsupported update trust configuration"
        );
        ensure!(!self.channel.is_empty(), "Missing update channel");
        ensure!(
            !self.public_keys.is_empty(),
            "Production update signing keys are not configured"
        );
        for key in &self.public_keys {
            let key: [u8; 32] = BASE64_STANDARD
                .decode(key)?
                .try_into()
                .map_err(|_| anyhow::anyhow!("Invalid update public key"))?;
            ensure!(
                !VerifyingKey::from_bytes(&key)?.is_weak(),
                "Weak update public key"
            );
        }
        self.url(
            self.manifest_url
                .as_deref()
                .context("Production update endpoint is not configured")?,
        )?;
        Ok(())
    }
    pub fn url(&self, text: &str) -> Result<Url> {
        let url = Url::parse(text)?;
        ensure!(
            url.scheme() == "https"
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none(),
            "Updates require a trusted HTTPS URL"
        );
        ensure!(
            self.allowed_origins.iter().any(|origin| {
                Url::parse(origin).is_ok_and(|o| {
                    o.scheme() == "https"
                        && o.origin() == url.origin()
                        && o.path() == "/"
                        && o.query().is_none()
                        && o.fragment().is_none()
                        && o.username().is_empty()
                        && o.password().is_none()
                })
            }),
            "Update URL origin is not trusted"
        );
        Ok(url)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Envelope {
    schema_version: u32,
    payload: String,
    signature: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub product: String,
    pub channel: String,
    pub version: String,
    pub issued_at: u64,
    pub expires_at: u64,
    pub artifacts: Vec<Artifact>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Artifact {
    pub target: String,
    pub package: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
    pub min_launcher_protocol: u32,
}
pub struct VerifiedRelease {
    pub manifest: Manifest,
    pub envelope: Vec<u8>,
}
impl VerifiedRelease {
    pub fn verify(bytes: &[u8], trust: &Trust, now: Option<u64>) -> Result<Self> {
        trust.validate()?;
        ensure!(
            bytes.len() as u64 <= MAX_MANIFEST,
            "Update manifest is too large"
        );
        let envelope: Envelope = serde_json::from_slice(bytes)?;
        ensure!(
            envelope.schema_version == 1,
            "Unsupported update envelope version"
        );
        let payload = BASE64_STANDARD.decode(&envelope.payload)?;
        let signature = Signature::from_slice(&BASE64_STANDARD.decode(&envelope.signature)?)?;
        let valid = trust.public_keys.iter().any(|key| {
            let Ok(raw) = BASE64_STANDARD.decode(key) else {
                return false;
            };
            let Ok(raw) = <[u8; 32]>::try_from(raw) else {
                return false;
            };
            VerifyingKey::from_bytes(&raw)
                .is_ok_and(|key| key.verify_strict(&payload, &signature).is_ok())
        });
        ensure!(valid, "Update manifest signature is invalid");
        let manifest: Manifest = serde_json::from_slice(&payload)?;
        ensure!(
            manifest.schema_version == 1
                && manifest.product == "freym"
                && manifest.channel == trust.channel,
            "Update manifest product, channel or schema does not match"
        );
        let version = Version::parse(&manifest.version)?;
        ensure!(
            trust.channel != "stable" || version.pre.is_empty(),
            "Prerelease on stable update channel"
        );
        ensure!(
            manifest.expires_at > manifest.issued_at
                && manifest.expires_at - manifest.issued_at <= 14 * 86400,
            "Invalid update manifest validity period"
        );
        if let Some(now) = now {
            ensure!(
                manifest.issued_at <= now.saturating_add(300) && now < manifest.expires_at,
                "Update manifest has expired or the system clock is incorrect"
            );
        }
        ensure!(manifest.artifacts.len() <= 24, "Too many update artifacts");
        let mut targets = std::collections::HashSet::new();
        for artifact in &manifest.artifacts {
            ensure!(
                targets.insert((&artifact.target, &artifact.package)),
                "Duplicate update artifact"
            );
            trust.url(&artifact.url)?;
            ensure!(
                artifact.size > 0 && artifact.size <= MAX_PAYLOAD,
                "Unsupported update size"
            );
            ensure!(
                artifact.sha256.len() == 64
                    && artifact
                        .sha256
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                "Invalid update SHA-256"
            );
        }
        Ok(Self {
            manifest,
            envelope: bytes.to_vec(),
        })
    }
    pub fn artifact(&self) -> Result<&Artifact> {
        self.manifest
            .artifacts
            .iter()
            .find(|a| {
                a.target == super::platform::target() && a.package == super::platform::package()
            })
            .context("No supported update package for this platform")
    }
    pub fn newer_than(&self, installed: &str) -> Result<bool> {
        Ok(Version::parse(&self.manifest.version)?
            .cmp_precedence(&Version::parse(installed)?)
            .is_gt())
    }
}

pub fn hash_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
pub fn verify_file(path: &Path, artifact: &Artifact) -> Result<()> {
    if std::fs::metadata(path)?.len() != artifact.size || hash_file(path)? != artifact.sha256 {
        bail!("Downloaded update failed integrity verification");
    }
    Ok(())
}
pub fn now() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs())
}
