use bip39::Mnemonic;
use ed25519_dalek_bip32::{ChildIndex, DerivationPath, ExtendedSigningKey};
use solana_sdk::{pubkey::Pubkey, signature::Keypair};

/// BIP44 names Solana as coin type 501.
const SOLANA_COIN_TYPE: u32 = 501;

#[derive(Debug, thiserror::Error)]
pub enum SeedError {
    #[error("the seed phrase does not parse")]
    BadMnemonic(#[source] bip39::Error),
    #[error("the derivation failed")]
    Derive(#[source] ed25519_dalek_bip32::Error),
    #[error("the account number {0} is above the BIP32 limit")]
    AccountTooLarge(u32),
}

/// The path that `solana-keygen` and every Solana wallet use.
///
/// `m/44'/501'/<account>'/0'`. Every step is hardened, because ed25519 knows
/// no other kind.
pub fn derivation_path(account: u32) -> Result<DerivationPath, SeedError> {
    let hardened =
        |index: u32| ChildIndex::hardened(index).map_err(|_| SeedError::AccountTooLarge(index));
    Ok(DerivationPath::new([
        hardened(44)?,
        hardened(SOLANA_COIN_TYPE)?,
        hardened(account)?,
        hardened(0)?,
    ]))
}

/// Builds the Solana keypair of one account from a BIP39 seed phrase.
///
/// BitWindow holds one seed phrase for every sidechain. This is how a wallet
/// on this chain comes back from that one phrase.
pub fn keypair_from_mnemonic(
    phrase: &str,
    passphrase: &str,
    account: u32,
) -> Result<Keypair, SeedError> {
    let mnemonic = Mnemonic::parse_normalized(phrase.trim()).map_err(SeedError::BadMnemonic)?;
    let seed = mnemonic.to_seed_normalized(passphrase);
    keypair_from_seed(&seed, account)
}

/// Builds the Solana keypair of one account from a 64-byte BIP39 seed.
pub fn keypair_from_seed(seed: &[u8; 64], account: u32) -> Result<Keypair, SeedError> {
    let master = ExtendedSigningKey::from_seed(seed).map_err(SeedError::Derive)?;
    let derived = master
        .derive(&derivation_path(account)?)
        .map_err(SeedError::Derive)?;
    Ok(Keypair::new_from_array(derived.signing_key.to_bytes()))
}

/// The address of one account.
pub fn pubkey_from_mnemonic(
    phrase: &str,
    passphrase: &str,
    account: u32,
) -> Result<Pubkey, SeedError> {
    use solana_sdk::signer::Signer as _;

    Ok(keypair_from_mnemonic(phrase, passphrase, account)?.pubkey())
}

/// The 64 bytes that a Solana keypair file holds: the secret, then the public.
pub fn keypair_file_bytes(keypair: &Keypair) -> Vec<u8> {
    keypair.to_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signer::Signer as _;

    /// The BIP39 test phrase. It is public, so it holds no money.
    const A_TEST_PHRASE: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    /// `solana-keygen recover 'prompt://?key=0/0'` answered with this address
    /// for the phrase above and an empty passphrase.
    const ACCOUNT_0: &str = "HAgk14JpMQLgt6rVgv7cBQFJWFto5Dqxi472uT3DKpqk";
    const ACCOUNT_1: &str = "Hh8QwFUA6MtVu1qAoq12ucvFHNwCcVTV7hpWjeY1Hztb";

    #[test]
    fn account_zero_matches_solana_keygen() {
        let pubkey = pubkey_from_mnemonic(A_TEST_PHRASE, "", 0).unwrap();
        assert_eq!(pubkey.to_string(), ACCOUNT_0);
    }

    #[test]
    fn account_one_matches_solana_keygen() {
        let pubkey = pubkey_from_mnemonic(A_TEST_PHRASE, "", 1).unwrap();
        assert_eq!(pubkey.to_string(), ACCOUNT_1);
    }

    #[test]
    fn each_account_gives_another_address() {
        let first = pubkey_from_mnemonic(A_TEST_PHRASE, "", 0).unwrap();
        let second = pubkey_from_mnemonic(A_TEST_PHRASE, "", 1).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn one_phrase_always_gives_the_same_address() {
        let first = pubkey_from_mnemonic(A_TEST_PHRASE, "", 7).unwrap();
        let second = pubkey_from_mnemonic(A_TEST_PHRASE, "", 7).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn a_passphrase_gives_another_address() {
        let bare = pubkey_from_mnemonic(A_TEST_PHRASE, "", 0).unwrap();
        let guarded = pubkey_from_mnemonic(A_TEST_PHRASE, "a passphrase", 0).unwrap();
        assert_ne!(bare, guarded);
    }

    #[test]
    fn extra_spaces_do_not_change_the_address() {
        let padded = format!("  {A_TEST_PHRASE}  ");
        assert_eq!(
            pubkey_from_mnemonic(&padded, "", 0).unwrap().to_string(),
            ACCOUNT_0
        );
    }

    #[test]
    fn a_phrase_that_is_no_mnemonic_fails() {
        let error = pubkey_from_mnemonic("not a seed phrase", "", 0).unwrap_err();
        assert!(matches!(error, SeedError::BadMnemonic(_)));
    }

    #[test]
    fn a_wrong_checksum_fails() {
        let wrong = A_TEST_PHRASE.replace("about", "abandon");
        assert!(pubkey_from_mnemonic(&wrong, "", 0).is_err());
    }

    #[test]
    fn the_path_names_coin_type_501() {
        let path = derivation_path(3).unwrap();
        let steps: Vec<ChildIndex> = path.path().to_vec();
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0], ChildIndex::hardened(44).unwrap());
        assert_eq!(steps[1], ChildIndex::hardened(501).unwrap());
        assert_eq!(steps[2], ChildIndex::hardened(3).unwrap());
        assert_eq!(steps[3], ChildIndex::hardened(0).unwrap());
    }

    #[test]
    fn a_keypair_file_holds_sixty_four_bytes() {
        let keypair = keypair_from_mnemonic(A_TEST_PHRASE, "", 0).unwrap();
        let bytes = keypair_file_bytes(&keypair);
        assert_eq!(bytes.len(), 64);
        assert_eq!(&bytes[32..], keypair.pubkey().as_ref());
    }
}
