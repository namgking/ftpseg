//! Portable credential obfuscation, NOT secure cryptography.
//!
//! New values use a public, fixed XOR key and hex encoding, independent of the
//! OS account. Anyone with the file and this code can recover the credentials.
//! Length and checksum only detect accidental corruption, not malicious edits.
//! Legacy DPAPI/keyring values remain readable on their original platform for migration.

use crate::error::{Error, Result};

#[cfg(windows)]
const DPAPI_PREFIX: &str = "dpapi:v1:";
#[cfg(any(target_os = "linux", target_os = "macos"))]
const KEYRING_PREFIX: &str = "os-keyring:v1:";

const OBFUSCATED_PREFIX: &str = "obfuscated:v1:";
const XOR_KEY: &[u8] = b"FTPMUTIL-obfuscation-v1";
const HEADER_LEN: usize = 12;

/// Obfuscate UTF-8 using a fixed public key; this provides no security guarantee.
pub(crate) fn protect(plaintext: &str) -> Result<String> {
    let bytes = plaintext.as_bytes();
    let length = u64::try_from(bytes.len()).map_err(|_| invalid("payload too large"))?;
    let mut payload = Vec::with_capacity(HEADER_LEN + bytes.len());
    payload.extend_from_slice(&length.to_le_bytes());
    payload.extend_from_slice(&checksum(bytes).to_le_bytes());
    payload.extend_from_slice(bytes);
    xor(&mut payload);
    Ok(format!("{OBFUSCATED_PREFIX}{}", hex_encode(&payload)))
}

/// Read portable values first; legacy OS-bound values are only read for migration.
pub(crate) fn unprotect(encoded: &str) -> Result<String> {
    let Some(hex) = encoded.strip_prefix(OBFUSCATED_PREFIX) else {
        return platform::unprotect(encoded);
    };
    let mut payload = hex_decode(hex)?;
    xor(&mut payload);
    if payload.len() < HEADER_LEN {
        return Err(invalid("truncated obfuscated payload"));
    }
    let length = u64::from_le_bytes(payload[..8].try_into().unwrap());
    let expected_checksum = u32::from_le_bytes(payload[8..HEADER_LEN].try_into().unwrap());
    let bytes = &payload[HEADER_LEN..];
    if u64::try_from(bytes.len()).ok() != Some(length) {
        return Err(invalid("obfuscated payload length mismatch"));
    }
    if checksum(bytes) != expected_checksum {
        return Err(invalid("obfuscated payload checksum mismatch"));
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| invalid("obfuscated payload is not UTF-8"))
}

/// Legacy values need rewriting; malformed current values must still fail decoding.
pub(crate) fn needs_migration(encoded: &str) -> bool {
    !encoded.starts_with(OBFUSCATED_PREFIX)
}

fn xor(bytes: &mut [u8]) {
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte ^= XOR_KEY[index % XOR_KEY.len()];
    }
}

// FNV-1a is merely an accidental-corruption check, not authentication.
fn checksum(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c9dc5, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(0x01000193)
    })
}

fn invalid(message: &str) -> Error {
    Error::Config(format!("credential storage: {message}"))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 15) as usize] as char);
    }
    result
}

fn hex_decode(value: &str) -> Result<Vec<u8>> {
    if value.is_empty() || value.len() % 2 != 0 {
        return Err(invalid("invalid hexadecimal payload"));
    }
    fn digit(byte: u8) -> Result<u8> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            b'A'..=b'F' => Ok(byte - b'A' + 10),
            _ => Err(invalid("invalid hexadecimal payload")),
        }
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| Ok((digit(pair[0])? << 4) | digit(pair[1])?))
        .collect()
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ptr;
    use windows_sys::Win32::Foundation::LocalFree;
    #[cfg(test)]
    use windows_sys::Win32::Security::Cryptography::CryptProtectData;
    use windows_sys::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    // DPAPI owns this allocation. Keep the guard alive even on UTF-8 errors.
    struct LocalBlob(CRYPT_INTEGER_BLOB);

    impl Drop for LocalBlob {
        fn drop(&mut self) {
            if !self.0.pbData.is_null() {
                // SAFETY: DPAPI output is allocated with LocalAlloc.
                unsafe { LocalFree(self.0.pbData.cast()) };
            }
        }
    }

    pub(super) fn transform(bytes: &[u8], _encrypt: bool) -> Result<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(bytes.len())
                .map_err(|_| invalid("payload exceeds DPAPI size limit"))?,
            pbData: bytes.as_ptr() as *mut u8,
        };
        let mut output = LocalBlob(CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        });
        // SAFETY: input lives throughout the call; output is freed by LocalBlob.
        // No machine flag, entropy, description, or interactive prompt is used.
        let success = unsafe {
            #[cfg(test)]
            if _encrypt {
                CryptProtectData(
                    &input,
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output.0,
                )
            } else {
                CryptUnprotectData(
                    &input,
                    ptr::null_mut(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output.0,
                )
            }
            #[cfg(not(test))]
            CryptUnprotectData(
                &input,
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.0,
            )
        };
        if success == 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        if output.0.cbData == 0 {
            return Ok(Vec::new());
        }
        if output.0.pbData.is_null() {
            return Err(invalid("DPAPI returned an invalid buffer"));
        }
        // SAFETY: successful DPAPI call provides cbData initialized bytes.
        Ok(unsafe {
            std::slice::from_raw_parts(output.0.pbData, output.0.cbData as usize).to_vec()
        })
    }

    pub(super) fn unprotect(encoded: &str) -> Result<String> {
        let payload = encoded
            .strip_prefix(DPAPI_PREFIX)
            .ok_or_else(|| invalid("unsupported credential scheme or version"))?;
        let decrypted = transform(&hex_decode(payload)?, false).map_err(|_| {
            invalid("legacy DPAPI credential could not be decrypted; open and save it under the original Windows account to migrate, or re-enter the credential")
        })?;
        String::from_utf8(decrypted).map_err(|_| invalid("decrypted payload is not UTF-8"))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod platform {
    use super::*;
    use std::process::{Command, Stdio};

    const SERVICE: &str = "ftpseg.credentials.v1";

    fn parse_key(encoded: &str) -> Result<&str> {
        let key = encoded
            .strip_prefix(KEYRING_PREFIX)
            .ok_or_else(|| invalid("unsupported credential scheme or version"))?;
        // Only our fixed-width identifier is accepted; never command syntax.
        if key.len() != 58
            || key.as_bytes()[32] != b'-'
            || key.as_bytes()[41] != b'-'
            || !key.bytes().enumerate().all(|(i, b)| {
                if i == 32 || i == 41 {
                    b == b'-'
                } else {
                    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
                }
            })
        {
            return Err(invalid("invalid OS keyring identifier"));
        }
        Ok(key)
    }

    fn keyring_failure(operation: &str) -> Error {
        // Do not echo subprocess diagnostics: they may include secret input.
        Error::Config(format!(
            "credential storage: OS keyring {operation} failed; ensure the system keyring is installed, available, and unlocked"
        ))
    }

    #[cfg(target_os = "linux")]
    fn lookup(key: &str) -> Result<String> {
        let output = Command::new("secret-tool")
            .args(["lookup", "service", SERVICE, "key", key])
            .stdin(Stdio::null())
            .output()
            .map_err(|_| keyring_failure("launch"))?;
        if !output.status.success() {
            return Err(keyring_failure("lookup"));
        }
        let mut value = String::from_utf8(output.stdout)
            .map_err(|_| invalid("keyring payload is not UTF-8"))?;
        // secret-tool adds precisely one line terminator to the secret.
        if value.ends_with('\n') {
            value.pop();
        }
        decode_transport(&value)
    }

    #[cfg(target_os = "macos")]
    fn lookup(key: &str) -> Result<String> {
        let output = Command::new("/usr/bin/security")
            .args(["find-generic-password", "-a", key, "-s", SERVICE, "-w"])
            .stdin(Stdio::null())
            .output()
            .map_err(|_| keyring_failure("launch"))?;
        if !output.status.success() {
            return Err(keyring_failure("lookup"));
        }
        let mut value = String::from_utf8(output.stdout)
            .map_err(|_| invalid("keychain transport is not UTF-8"))?;
        if value.ends_with('\n') {
            value.pop();
        }
        decode_transport(&value)
    }

    fn decode_transport(value: &str) -> Result<String> {
        let hex = value
            .strip_prefix("utf8:")
            .ok_or_else(|| invalid("invalid keychain transport format"))?;
        if hex.is_empty() {
            return Ok(String::new());
        }
        String::from_utf8(hex_decode(hex)?).map_err(|_| invalid("keychain payload is not UTF-8"))
    }

    pub(super) fn unprotect(encoded: &str) -> Result<String> {
        lookup(parse_key(encoded)?)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn utf8_transport_preserves_special_characters() {
            for plaintext in ["", "用户\0密码\n\"'🦀", "line\n\n"] {
                let encoded = format!("utf8:{}", hex_encode(plaintext.as_bytes()));
                assert_eq!(decode_transport(&encoded).unwrap(), plaintext);
            }
            for bad in ["plaintext", "utf8:0", "utf8:gg", "utf8:ff"] {
                assert!(decode_transport(bad).is_err());
            }
        }

        #[test]
        fn legacy_identifiers_are_strictly_validated() {
            let key = "00000000000000000000000000000000-00000000-0000000000000000";
            assert_eq!(parse_key(&format!("{KEYRING_PREFIX}{key}")).unwrap(), key);
            for bad in [
                "",
                "os-keyring:v1:",
                "os-keyring:v2:abc",
                "dpapi:v1:00",
                "os-keyring:v1:a;command",
            ] {
                assert!(parse_key(bad).is_err());
            }
        }
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod platform {
    use super::*;

    pub(super) fn unprotect(_: &str) -> Result<String> {
        Err(Error::Unsupported(
            "OS-protected credential storage on this platform".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_hex_codec() {
        assert_eq!(hex_encode(&[0, 1, 15, 255]), "00010fff");
        assert_eq!(hex_decode("00010FfF").unwrap(), [0, 1, 15, 255]);
        for malformed in ["", "0", "000", "gg", "00 0", " 00", "00\n", "é"] {
            assert!(hex_decode(malformed).is_err(), "accepted {malformed:?}");
        }
    }

    #[test]
    fn reject_unknown_schemes_and_versions() {
        for value in [
            "",
            "plaintext",
            "plain:v1:secret",
            "dpapi:v2:00",
            "os-keyring:v2:key",
        ] {
            assert!(unprotect(value).is_err());
        }
    }

    #[test]
    fn portable_utf8_roundtrip_is_deterministic() {
        for plaintext in ["", "用户\0密码\n\"'🦀", "päss密碼", "line\n\n"] {
            let first = protect(plaintext).unwrap();
            assert!(first.starts_with(OBFUSCATED_PREFIX));
            assert_eq!(first, protect(plaintext).unwrap());
            assert_eq!(unprotect(&first).unwrap(), plaintext);
            assert!(!needs_migration(&first));
        }
        // Fixed fixture pins encoding, byte order, key, and checksum across OSes.
        assert_eq!(
            protect("").unwrap(),
            "obfuscated:v1:4654504d5554494ce8f27ee7"
        );
    }

    #[test]
    fn migration_only_applies_to_non_current_prefixes() {
        for legacy in ["dpapi:v1:00", "os-keyring:v1:key", "", "obfuscated:v2:00"] {
            assert!(needs_migration(legacy));
        }
        assert!(!needs_migration("obfuscated:v1:"));
    }

    #[test]
    fn reject_malformed_and_corrupted_obfuscation() {
        for bad in [
            "obfuscated:v1:",
            "obfuscated:v1:0",
            "obfuscated:v1:zz",
            "obfuscated:v1:00",
            "obfuscated:v2:00",
        ] {
            assert!(unprotect(bad).is_err(), "accepted {bad:?}");
        }
        let encoded = protect("credential").unwrap();
        let bytes = hex_decode(encoded.strip_prefix(OBFUSCATED_PREFIX).unwrap()).unwrap();
        // Corruption in length, checksum and body is detected.
        for index in [0, 8, HEADER_LEN] {
            let mut corrupted = bytes.clone();
            corrupted[index] ^= 1;
            assert!(unprotect(&format!("{OBFUSCATED_PREFIX}{}", hex_encode(&corrupted))).is_err());
        }
        let mut truncated = bytes.clone();
        truncated.pop();
        assert!(unprotect(&format!("{OBFUSCATED_PREFIX}{}", hex_encode(&truncated))).is_err());
        let mut invalid_utf8 = Vec::new();
        invalid_utf8.extend_from_slice(&1u64.to_le_bytes());
        invalid_utf8.extend_from_slice(&checksum(&[0xff]).to_le_bytes());
        invalid_utf8.push(0xff);
        xor(&mut invalid_utf8);
        assert!(unprotect(&format!("{OBFUSCATED_PREFIX}{}", hex_encode(&invalid_utf8))).is_err());
    }

    #[cfg(windows)]
    fn legacy_fixture(plaintext: &str) -> String {
        format!(
            "{DPAPI_PREFIX}{}",
            hex_encode(&platform::transform(plaintext.as_bytes(), true).unwrap())
        )
    }

    #[cfg(windows)]
    #[test]
    fn legacy_windows_values_remain_readable_for_migration() {
        for plaintext in ["", "用户\0密码\n\"'🦀"] {
            let legacy = legacy_fixture(plaintext);
            assert!(needs_migration(&legacy));
            assert_eq!(unprotect(&legacy).unwrap(), plaintext);
            let migrated = protect(&unprotect(&legacy).unwrap()).unwrap();
            assert!(!needs_migration(&migrated));
            assert_eq!(unprotect(&migrated).unwrap(), plaintext);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_reject_tampered_ciphertext_and_invalid_formats() {
        let protected = legacy_fixture("a secret credential");
        let mut bytes = hex_decode(protected.strip_prefix(DPAPI_PREFIX).unwrap()).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x80;
        assert!(unprotect(&format!("{DPAPI_PREFIX}{}", hex_encode(&bytes))).is_err());
        for bad in [
            "dpapi:v1:",
            "dpapi:v1:0",
            "dpapi:v1:zz",
            "dpapi:v1:00",
            "dpapi:v1:00\n",
            "dpapi:v1:00:11",
            "os-keyring:v1:abc",
        ] {
            assert!(unprotect(bad).is_err(), "accepted {bad:?}");
        }
    }
}
