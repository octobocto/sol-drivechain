use bitcoin::hashes::{sha256, Hash};
use solana_sdk::pubkey::Pubkey;

/// The number of checksum bytes that the address carries, in hex.
const CHECKSUM_BYTES: usize = 3;

#[derive(Debug, thiserror::Error)]
pub enum AddressError {
    #[error("the base58 body does not decode")]
    Base58(#[from] bs58::decode::Error),
    #[error("the body decodes to {0} bytes, but a pubkey is 32 bytes")]
    WrongPubkeyLength(usize),
}

/// Builds the deposit address that a user copies for a Solana pubkey.
///
/// The form is `s<slot>_<base58 pubkey>_<checksum>`. The checksum is the first
/// three bytes of the SHA-256 of everything before it, in hex. The OP_RETURN
/// carries only the bare pubkey.
pub fn format_deposit_address(slot: u8, pubkey: &Pubkey) -> String {
    let prefix = format!("s{slot}_{}_", bs58::encode(pubkey.to_bytes()).into_string());
    let checksum = checksum_of(&prefix);
    format!("{prefix}{checksum}")
}

/// Reads the Solana pubkey out of a deposit OP_RETURN, a bare base58 pubkey.
pub fn parse_deposit_recipient(text: &str) -> Result<Pubkey, AddressError> {
    let bytes = bs58::decode(text).into_vec()?;
    let bytes: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| AddressError::WrongPubkeyLength(bytes.len()))?;
    Ok(Pubkey::new_from_array(bytes))
}

fn checksum_of(prefix: &str) -> String {
    let digest = sha256::Hash::hash(prefix.as_bytes());
    crate::hex::encode(&digest.to_byte_array()[..CHECKSUM_BYTES])
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLOT: u8 = 8;

    fn a_pubkey() -> Pubkey {
        Pubkey::new_from_array([7u8; 32])
    }

    #[test]
    fn the_address_stays_under_eighty_bytes() {
        let address = format_deposit_address(SLOT, &a_pubkey());
        assert!(
            address.len() <= 80,
            "the address is {} bytes",
            address.len()
        );
    }

    #[test]
    fn the_address_holds_the_slot_the_pubkey_and_a_checksum() {
        let pubkey = a_pubkey();
        let address = format_deposit_address(SLOT, &pubkey);
        let parts: Vec<&str> = address.split('_').collect();
        assert_eq!(parts[0], "s8");
        assert_eq!(parts[1], pubkey.to_string());
        assert_eq!(parts[2], checksum_of(&format!("s8_{pubkey}_")));
        assert_eq!(parts[2].len(), CHECKSUM_BYTES * 2);
    }

    #[test]
    fn a_bare_pubkey_parses() {
        let pubkey = a_pubkey();
        assert_eq!(
            parse_deposit_recipient(&pubkey.to_string()).unwrap(),
            pubkey
        );
    }

    #[test]
    fn the_full_address_fails() {
        let address = format_deposit_address(SLOT, &a_pubkey());
        let error = parse_deposit_recipient(&address).unwrap_err();
        assert!(matches!(error, AddressError::Base58(_)));
    }

    #[test]
    fn a_short_pubkey_fails() {
        let text = bs58::encode([1u8; 16]).into_string();
        let error = parse_deposit_recipient(&text).unwrap_err();
        assert!(matches!(error, AddressError::WrongPubkeyLength(16)));
    }
}
