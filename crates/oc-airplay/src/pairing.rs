use crate::invalid;
use aes::Aes128;
use ctr::cipher::{KeyIvInit, StreamCipher};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand_core::OsRng;
use sha2::{Digest, Sha512};
use std::io;
use x25519_dalek::{PublicKey, StaticSecret};

type Cipher = ctr::Ctr128BE<Aes128>;

pub(crate) struct Identity {
    key: SigningKey,
}

impl Identity {
    pub fn new() -> Self {
        Self {
            key: SigningKey::generate(&mut OsRng),
        }
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    pub fn device_id(&self) -> String {
        let digest = Sha512::digest(self.public_key());
        let mut bytes: [u8; 6] = digest[..6].try_into().unwrap();
        bytes[0] = (bytes[0] & 0xfc) | 0x02;
        bytes
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    }
}

#[derive(Default)]
pub(crate) struct Pairing {
    pending: Option<Pending>,
    pub verified: bool,
}

struct Pending {
    cipher: Cipher,
    peer_key: VerifyingKey,
    message: [u8; 64],
}

impl Pairing {
    pub fn exchange(&mut self, identity: &Identity, body: &[u8]) -> io::Result<Vec<u8>> {
        if body.len() != 68 || body[1..4] != [0; 3] {
            self.pending = None;
            self.verified = false;
            return Err(invalid("Legacy pair-verify requires a 68-byte record"));
        }
        match body[0] {
            1 => {
                self.pending = None;
                self.verified = false;
                let peer_curve = PublicKey::from(<[u8; 32]>::try_from(&body[4..36]).unwrap());
                let peer_key = VerifyingKey::from_bytes(body[36..].try_into().unwrap())
                    .map_err(|_| invalid("Invalid Ed25519 public key"))?;
                let secret = StaticSecret::random_from_rng(OsRng);
                let own_curve = PublicKey::from(&secret);
                let shared = secret.diffie_hellman(&peer_curve);
                if !shared.was_contributory() {
                    return Err(invalid("Non-contributory X25519 public key"));
                }
                let derive = |label: &[u8]| -> [u8; 16] {
                    let mut hash = Sha512::new();
                    hash.update(label);
                    hash.update(shared.as_bytes());
                    hash.finalize()[..16].try_into().unwrap()
                };
                let mut cipher = Cipher::new(
                    &derive(b"Pair-Verify-AES-Key").into(),
                    &derive(b"Pair-Verify-AES-IV").into(),
                );
                let mut message = [0; 64];
                message[..32].copy_from_slice(own_curve.as_bytes());
                message[32..].copy_from_slice(peer_curve.as_bytes());
                let mut signature = identity.key.sign(&message).to_bytes();
                cipher.apply_keystream(&mut signature);
                let mut response = own_curve.as_bytes().to_vec();
                response.extend_from_slice(&signature);
                message.rotate_left(32);
                self.pending = Some(Pending {
                    cipher,
                    peer_key,
                    message,
                });
                Ok(response)
            }
            0 => {
                self.verified = false;
                let mut pending = self
                    .pending
                    .take()
                    .ok_or_else(|| invalid("pair-verify stage one is missing"))?;
                let mut bytes: [u8; 64] = body[4..].try_into().unwrap();
                pending.cipher.apply_keystream(&mut bytes);
                pending
                    .peer_key
                    .verify_strict(&pending.message, &Signature::from_bytes(&bytes))
                    .map_err(|_| invalid("Client pair-verify signature is invalid"))?;
                self.verified = true;
                Ok(Vec::new())
            }
            _ => {
                self.pending = None;
                self.verified = false;
                Err(invalid("Unknown pair-verify stage"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_exchange(corrupt: bool) -> (Pairing, io::Result<Vec<u8>>) {
        let identity = Identity::new();
        let client_key = SigningKey::from_bytes(&[7; 32]);
        let client_curve = StaticSecret::from([9; 32]);
        let client_public = PublicKey::from(&client_curve);
        let mut first = vec![1, 0, 0, 0];
        first.extend_from_slice(client_public.as_bytes());
        first.extend_from_slice(client_key.verifying_key().as_bytes());
        let mut pairing = Pairing::default();
        let response = pairing.exchange(&identity, &first).unwrap();
        assert_eq!(response.len(), 96);
        let server_public = PublicKey::from(<[u8; 32]>::try_from(&response[..32]).unwrap());
        let shared = client_curve.diffie_hellman(&server_public);
        let key = Sha512::digest([b"Pair-Verify-AES-Key".as_slice(), shared.as_bytes()].concat());
        let iv = Sha512::digest([b"Pair-Verify-AES-IV".as_slice(), shared.as_bytes()].concat());
        let mut cipher = Cipher::new(key[..16].into(), iv[..16].into());
        let mut signature: [u8; 64] = response[32..].try_into().unwrap();
        cipher.apply_keystream(&mut signature);
        let server_message = [
            server_public.as_bytes().as_slice(),
            client_public.as_bytes(),
        ]
        .concat();
        identity
            .key
            .verifying_key()
            .verify_strict(&server_message, &Signature::from_bytes(&signature))
            .unwrap();
        let client_message = [
            client_public.as_bytes().as_slice(),
            server_public.as_bytes(),
        ]
        .concat();
        let mut signature = client_key.sign(&client_message).to_bytes();
        if corrupt {
            signature[0] ^= 1;
        }
        cipher.apply_keystream(&mut signature);
        let mut second = vec![0; 4];
        second.extend_from_slice(&signature);
        let result = pairing.exchange(&identity, &second);
        (pairing, result)
    }

    #[test]
    fn both_signatures_and_continuous_ctr_are_verified() {
        let (pairing, result) = client_exchange(false);
        assert_eq!(result.unwrap(), Vec::<u8>::new());
        assert!(pairing.verified);
    }

    #[test]
    fn forged_client_signature_fails() {
        let (pairing, result) = client_exchange(true);
        assert!(result.is_err());
        assert!(!pairing.verified);
    }

    #[test]
    fn missing_first_stage_bad_lengths_and_low_order_keys_fail() {
        let identity = Identity::new();
        let mut pairing = Pairing::default();
        assert!(pairing.exchange(&identity, &[0; 68]).is_err());
        for size in [0, 4, 32, 67, 69, 1024] {
            assert!(pairing.exchange(&identity, &vec![1; size]).is_err());
        }
        let mut first = vec![1, 0, 0, 0];
        first.extend_from_slice(&[0; 32]);
        first.extend_from_slice(SigningKey::from_bytes(&[5; 32]).verifying_key().as_bytes());
        assert!(pairing.exchange(&identity, &first).is_err());
        assert!(!pairing.verified);
    }
}
