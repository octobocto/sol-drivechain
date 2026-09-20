use bitcoin::hashes::{sha256, Hash};
use solana_sdk::pubkey::Pubkey;

/// The number of checksum bytes that the address carries, in hex.
const CHECKSUM_BYTES: usize = 3;

#[derive(Debug, thiserror::Error)]
pub enum AddressError {
    #[error("the address does not start with `s`")]
    MissingPrefix,
    #[error("the address does not hold three parts separated by `_`")]
    WrongPartCount,
    #[error("the slot `{0}` is not a number")]
    SlotNotANumber(String),
    #[error("the address names slot {found}, but this sidechain holds slot {want}")]
    WrongSlot { want: u8, found: u8 },
    #[error("the base58 body does not decode")]
    Base58(#[from] bs58::decode::Error),
    #[error("the body decodes to {0} bytes, but a pubkey is 32 bytes")]
    WrongPubkeyLength(usize),
    #[error("the checksum is `{found}`, but it must be `{want}`")]
    WrongChecksum { want: String, found: String },
}

/// Builds the mainchain deposit address for a Solana pubkey.
///
/// The form is `s<slot>_<base58 pubkey>_<checksum>`. The checksum is the first
/// three bytes of the SHA-256 of everything before it, in hex.
pub fn format_deposit_address(slot: u8, pubkey: &Pubkey) -> String {
    let prefix = format!("s{slot}_{}_", bs58::encode(pubkey.to_bytes()).into_string());
    let checksum = checksum_of(&prefix);
    format!("{prefix}{checksum}")
}

/// Reads a Solana pubkey out of a mainchain deposit address.
pub fn parse_deposit_address(slot: u8, address: &str) -> Result<Pubkey, AddressError> {
    let body = address
        .strip_prefix('s')
        .ok_or(AddressError::MissingPrefix)?;
    let mut parts = body.split('_');
    let (Some(found_slot), Some(base58), Some(found_checksum), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(AddressError::WrongPartCount);
    };

    let found_slot: u8 = found_slot
        .parse()
        .map_err(|_| AddressError::SlotNotANumber(found_slot.to_owned()))?;
    if found_slot != slot {
        return Err(AddressError::WrongSlot {
            want: slot,
            found: found_slot,
        });
    }

    let want_checksum = checksum_of(&format!("s{found_slot}_{base58}_"));
    if found_checksum != want_checksum {
        return Err(AddressError::WrongChecksum {
            want: want_checksum,
            found: found_checksum.to_owned(),
        });
    }

    let bytes = bs58::decode(base58).into_vec()?;
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
    fn a_formatted_address_parses_back() {
        let pubkey = a_pubkey();
        let address = format_deposit_address(SLOT, &pubkey);
        assert_eq!(parse_deposit_address(SLOT, &address).unwrap(), pubkey);
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
    fn the_address_holds_the_slot_and_a_six_character_checksum() {
        let address = format_deposit_address(SLOT, &a_pubkey());
        let parts: Vec<&str> = address.split('_').collect();
        assert_eq!(parts[0], "s8");
        assert_eq!(parts[2].len(), CHECKSUM_BYTES * 2);
    }

    #[test]
    fn another_slot_fails() {
        let address = format_deposit_address(SLOT, &a_pubkey());
        let error = parse_deposit_address(SLOT + 1, &address).unwrap_err();
        assert!(matches!(error, AddressError::WrongSlot { .. }));
    }

    #[test]
    fn a_changed_body_fails_the_checksum() {
        let address = format_deposit_address(SLOT, &a_pubkey());
        let mut parts: Vec<String> = address.split('_').map(str::to_owned).collect();
        parts[1] = bs58::encode([8u8; 32]).into_string();
        let error = parse_deposit_address(SLOT, &parts.join("_")).unwrap_err();
        assert!(matches!(error, AddressError::WrongChecksum { .. }));
    }

    #[test]
    fn a_missing_prefix_fails() {
        let error = parse_deposit_address(SLOT, "8_abc_000000").unwrap_err();
        assert!(matches!(error, AddressError::MissingPrefix));
    }

    #[test]
    fn a_short_body_fails() {
        let base58 = bs58::encode([1u8; 16]).into_string();
        let prefix = format!("s{SLOT}_{base58}_");
        let address = format!("{prefix}{}", checksum_of(&prefix));
        let error = parse_deposit_address(SLOT, &address).unwrap_err();
        assert!(matches!(error, AddressError::WrongPubkeyLength(16)));
    }

    #[test]
    fn a_fourth_part_fails() {
        let error = parse_deposit_address(SLOT, "s8_abc_000000_extra").unwrap_err();
        assert!(matches!(error, AddressError::WrongPartCount));
    }
}
