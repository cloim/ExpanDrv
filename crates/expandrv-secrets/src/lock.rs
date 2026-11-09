//! 앱 PIN 검증값. 설정 파일별로 분리하고 현재 사용자의 암호화 저장소에 둡니다.
use crate::CredentialStore;
use expandrv_core::{Error, Result};
use ring::{
    digest, pbkdf2,
    rand::{SecureRandom, SystemRandom},
};
use secrecy::{ExposeSecret, SecretString};
use std::{num::NonZeroU32, path::Path};

const ITERATIONS: NonZeroU32 = NonZeroU32::new(120_000).unwrap();
const LOCKED: &str = "locked";
const UNLOCKED: &str = "unlocked";

pub struct PinStore {
    target: String,
    /// 재시작 뒤에도 유지할 마지막 잠금 상태
    state_target: String,
}

impl PinStore {
    pub fn new(config: &Path) -> Result<Self> {
        let path = std::path::absolute(config)?
            .to_string_lossy()
            .to_lowercase();
        let hash = hex(digest::digest(&digest::SHA256, path.as_bytes()).as_ref());
        Ok(Self {
            target: format!("ExpanDrv.AppLock/{hash}/pin"),
            state_target: format!("ExpanDrv.AppLock/{hash}/state"),
        })
    }

    /// 마지막으로 기록한 잠금 상태. 기록이 없거나 읽지 못하면 잠김으로 봅니다.
    pub fn remembered_locked(&self) -> bool {
        !matches!(
            CredentialStore.get(&self.state_target),
            Ok(v) if v.expose_secret() == UNLOCKED
        )
    }

    /// PIN 잠금 해제나 사용자의 잠금을 기록해 에이전트를 다시 시작해도 같은 상태로 시작하게 합니다.
    pub fn remember_locked(&self, locked: bool) -> Result<()> {
        let value = if locked { LOCKED } else { UNLOCKED };
        CredentialStore.set(&self.state_target, &SecretString::from(value))
    }

    pub fn configured(&self) -> Result<bool> {
        match CredentialStore.get(&self.target) {
            Ok(_) => Ok(true),
            Err(Error::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn verify(&self, pin: &str) -> Result<()> {
        let saved = CredentialStore.get(&self.target)?;
        verify(saved.expose_secret(), pin)
    }

    pub fn remove(&self, current: &str) -> Result<()> {
        self.verify(current)?;
        CredentialStore.delete(&self.target)?;
        match CredentialStore.delete(&self.state_target) {
            Err(Error::NotFound) => Ok(()),
            r => r,
        }
    }

    pub fn set(&self, current: &str, new: &str) -> Result<()> {
        validate(new)?;
        if self.configured()? {
            self.verify(current)?;
        }
        CredentialStore.set(&self.target, &SecretString::from(encode(new)?))
    }
}

fn validate(pin: &str) -> Result<()> {
    if (6..=12).contains(&pin.len()) && pin.bytes().all(|b| b.is_ascii_digit()) {
        Ok(())
    } else {
        Err(Error::Config("PIN은 숫자 6~12자리로 입력하세요".into()))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn encode(pin: &str) -> Result<String> {
    validate(pin)?;
    let mut salt = [0; 16];
    SystemRandom::new()
        .fill(&mut salt)
        .map_err(|_| Error::Config("PIN 난수 생성 실패".into()))?;
    let mut hash = [0; 32];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        ITERATIONS,
        &salt,
        pin.as_bytes(),
        &mut hash,
    );
    Ok(format!("v1:{}:{}", hex(&salt), hex(&hash)))
}

fn verify(saved: &str, pin: &str) -> Result<()> {
    let parts: Vec<_> = saved.split(':').collect();
    if parts.len() != 3 || parts[0] != "v1" || parts[1].len() != 32 || parts[2].len() != 64 {
        return Err(Error::Config("PIN 검증값 형식이 올바르지 않습니다".into()));
    }
    let decode = |text: &str| -> Result<Vec<u8>> {
        text.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let text = std::str::from_utf8(pair)
                    .map_err(|_| Error::Config("PIN 검증값 형식 오류".into()))?;
                u8::from_str_radix(text, 16)
                    .map_err(|_| Error::Config("PIN 검증값 형식 오류".into()))
            })
            .collect()
    };
    pbkdf2::verify(
        pbkdf2::PBKDF2_HMAC_SHA256,
        ITERATIONS,
        &decode(parts[1])?,
        pin.as_bytes(),
        &decode(parts[2])?,
    )
    .map_err(|_| Error::Auth("PIN이 일치하지 않습니다".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn salted_pin_roundtrip_and_rejection() {
        let encoded = encode("123456").unwrap();
        verify(&encoded, "123456").unwrap();
        assert!(verify(&encoded, "654321").is_err());
        assert_ne!(encoded, encode("123456").unwrap());
        assert!(!encoded.contains("123456"));
        assert!(verify("v1:invalid", "123456").is_err());
        assert!(encode("12345").is_err());
        assert!(encode("abcdef").is_err());
    }
}
