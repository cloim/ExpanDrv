//! 인증서 고정(pinning). 서버 리프 인증서 DER의 SHA-256이 설정 값과 같을 때만 신뢰합니다.
//! 체인과 호스트 이름 검증은 지문 일치로 대신하지만 핸드셰이크 서명 검증은 그대로 수행합니다.

use expandrv_core::{Error, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// `AB:CD:..`, 공백 구분, 대소문자를 모두 받아 32바이트 지문으로 바꿉니다.
pub(crate) fn parse_fingerprint(s: &str) -> Result<[u8; 32]> {
    let hex: String = s
        .chars()
        .filter(|c| !matches!(c, ':' | ' ' | '-'))
        .collect();
    let bad =
        || Error::Config("pinned_cert_sha256은 64자리 16진수 SHA-256 지문이어야 합니다".into());
    if hex.len() != 64 || !hex.is_ascii() {
        return Err(bad());
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| bad())?;
    }
    Ok(out)
}

#[derive(Debug)]
pub(crate) struct PinnedVerifier {
    pin: [u8; 32],
    algs: WebPkiSupportedAlgorithms,
}

impl PinnedVerifier {
    pub(crate) fn new(pin: [u8; 32], provider: &CryptoProvider) -> Self {
        Self {
            pin,
            algs: provider.signature_verification_algorithms,
        }
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let digest: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
        if digest == self.pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// 고정 지문 검증기를 쓰는 rustls 설정. TLS 1.2 이상만 허용합니다.
pub(crate) fn pinned_client_config(pin: [u8; 32]) -> Result<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinnedVerifier::new(pin, &provider));
    let cfg = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|e| Error::Config(format!("TLS 설정 실패: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(cfg)
}
