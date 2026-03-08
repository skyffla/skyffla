use anyhow::Result;
use ed25519_dalek::SigningKey;
use rand_core::OsRng;
use sha2::{Digest as ShaDigestTrait, Sha256};

use crate::local_state::{load_local_state, save_local_state};

pub(crate) struct LocalIdentity {
    pub(crate) fingerprint: String,
}

pub(crate) fn load_or_create_identity(
    state_path: &Option<std::path::PathBuf>,
) -> Result<LocalIdentity> {
    let mut state = load_local_state(state_path);

    if let Some(secret_hex) = state.local_identity_secret_hex.as_deref() {
        if let Some(identity) = parse_identity(secret_hex) {
            return Ok(identity);
        }
    }

    let signing_key = SigningKey::generate(&mut OsRng);
    state.local_identity_secret_hex = Some(hex::encode(signing_key.to_bytes()));
    save_local_state(state_path, &state);
    Ok(identity_from_signing_key(&signing_key))
}

fn parse_identity(secret_hex: &str) -> Option<LocalIdentity> {
    let secret_bytes = hex::decode(secret_hex).ok()?;
    let secret_bytes: [u8; 32] = secret_bytes.try_into().ok()?;
    let signing_key = SigningKey::from_bytes(&secret_bytes);
    Some(identity_from_signing_key(&signing_key))
}

fn identity_from_signing_key(signing_key: &SigningKey) -> LocalIdentity {
    let public_key = signing_key.verifying_key();
    let digest = Sha256::digest(public_key.as_bytes());
    LocalIdentity {
        fingerprint: hex::encode(&digest[..16]),
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::{identity_from_signing_key, load_or_create_identity};
    use crate::local_state::LocalState;

    #[test]
    fn identity_fingerprint_is_stable_for_same_secret() {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[7_u8; 32]);
        let identity = identity_from_signing_key(&signing_key);

        assert_eq!(identity.fingerprint, "fe812c12f3ab4ce6ac5db69ac352f906");
    }

    #[test]
    fn identity_is_persisted_in_local_state() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "skyffla-identity-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let path_opt = Some(path.clone());

        let first = load_or_create_identity(&path_opt)?;
        let second = load_or_create_identity(&path_opt)?;
        let state: LocalState = serde_json::from_str(&std::fs::read_to_string(&path)?)?;

        assert_eq!(first.fingerprint, second.fingerprint);
        assert!(state.local_identity_secret_hex.is_some());

        let _ = std::fs::remove_file(path);
        Ok(())
    }
}
