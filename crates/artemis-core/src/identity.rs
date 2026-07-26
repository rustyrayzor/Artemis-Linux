use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use openssl::asn1::Asn1Time;
use openssl::bn::BigNum;
use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, Private};
use openssl::rsa::Rsa;
use openssl::x509::{X509, X509NameBuilder};

use crate::{Error, Result};

const CERT_FILE: &str = "client.crt";
const KEY_FILE: &str = "client.key";
const PROJECT_APPLICATION: &str = "Artemis-Linux";
const UNIQUE_ID_FILE: &str = "uniqueid";

/// Long-lived client identity used for pairing and mutual TLS.
#[derive(Clone)]
pub struct ClientIdentity {
    config_dir: PathBuf,
    unique_id: String,
    certificate: X509,
    private_key: PKey<Private>,
}

impl ClientIdentity {
    /// Loads or creates identity data in the standard per-user configuration directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the platform directory, identity files, or cryptography fail.
    pub fn load_or_create_default() -> Result<Self> {
        let directories =
            directories::ProjectDirs::from("com", "Rayzor Studios", PROJECT_APPLICATION)
                .ok_or_else(|| {
                    Error::Configuration(
                        "the operating system did not provide a user configuration directory"
                            .to_owned(),
                    )
                })?;
        Self::load_or_create(directories.config_dir())
    }

    /// Loads an existing identity or atomically creates a new one.
    ///
    /// # Errors
    ///
    /// Returns an error when files cannot be read or written, persisted data is invalid,
    /// or certificate generation fails.
    pub fn load_or_create(config_dir: impl AsRef<Path>) -> Result<Self> {
        let config_dir = config_dir.as_ref().to_path_buf();
        fs::create_dir_all(&config_dir)?;
        #[cfg(unix)]
        secure_directory(&config_dir)?;

        let cert_path = config_dir.join(CERT_FILE);
        let key_path = config_dir.join(KEY_FILE);
        let unique_id_path = config_dir.join(UNIQUE_ID_FILE);

        if cert_path.exists() && key_path.exists() && unique_id_path.exists() {
            let certificate = X509::from_pem(&fs::read(cert_path)?)?;
            let private_key = PKey::private_key_from_pem(&fs::read(key_path)?)?;
            let unique_id = fs::read_to_string(unique_id_path)?.trim().to_owned();
            validate_unique_id(&unique_id)?;
            return Ok(Self {
                config_dir,
                unique_id,
                certificate,
                private_key,
            });
        }

        let private_key = PKey::from_rsa(Rsa::generate(2048)?)?;
        let certificate = generate_certificate(&private_key)?;
        let mut unique_id_bytes = [0_u8; 8];
        openssl::rand::rand_bytes(&mut unique_id_bytes)?;
        let unique_id = hex::encode(unique_id_bytes);

        write_private(&key_path, &private_key.private_key_to_pem_pkcs8()?)?;
        write_private(&cert_path, &certificate.to_pem()?)?;
        write_private(&unique_id_path, unique_id.as_bytes())?;

        Ok(Self {
            config_dir,
            unique_id,
            certificate,
            private_key,
        })
    }

    #[must_use]
    pub fn unique_id(&self) -> &str {
        &self.unique_id
    }

    #[must_use]
    pub fn certificate(&self) -> &X509 {
        &self.certificate
    }

    #[must_use]
    pub fn private_key(&self) -> &PKey<Private> {
        &self.private_key
    }

    /// Returns the PEM-encoded public client certificate.
    ///
    /// # Errors
    ///
    /// Returns an error if OpenSSL cannot encode the certificate.
    pub fn certificate_pem(&self) -> Result<Vec<u8>> {
        Ok(self.certificate.to_pem()?)
    }

    #[must_use]
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }
}

fn generate_certificate(private_key: &PKey<Private>) -> Result<X509> {
    let mut name = X509NameBuilder::new()?;
    name.append_entry_by_text("CN", "NVIDIA GameStream Client")?;
    let name = name.build();

    let mut serial_bytes = [0_u8; 16];
    openssl::rand::rand_bytes(&mut serial_bytes)?;
    serial_bytes[0] &= 0x7f;
    let serial = BigNum::from_slice(&serial_bytes)?.to_asn1_integer()?;

    let mut builder = X509::builder()?;
    builder.set_version(2)?;
    builder.set_serial_number(&serial)?;
    builder.set_subject_name(&name)?;
    builder.set_issuer_name(&name)?;
    builder.set_pubkey(private_key)?;
    builder.set_not_before(Asn1Time::days_from_now(0)?.as_ref())?;
    builder.set_not_after(Asn1Time::days_from_now(3650)?.as_ref())?;
    builder.sign(private_key, MessageDigest::sha256())?;
    Ok(builder.build())
}

fn validate_unique_id(value: &str) -> Result<()> {
    if value.len() == 16 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(Error::Configuration(
            "the persisted unique ID must contain 16 hexadecimal characters".to_owned(),
        ))
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::PROJECT_APPLICATION;

    #[test]
    fn project_directory_matches_documented_linux_slug() {
        let directories =
            directories::ProjectDirs::from("com", "Rayzor Studios", PROJECT_APPLICATION)
                .expect("Linux should provide a home directory");

        assert_eq!(
            directories.project_path(),
            std::path::Path::new("artemis-linux")
        );
    }
}
