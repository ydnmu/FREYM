use super::*;
use base64::{Engine, prelude::BASE64_STANDARD};
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use std::fs;

fn key() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}
fn test_trust() -> Trust {
    Trust {
        schema_version: 1,
        manifest_url: Some("https://updates.example.test/stable.json".into()),
        public_keys: vec![BASE64_STANDARD.encode(key().verifying_key().as_bytes())],
        allowed_origins: vec!["https://updates.example.test".into()],
        channel: "stable".into(),
        package: platform::package().into(),
    }
}
fn release(payload: &[u8], version: &str) -> Manifest {
    let now = manifest::now().unwrap();
    Manifest {
        schema_version: 1,
        product: "freym".into(),
        channel: "stable".into(),
        version: version.into(),
        issued_at: now - 60,
        expires_at: now + 86400,
        artifacts: vec![Artifact {
            target: platform::target(),
            package: platform::package().into(),
            url: "https://updates.example.test/freym".into(),
            size: payload.len() as u64,
            sha256: format!("{:x}", Sha256::digest(payload)),
            min_launcher_protocol: 1,
        }],
    }
}
fn sign(manifest: &Manifest) -> Vec<u8> {
    let payload = serde_json::to_vec(manifest).unwrap();
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 1, "payload": BASE64_STANDARD.encode(&payload),
        "signature": BASE64_STANDARD.encode(key().sign(&payload).to_bytes()),
    }))
    .unwrap()
}
fn verify(manifest: &Manifest) -> anyhow::Result<VerifiedRelease> {
    VerifiedRelease::verify(
        &sign(manifest),
        &test_trust(),
        Some(manifest::now().unwrap()),
    )
}

#[test]
fn shipping_configuration_cannot_install_unsigned_updates() {
    let config = trust().unwrap();
    assert!(config.validate().is_err());
    assert_eq!(config.package, "disabled");
    assert!(config.public_keys.is_empty());
    assert!(config.manifest_url.is_none());
}
#[test]
fn signatures_are_verified_over_exact_payload_bytes() {
    let m = release(b"update", "0.2.0");
    assert!(verify(&m).is_ok());
    let mut envelope: serde_json::Value = serde_json::from_slice(&sign(&m)).unwrap();
    envelope["payload"] = BASE64_STANDARD.encode(b"tampered").into();
    assert!(
        VerifiedRelease::verify(&serde_json::to_vec(&envelope).unwrap(), &test_trust(), None)
            .is_err()
    );
    let mut trust = test_trust();
    trust.public_keys =
        vec![BASE64_STANDARD.encode(SigningKey::from_bytes(&[8; 32]).verifying_key().as_bytes())];
    assert!(VerifiedRelease::verify(&sign(&m), &trust, None).is_err());
    trust.public_keys = vec![BASE64_STANDARD.encode([0; 32])];
    assert!(trust.validate().is_err());
}
#[test]
fn manifest_rejects_wrong_product_channel_schema_expiry_and_duplicates() {
    let good = release(b"update", "0.2.0");
    let mut bad = good.clone();
    bad.product = "plugin".into();
    assert!(verify(&bad).is_err());
    bad = good.clone();
    bad.channel = "beta".into();
    assert!(verify(&bad).is_err());
    bad = good.clone();
    bad.schema_version = 2;
    assert!(verify(&bad).is_err());
    bad = good.clone();
    bad.expires_at = bad.issued_at + 1;
    assert!(verify(&bad).is_err());
    bad = good.clone();
    bad.issued_at += 86400;
    assert!(verify(&bad).is_err());
    bad = good.clone();
    bad.expires_at += 30 * 86400;
    assert!(verify(&bad).is_err());
    bad = good.clone();
    bad.artifacts.push(bad.artifacts[0].clone());
    assert!(verify(&bad).is_err());
    bad = good.clone();
    bad.artifacts[0].size = manifest::MAX_PAYLOAD + 1;
    assert!(verify(&bad).is_err());
    bad = good.clone();
    bad.version = "0.2.0-beta.1".into();
    assert!(verify(&bad).is_err());
    assert!(verify(&good).unwrap().newer_than("0.1.0").unwrap());
    assert!(!verify(&good).unwrap().newer_than("0.2.0").unwrap());
    assert!(!verify(&good).unwrap().newer_than("0.3.0").unwrap());
    assert!(
        !verify(&good)
            .unwrap()
            .newer_than("0.2.0+other-build")
            .unwrap()
    );
}
#[test]
fn untrusted_urls_and_unsupported_packages_are_refused() {
    let trust = test_trust();
    for url in [
        "http://updates.example.test/a",
        "https://evil.test/a",
        "https://updates.example.test:444/a",
        "https://user@updates.example.test/a",
        "https://updates.example.test/a#fragment",
    ] {
        assert!(trust.url(url).is_err(), "{url}");
    }
    let mut m = release(b"update", "0.2.0");
    m.artifacts[0].package = "deb".into();
    assert!(verify(&m).unwrap().artifact().is_err());
    let temp = tempfile::tempdir().unwrap();
    let mut trust = test_trust();
    trust.package = "disabled".into();
    assert!(platform::portable_root(&trust, &temp.path().join(platform::app_name())).is_err());
}

struct Fixture {
    _temp: tempfile::TempDir,
    layout: install::Layout,
    trust: Trust,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let layout = install::Layout::new(temp.path()).unwrap();
        fs::write(layout.app(), b"old executable").unwrap();
        for name in ["native-ui.json", "plugin-installation", "recording.mkv"] {
            fs::write(temp.path().join(name), b"user data").unwrap();
        }
        let fixture = Self {
            _temp: temp,
            layout,
            trust: test_trust(),
        };
        fixture.stage("0.2.0");
        fixture
    }
    fn stage(&self, version: &str) {
        let payload = b"new executable";
        fs::write(self.layout.payload(), payload).unwrap();
        fs::write(self.layout.ready(), sign(&release(payload, version))).unwrap();
    }
    fn preserve(&self) {
        for name in ["native-ui.json", "plugin-installation", "recording.mkv"] {
            assert_eq!(fs::read(self.layout.root.join(name)).unwrap(), b"user data");
        }
    }
}
#[test]
fn running_instance_prevents_installation_lock_and_locks_release_on_exit() {
    let f = Fixture::new();
    let guard = f.layout.lock("running.lock", true).unwrap();
    assert!(f.layout.running_lock().is_err());
    assert_eq!(fs::read(f.layout.app()).unwrap(), b"old executable");
    drop(guard);
    assert!(f.layout.running_lock().is_ok());
    let download = f.layout.download_lock().unwrap();
    assert!(f.layout.download_lock().is_err());
    drop(download);
    assert!(f.layout.download_lock().is_ok());
}
#[test]
fn corrupt_download_never_replaces_installed_application() {
    let f = Fixture::new();
    fs::write(f.layout.payload(), b"new executablE").unwrap();
    assert!(install::install_ready(&f.layout, &f.trust).is_err());
    fs::write(f.layout.payload(), b"partial").unwrap();
    assert!(install::install_ready(&f.layout, &f.trust).is_err());
    assert_eq!(fs::read(f.layout.app()).unwrap(), b"old executable");
    assert!(!f.layout.journal().exists());
    f.preserve();
}

#[test]
fn interrupted_download_cleanup_preserves_ready_payloads_and_user_files() {
    let f = Fixture::new();
    fs::write(
        f.layout.directory.join(".freym-download-interrupted"),
        b"partial",
    )
    .unwrap();
    fs::write(f.layout.directory.join("other-user-file"), b"preserve").unwrap();
    let _download = f.layout.download_lock().unwrap();
    f.layout.cleanup_downloads().unwrap();
    assert!(
        !f.layout
            .directory
            .join(".freym-download-interrupted")
            .exists()
    );
    assert!(f.layout.ready().exists());
    assert!(f.layout.payload().exists());
    assert_eq!(
        fs::read(f.layout.directory.join("other-user-file")).unwrap(),
        b"preserve"
    );
    f.preserve();
}
#[test]
fn failed_launch_rolls_back_and_rejects_the_same_release() {
    let f = Fixture::new();
    install::install_ready(&f.layout, &f.trust).unwrap();
    // Installation remains pending until a user launches the application.
    install::recover(&f.layout, &f.trust).unwrap();
    assert_eq!(fs::read(f.layout.app()).unwrap(), b"new executable");
    assert!(install::begin_launch(&f.layout).unwrap().is_some());
    install::recover(&f.layout, &f.trust).unwrap();
    assert_eq!(fs::read(f.layout.app()).unwrap(), b"old executable");
    assert!(!f.layout.journal().exists());
    assert!(f.layout.last_error().unwrap().is_some());
    f.stage("0.2.0");
    assert!(install::install_ready(&f.layout, &f.trust).is_err());
    f.preserve();
}
#[test]
fn healthy_launch_commits_receipt_and_rejects_downgrade() {
    let f = Fixture::new();
    install::install_ready(&f.layout, &f.trust).unwrap();
    let nonce = install::begin_launch(&f.layout).unwrap().unwrap();
    fs::write(f.layout.healthy(), nonce).unwrap();
    install::recover(&f.layout, &f.trust).unwrap();
    assert_eq!(
        install::installed_version(&f.layout, &f.trust).unwrap(),
        "0.2.0"
    );
    assert_eq!(fs::read(f.layout.app()).unwrap(), b"new executable");
    f.stage("0.1.1");
    assert!(install::install_ready(&f.layout, &f.trust).is_err());
    f.preserve();
}
#[test]
fn recovery_handles_interruption_before_replacement_and_refuses_corrupt_backup() {
    let f = Fixture::new();
    install::install_ready(&f.layout, &f.trust).unwrap();
    fs::copy(f.layout.backup(), f.layout.app()).unwrap();
    install::recover(&f.layout, &f.trust).unwrap();
    assert!(!f.layout.journal().exists());
    f.stage("0.2.0");
    install::install_ready(&f.layout, &f.trust).unwrap();
    install::begin_launch(&f.layout).unwrap();
    fs::write(f.layout.backup(), b"corrupt").unwrap();
    assert!(install::recover(&f.layout, &f.trust).is_err());
    assert!(f.layout.journal().exists());
    f.preserve();
}

#[test]
fn interrupted_replacement_with_missing_app_restores_a_verified_backup() {
    let f = Fixture::new();
    install::install_ready(&f.layout, &f.trust).unwrap();
    fs::remove_file(f.layout.app()).unwrap();
    install::recover(&f.layout, &f.trust).unwrap();
    assert_eq!(fs::read(f.layout.app()).unwrap(), b"old executable");
    assert!(!f.layout.journal().exists());
    assert!(f.layout.last_error().unwrap().is_some());
    f.preserve();
}
#[test]
fn launcher_protocol_and_metadata_limits_fail_closed() {
    let f = Fixture::new();
    let mut m = release(b"new executable", "0.2.0");
    m.artifacts[0].min_launcher_protocol = LAUNCHER_PROTOCOL + 1;
    fs::write(f.layout.ready(), sign(&m)).unwrap();
    assert!(install::install_ready(&f.layout, &f.trust).is_err());
    fs::write(
        f.layout.ready(),
        vec![b' '; manifest::MAX_MANIFEST as usize + 1],
    )
    .unwrap();
    assert!(install::install_ready(&f.layout, &f.trust).is_err());
    assert_eq!(fs::read(f.layout.app()).unwrap(), b"old executable");
}
