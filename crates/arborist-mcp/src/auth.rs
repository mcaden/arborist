use std::io;

use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("failed to read current executable: {0}")]
    ReadBinary(io::Error),
    #[error("expected sidecar hash must be valid hex: {0}")]
    InvalidExpectedHex(hex::FromHexError),
    #[error("expected sidecar hash must decode to 32 bytes, got {0}")]
    InvalidExpectedLength(usize),
    #[error("sidecar hash mismatch")]
    HashMismatch { expected: [u8; 32], actual: [u8; 32] },
}

pub fn compute_self_hash() -> io::Result<[u8; 32]> {
    let exe = std::env::current_exe()?;
    let bytes = std::fs::read(exe)?;
    let digest = Sha256::digest(bytes);

    let mut out = [0_u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

pub fn verify_against(expected_hex: &str, allow_mismatch: bool) -> Result<(), AuthError> {
    let actual = compute_self_hash().map_err(AuthError::ReadBinary)?;
    let expected_vec = hex::decode(expected_hex).map_err(AuthError::InvalidExpectedHex)?;
    if expected_vec.len() != actual.len() {
        return Err(AuthError::InvalidExpectedLength(expected_vec.len()));
    }

    let mut expected = [0_u8; 32];
    expected.copy_from_slice(&expected_vec);

    if actual == expected {
        return Ok(());
    }

    if allow_mismatch {
        tracing::warn!(
            expected = %hex::encode(expected),
            actual = %hex::encode(actual),
            "sidecar hash mismatch allowed by dev mode"
        );
        return Ok(());
    }

    Err(AuthError::HashMismatch { expected, actual })
}

#[cfg(test)]
mod tests {
    use super::{compute_self_hash, verify_against, AuthError};

    #[test]
    fn verify_against_accepts_the_current_binary_hash() {
        let expected = hex::encode(compute_self_hash().expect("current binary hash should compute"));
        assert!(verify_against(&expected, false).is_ok());
    }

    #[test]
    fn verify_against_rejects_a_mismatching_hash() {
        let mut mismatching = compute_self_hash().expect("current binary hash should compute");
        mismatching[0] ^= 0xFF;

        let error = verify_against(&hex::encode(mismatching), false).expect_err("mismatched hash should fail");
        assert!(matches!(error, AuthError::HashMismatch { .. }));
    }

    #[test]
    fn verify_against_can_allow_mismatch_for_dev_mode() {
        let mut mismatching = compute_self_hash().expect("current binary hash should compute");
        mismatching[0] ^= 0xFF;

        assert!(verify_against(&hex::encode(mismatching), true).is_ok());
    }
}
