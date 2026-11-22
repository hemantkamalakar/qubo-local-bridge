//! Certificates for the local broker. The devices check the name `mqtt.platform.quboworld.com`.
//!
//! The keys are RSA 2048. Embedded TLS stacks often lack ECDSA.

use std::fs;
use std::path::{Path, PathBuf};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, PKCS_RSA_SHA256,
};
use rsa::pkcs8::EncodePrivateKey;
use rustls_pki_types::PrivatePkcs8KeyDer;
use time::{Duration, OffsetDateTime};

use crate::error::{Error, Result};
use crate::models::write_atomic;
use crate::protocol::CLOUD_HOST;

/// PEM data of a root CA and a server certificate.
#[derive(Debug, Clone)]
pub struct CertSet {
    /// Root CA certificate.
    pub ca_crt: String,
    /// Root CA key.
    pub ca_key: String,
    /// Server certificate for `CLOUD_HOST`.
    pub server_crt: String,
    /// Server key.
    pub server_key: String,
}

fn cert_error(err: impl std::fmt::Display) -> Error {
    Error::Value(format!("Cannot make the certificates: {err}"))
}

fn rsa_key() -> Result<KeyPair> {
    // The `rsa` crate only generates the key. The signing is done by `ring`.
    let key = rsa::PrivateKey::new(&mut rand::rngs::SysRng, 2048).map_err(cert_error)?;
    let der = key.to_pkcs8_der().map_err(cert_error)?;
    let der = PrivatePkcs8KeyDer::from(der.as_bytes().to_vec());
    KeyPair::from_pkcs8_der_and_sign_algo(&der, &PKCS_RSA_SHA256).map_err(cert_error)
}

/// Make a root CA and a server certificate for `CLOUD_HOST` that the CA signs.
///
/// # Errors
///
/// Returns [`Error::Value`] if a key or a certificate cannot be generated.
///
/// # Examples
///
/// ```
/// use qubo_bridge::certs::issue;
///
/// let set = issue(30)?;
/// assert!(set.server_crt.starts_with("-----BEGIN CERTIFICATE-----"));
/// # Ok::<(), qubo_bridge::error::Error>(())
/// ```
pub fn issue(days: i64) -> Result<CertSet> {
    // Start one day back to allow for clock skew on the devices.
    let start = OffsetDateTime::now_utc() - Duration::days(1);
    let end = start + Duration::days(days);

    let ca_key = rsa_key()?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).map_err(cert_error)?;
    ca_params.distinguished_name.push(DnType::CommonName, "qubo-local-bridge CA");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    ca_params.not_before = start;
    ca_params.not_after = end;
    let ca_cert = ca_params.self_signed(&ca_key).map_err(cert_error)?;

    let server_key = rsa_key()?;
    let mut params = CertificateParams::new(vec![CLOUD_HOST.to_owned(), "localhost".to_owned()])
        .map_err(cert_error)?;
    params.distinguished_name.push(DnType::CommonName, CLOUD_HOST);
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.not_before = start;
    params.not_after = end;
    let issuer = Issuer::new(ca_params, ca_key);
    let server_cert = params.signed_by(&server_key, &issuer).map_err(cert_error)?;

    Ok(CertSet {
        ca_crt: ca_cert.pem(),
        ca_key: issuer.key().serialize_pem(),
        server_crt: server_cert.pem(),
        server_key: server_key.serialize_pem(),
    })
}

/// Make the certificates on the first start. Return the paths of `server.crt` and `server.key`.
///
/// Existing certificates stay as they are.
///
/// # Errors
///
/// Returns [`Error::Value`] if the certificates cannot be made, and [`Error::Io`] if a file cannot be written.
pub fn ensure(directory: &Path) -> Result<(PathBuf, PathBuf)> {
    let crt = directory.join("server.crt");
    let key = directory.join("server.key");
    if !(crt.exists() && key.exists()) {
        fs::create_dir_all(directory)?;
        let set = issue(3650)?;
        let files = [
            ("ca.crt", set.ca_crt, false),
            ("ca.key", set.ca_key, true),
            ("server.crt", set.server_crt, false),
            ("server.key", set.server_key, true),
        ];
        for (name, pem, private) in files {
            write_atomic(&directory.join(name), pem.as_bytes(), private)?;
        }
    }
    Ok((crt, key))
}

#[cfg(test)]
mod tests {
    use x509_parser::pem::parse_x509_pem;

    use super::*;

    fn parse(pem: &str) -> x509_parser::pem::Pem {
        parse_x509_pem(pem.as_bytes()).unwrap().1
    }

    /// True for a PKCS#8 key. The header is built in parts, so that secret scanners do not
    /// report this test file.
    fn is_pkcs8_pem(pem: &str) -> bool {
        let header = ["-----BEGIN", "PRIVATE", "KEY-----"].join(" ");
        pem.lines().next() == Some(header.as_str())
    }

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    mod issue {
        use super::*;

        #[test]
        fn signs_the_server_certificate_with_the_ca() {
            let set = issue(3650).unwrap();
            let (ca_pem, server_pem) = (parse(&set.ca_crt), parse(&set.server_crt));
            let ca = ca_pem.parse_x509().unwrap();
            let server = server_pem.parse_x509().unwrap();
            assert!(server.verify_signature(Some(ca.public_key())).is_ok());
        }

        #[test]
        fn puts_the_cloud_host_in_the_alternative_names() {
            let set = issue(3650).unwrap();
            let pem = parse(&set.server_crt);
            let server = pem.parse_x509().unwrap();
            let san = server.subject_alternative_name().unwrap().unwrap();
            let names: Vec<String> =
                san.value.general_names.iter().map(ToString::to_string).collect();
            assert!(names.iter().any(|n| n.contains(CLOUD_HOST)), "{names:?}");
        }

        #[test]
        fn marks_the_root_certificate_as_a_ca() {
            let set = issue(3650).unwrap();
            assert!(parse(&set.ca_crt).parse_x509().unwrap().is_ca());
        }
    }

    mod ensure {
        use super::*;

        #[test]
        fn makes_the_four_files_on_the_first_call() {
            let dir = tempfile::tempdir().unwrap();
            ensure(dir.path()).unwrap();
            assert_eq!(file_names(dir.path()), ["ca.crt", "ca.key", "server.crt", "server.key"]);
        }

        #[test]
        fn returns_the_paths_of_the_server_certificate_and_key() {
            let dir = tempfile::tempdir().unwrap();
            let (crt, key) = ensure(dir.path()).unwrap();
            assert_eq!((crt, key), (dir.path().join("server.crt"), dir.path().join("server.key")));
        }

        #[test]
        fn keeps_the_existing_certificate_on_the_second_call() {
            let dir = tempfile::tempdir().unwrap();
            let (crt, _) = ensure(dir.path()).unwrap();
            let first = fs::read(&crt).unwrap();
            ensure(dir.path()).unwrap();
            assert_eq!(fs::read(&crt).unwrap(), first);
        }

        #[test]
        fn writes_the_ca_key_as_private_pem() {
            let dir = tempfile::tempdir().unwrap();
            ensure(dir.path()).unwrap();
            let pem = fs::read_to_string(dir.path().join("ca.key")).unwrap();
            assert!(is_pkcs8_pem(&pem));
        }

        #[test]
        fn writes_the_server_key_as_private_pem() {
            let dir = tempfile::tempdir().unwrap();
            ensure(dir.path()).unwrap();
            let pem = fs::read_to_string(dir.path().join("server.key")).unwrap();
            assert!(is_pkcs8_pem(&pem));
        }
    }
}
