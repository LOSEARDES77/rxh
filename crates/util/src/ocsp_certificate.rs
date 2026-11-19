//! A server certificate that names an OCSP responder, for tests of OCSP
//! stapling.

use rcgen::{BasicConstraints, CertificateParams, CustomExtension, IsCa, Issuer, KeyPair};

/// A certificate for `tls.test` signed by a throwaway authority.
#[derive(Debug)]
pub struct OcspCertificate {
    /// PEM of the certificate followed by its authority.
    pub chain_pem: String,
    /// PEM of the private key.
    pub key_pem: String,
    /// DER of the authority, the root clients have to trust.
    pub authority_der: Vec<u8>,
}

impl OcspCertificate {
    /// Serial number of the certificate, as its DER has it: the leading
    /// zero keeps it positive.
    pub const SERIAL: [u8; 3] = [0x00, 0xab, 0xcd];

    /// Generates the certificate, with the URL of its OCSP responder if
    /// `responder` is given.
    ///
    /// # Panics
    ///
    /// Panics if `responder` is longer than 100 bytes.
    #[must_use]
    pub fn new(responder: Option<&str>) -> Self {
        let authority_key = KeyPair::generate().expect("key pair");
        let mut authority = CertificateParams::new(Vec::new()).expect("valid parameters");
        authority.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let authority_cert = authority
            .self_signed(&authority_key)
            .expect("self-signed authority");

        let key = KeyPair::generate().expect("key pair");
        let mut params =
            CertificateParams::new(vec![String::from("tls.test")]).expect("valid parameters");
        params.serial_number = Some(rcgen::SerialNumber::from_slice(&Self::SERIAL[1..]));
        if let Some(url) = responder {
            // AuthorityInfoAccess { AccessDescription { id-ad-ocsp, URI } }
            let len = |len: usize| u8::try_from(len).ok().filter(|len| *len < 0x80);
            let mut description = vec![0x06, 0x08, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x30, 0x01];
            description.extend([0x86, len(url.len()).expect("a short URL")]);
            description.extend(url.as_bytes());
            let mut access = vec![0x30, len(description.len() + 2).expect("a short URL")];
            access.extend([0x30, len(description.len()).expect("a short URL")]);
            access.extend(description);
            let oid = [1, 3, 6, 1, 5, 5, 7, 1, 1];
            params
                .custom_extensions
                .push(CustomExtension::from_oid_content(&oid, access));
        }
        let cert = params
            .signed_by(&key, &Issuer::new(authority, authority_key))
            .expect("signed certificate");

        Self {
            chain_pem: format!("{}{}", cert.pem(), authority_cert.pem()),
            key_pem: key.serialize_pem(),
            authority_der: authority_cert.der().to_vec(),
        }
    }
}
