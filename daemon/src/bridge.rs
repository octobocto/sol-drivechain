use bitcoin::hashes::{sha256, Hash as _};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};

/// The System Program id is thirty-two zero bytes.
pub const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::new_from_array([0u8; 32]);

/// The peg is 1 SOL to 1 BTC, so one satoshi is ten lamports.
pub const LAMPORTS_PER_SAT: u64 = 10;

pub const CONFIG_SEED: &[u8] = b"config";
pub const VAULT_SEED: &[u8] = b"vault";
pub const WITHDRAWAL_SEED: &[u8] = b"withdrawal";
pub const TREASURY_SEED: &[u8] = b"treasury";

/// The account that the patched validator builds for each settle. It must
/// match `BMM_ANSWER_ID` in the Agave patch and the bridge program.
pub const BMM_ANSWER_ID: Pubkey =
    Pubkey::from_str_const("BmmAnswer1111111111111111111111111111111111");

/// The account that the patched validator builds for each credit. It must
/// match `DEPOSIT_ANSWER_ID` in the Agave patch and the bridge program.
pub const DEPOSIT_ANSWER_ID: Pubkey =
    Pubkey::from_str_const("DepositAnswer111111111111111111111111111111");

/// The block record of the patched validator: the bank hash of the last block
/// of each stride of slots on the fork. A settle pays only a commitment to a
/// block in it. It must match `BMM_BLOCKS_ID` in the Agave patch.
pub const BMM_BLOCKS_ID: Pubkey =
    Pubkey::from_str_const("BmmB1ocks1111111111111111111111111111111111");

/// One record entry: the slot, then the bank hash.
const BLOCK_RECORD_ENTRY_LEN: usize = 8 + 32;

/// The newest block in a block record: its slot and its bank hash.
pub fn newest_recorded_block(record: &[u8]) -> Option<(u64, [u8; 32])> {
    record
        .chunks_exact(BLOCK_RECORD_ENTRY_LEN)
        .filter_map(|entry| {
            let (slot, hash) = entry.split_first_chunk::<8>()?;
            let hash: [u8; 32] = hash.try_into().ok()?;
            (hash != [0u8; 32]).then_some((u64::from_le_bytes(*slot), hash))
        })
        .max_by_key(|(slot, _)| *slot)
}

/// h* of a BMM bid: SHA-256(Solana bank hash ‖ payee).
pub fn commitment_of(solana_block: &[u8; 32], payee: &Pubkey) -> [u8; 32] {
    let mut bytes = solana_block.to_vec();
    bytes.extend_from_slice(payee.as_ref());
    sha256::Hash::hash(&bytes).to_byte_array()
}

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("the account holds {found} bytes, but `{name}` asks for at least {want}")]
    ShortAccount {
        name: &'static str,
        want: usize,
        found: usize,
    },
    #[error("the account discriminator does not name `{0}`")]
    WrongDiscriminator(&'static str),
    #[error("the script pubkey claims {0} bytes, but the account stops before that")]
    ScriptPubkeyOverrun(u32),
    #[error("the `{name}` account stops inside the `{field}` field")]
    TruncatedField {
        name: &'static str,
        field: &'static str,
    },
}

pub fn config_pda(program_id: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[CONFIG_SEED], program_id)
}

pub fn vault_pda(program_id: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[VAULT_SEED], program_id)
}

pub fn withdrawal_pda(program_id: &Pubkey, index: u64) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[WITHDRAWAL_SEED, &index.to_le_bytes()], program_id)
}

pub fn treasury_pda(program_id: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[TREASURY_SEED], program_id)
}

/// Anchor names an instruction or an account by the first eight bytes of
/// `sha256("<namespace>:<name>")`.
pub fn discriminator(namespace: &str, name: &str) -> [u8; 8] {
    let digest = sha256::Hash::hash(format!("{namespace}:{name}").as_bytes());
    let mut out = [0u8; 8];
    out.copy_from_slice(&digest.to_byte_array()[..8]);
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub oracle: Pubkey,
    pub deposit_high_water: u64,
    pub pegged_lamports: u64,
    pub withdrawal_count: u64,
    pub bmm_next_height: u64,
    pub bmm_paid_total: u64,
    pub deposit_confirmations: u64,
    pub deposit_lag: u64,
    pub credited_height: u64,
    pub credited_block: [u8; 32],
    pub credit_index: u64,
    pub stranded_lamports: u64,
    pub bump: u8,
    pub vault_bump: u8,
    pub treasury_bump: u8,
}

impl Config {
    pub const LEN: usize = 8 + 32 + 8 * 5 + 8 * 3 + 32 + 8 * 2 + 1 + 1 + 1;

    pub fn decode(data: &[u8]) -> Result<Self, BridgeError> {
        if data.len() < Self::LEN {
            return Err(BridgeError::ShortAccount {
                name: "Config",
                want: Self::LEN,
                found: data.len(),
            });
        }
        if data[..8] != discriminator("account", "Config") {
            return Err(BridgeError::WrongDiscriminator("Config"));
        }
        let mut reader = Reader::new(&data[8..]);
        let cut = |field| BridgeError::TruncatedField {
            name: "Config",
            field,
        };
        Ok(Self {
            oracle: Pubkey::new_from_array(reader.array32().ok_or_else(|| cut("oracle"))?),
            deposit_high_water: reader.u64().ok_or_else(|| cut("deposit_high_water"))?,
            pegged_lamports: reader.u64().ok_or_else(|| cut("pegged_lamports"))?,
            withdrawal_count: reader.u64().ok_or_else(|| cut("withdrawal_count"))?,
            bmm_next_height: reader.u64().ok_or_else(|| cut("bmm_next_height"))?,
            bmm_paid_total: reader.u64().ok_or_else(|| cut("bmm_paid_total"))?,
            deposit_confirmations: reader.u64().ok_or_else(|| cut("deposit_confirmations"))?,
            deposit_lag: reader.u64().ok_or_else(|| cut("deposit_lag"))?,
            credited_height: reader.u64().ok_or_else(|| cut("credited_height"))?,
            credited_block: reader.array32().ok_or_else(|| cut("credited_block"))?,
            credit_index: reader.u64().ok_or_else(|| cut("credit_index"))?,
            stranded_lamports: reader.u64().ok_or_else(|| cut("stranded_lamports"))?,
            bump: reader.u8().ok_or_else(|| cut("bump"))?,
            vault_bump: reader.u8().ok_or_else(|| cut("vault_bump"))?,
            treasury_bump: reader.u8().ok_or_else(|| cut("treasury_bump"))?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WithdrawalRecord {
    pub index: u64,
    pub owner: Pubkey,
    pub burned_lamports: u64,
    pub payout_sats: u64,
    pub fee_sats: u64,
    pub script_pubkey: Vec<u8>,
    pub bump: u8,
}

impl WithdrawalRecord {
    const HEAD_LEN: usize = 8 + 8 + 32 + 8 + 8 + 8 + 4;

    pub fn decode(data: &[u8]) -> Result<Self, BridgeError> {
        if data.len() < Self::HEAD_LEN + 1 {
            return Err(BridgeError::ShortAccount {
                name: "WithdrawalRecord",
                want: Self::HEAD_LEN + 1,
                found: data.len(),
            });
        }
        if data[..8] != discriminator("account", "WithdrawalRecord") {
            return Err(BridgeError::WrongDiscriminator("WithdrawalRecord"));
        }
        let mut reader = Reader::new(&data[8..]);
        let cut = |field| BridgeError::TruncatedField {
            name: "WithdrawalRecord",
            field,
        };
        let index = reader.u64().ok_or_else(|| cut("index"))?;
        let owner = Pubkey::new_from_array(reader.array32().ok_or_else(|| cut("owner"))?);
        let burned_lamports = reader.u64().ok_or_else(|| cut("burned_lamports"))?;
        let payout_sats = reader.u64().ok_or_else(|| cut("payout_sats"))?;
        let fee_sats = reader.u64().ok_or_else(|| cut("fee_sats"))?;
        let script_len = reader.u32().ok_or_else(|| cut("script_len"))?;
        let script_pubkey = reader
            .bytes(script_len as usize)
            .ok_or(BridgeError::ScriptPubkeyOverrun(script_len))?;
        let bump = reader.u8().ok_or_else(|| cut("bump"))?;
        Ok(Self {
            index,
            owner,
            burned_lamports,
            payout_sats,
            fee_sats,
            script_pubkey,
            bump,
        })
    }
}

/// What `initialize` sets besides the oracle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Start {
    pub bmm_start_height: u64,
    /// The deposits up to this eCash height count as credited.
    pub deposit_start_height: u64,
    /// D, the eCash confirmations before a credit.
    pub deposit_confirmations: u64,
    /// A BMM checkpoint at eCash height H needs credits through H - lag.
    pub deposit_lag: u64,
}

pub fn initialize_ix(
    program_id: &Pubkey,
    payer: &Pubkey,
    oracle: &Pubkey,
    start: Start,
) -> Instruction {
    let mut data = discriminator("global", "initialize").to_vec();
    data.extend_from_slice(oracle.as_ref());
    data.extend_from_slice(&start.bmm_start_height.to_le_bytes());
    data.extend_from_slice(&start.deposit_start_height.to_le_bytes());
    data.extend_from_slice(&start.deposit_confirmations.to_le_bytes());
    data.extend_from_slice(&start.deposit_lag.to_le_bytes());
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(config_pda(program_id).0, false),
            AccountMeta::new_readonly(vault_pda(program_id).0, false),
            AccountMeta::new_readonly(treasury_pda(program_id).0, false),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data,
    }
}

/// Credits the deposits of eCash block `block_hash` at `height`, from index
/// `first`, `count` of them. `targets` holds the pubkey of each deposit of
/// the part that names one, in order. `block_hash` is in the internal byte
/// order. Anyone can send it.
pub fn credit_deposits_ix(
    program_id: &Pubkey,
    height: u64,
    block_hash: &[u8; 32],
    first: u32,
    count: u8,
    targets: &[Pubkey],
) -> Instruction {
    let mut data = discriminator("global", "credit_deposits").to_vec();
    data.extend_from_slice(&height.to_le_bytes());
    data.extend_from_slice(block_hash);
    data.extend_from_slice(&first.to_le_bytes());
    data.push(count);
    let mut accounts = vec![
        AccountMeta::new(config_pda(program_id).0, false),
        AccountMeta::new(vault_pda(program_id).0, false),
        AccountMeta::new_readonly(DEPOSIT_ANSWER_ID, false),
        AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
    ];
    accounts.extend(
        targets
            .iter()
            .map(|target| AccountMeta::new(*target, false)),
    );
    Instruction {
        program_id: *program_id,
        accounts,
        data,
    }
}

pub fn withdraw_ix(
    program_id: &Pubkey,
    user: &Pubkey,
    index: u64,
    lamports: u64,
    fee_sats: u64,
    script_pubkey: &[u8],
) -> Instruction {
    let mut data = discriminator("global", "withdraw").to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    data.extend_from_slice(&fee_sats.to_le_bytes());
    data.extend_from_slice(&(script_pubkey.len() as u32).to_le_bytes());
    data.extend_from_slice(script_pubkey);
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(config_pda(program_id).0, false),
            AccountMeta::new(vault_pda(program_id).0, false),
            AccountMeta::new(withdrawal_pda(program_id, index).0, false),
            AccountMeta::new(*user, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data,
    }
}

pub fn mark_paid_ix(
    program_id: &Pubkey,
    oracle: &Pubkey,
    owner: &Pubkey,
    index: u64,
) -> Instruction {
    let mut data = discriminator("global", "mark_paid").to_vec();
    data.extend_from_slice(&index.to_le_bytes());
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new_readonly(config_pda(program_id).0, false),
            AccountMeta::new(withdrawal_pda(program_id, index).0, false),
            AccountMeta::new(*owner, false),
            AccountMeta::new_readonly(*oracle, true),
        ],
        data,
    }
}

/// Settles the commitment at eCash height `height`, which `block_hash` holds on
/// the active chain. The commitment must be SHA-256(`solana_block` ‖ `payee`).
/// `block_hash` is in the internal byte order.
pub fn settle_bmm_ix(
    program_id: &Pubkey,
    height: u64,
    block_hash: &[u8; 32],
    solana_block: &[u8; 32],
    payee: &Pubkey,
) -> Instruction {
    let mut data = discriminator("global", "settle_bmm").to_vec();
    data.extend_from_slice(&height.to_le_bytes());
    data.extend_from_slice(block_hash);
    data.extend_from_slice(solana_block);
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(config_pda(program_id).0, false),
            AccountMeta::new(treasury_pda(program_id).0, false),
            AccountMeta::new(*payee, false),
            AccountMeta::new_readonly(BMM_ANSWER_ID, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data,
    }
}

/// Reads an Anchor account body, one little-endian field at a time.
///
/// Every read answers `None` past the end. The daemon decodes bytes that a
/// Solana RPC hands it, so a short or hostile account must give an error and
/// not stop the peg.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        let end = self.at.checked_add(N)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        let mut out = [0u8; N];
        out.copy_from_slice(slice);
        Some(out)
    }

    fn u8(&mut self) -> Option<u8> {
        self.array::<1>().map(|out| out[0])
    }

    fn u32(&mut self) -> Option<u32> {
        self.array::<4>().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Option<u64> {
        self.array::<8>().map(u64::from_le_bytes)
    }

    fn array32(&mut self) -> Option<[u8; 32]> {
        self.array::<32>()
    }

    fn bytes(&mut self, len: usize) -> Option<Vec<u8>> {
        let end = self.at.checked_add(len)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_program_id() -> Pubkey {
        Pubkey::new_from_array([3u8; 32])
    }

    #[test]
    fn the_system_program_id_is_the_base58_ones() {
        assert_eq!(
            SYSTEM_PROGRAM_ID.to_string(),
            "11111111111111111111111111111111"
        );
    }

    #[test]
    fn the_discriminator_is_eight_bytes_of_sha256() {
        let want = &sha256::Hash::hash(b"global:credit_deposits").to_byte_array()[..8];
        assert_eq!(discriminator("global", "credit_deposits"), want);
    }

    #[test]
    fn each_instruction_carries_its_own_discriminator() {
        let names = [
            "initialize",
            "credit_deposits",
            "withdraw",
            "mark_paid",
            "settle_bmm",
        ];
        let mut seen: Vec<[u8; 8]> = names
            .iter()
            .map(|name| discriminator("global", name))
            .collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), names.len());
    }

    fn a_config() -> Config {
        Config {
            oracle: Pubkey::new_from_array([7u8; 32]),
            deposit_high_water: 12,
            pegged_lamports: 30_000_000,
            withdrawal_count: 2,
            bmm_next_height: 900,
            bmm_paid_total: 7_000,
            deposit_confirmations: 6,
            deposit_lag: 100,
            credited_height: 1_000,
            credited_block: [4u8; 32],
            credit_index: 2,
            stranded_lamports: 50,
            bump: 254,
            vault_bump: 253,
            treasury_bump: 252,
        }
    }

    fn a_config_account(config: &Config) -> Vec<u8> {
        let mut data = discriminator("account", "Config").to_vec();
        data.extend_from_slice(config.oracle.as_ref());
        data.extend_from_slice(&config.deposit_high_water.to_le_bytes());
        data.extend_from_slice(&config.pegged_lamports.to_le_bytes());
        data.extend_from_slice(&config.withdrawal_count.to_le_bytes());
        data.extend_from_slice(&config.bmm_next_height.to_le_bytes());
        data.extend_from_slice(&config.bmm_paid_total.to_le_bytes());
        data.extend_from_slice(&config.deposit_confirmations.to_le_bytes());
        data.extend_from_slice(&config.deposit_lag.to_le_bytes());
        data.extend_from_slice(&config.credited_height.to_le_bytes());
        data.extend_from_slice(&config.credited_block);
        data.extend_from_slice(&config.credit_index.to_le_bytes());
        data.extend_from_slice(&config.stranded_lamports.to_le_bytes());
        data.push(config.bump);
        data.push(config.vault_bump);
        data.push(config.treasury_bump);
        data
    }

    fn a_record() -> WithdrawalRecord {
        WithdrawalRecord {
            index: 3,
            owner: Pubkey::new_from_array([5u8; 32]),
            burned_lamports: 1_000_000,
            payout_sats: 99_000,
            fee_sats: 1_000,
            script_pubkey: vec![0x00, 0x14, 0xaa, 0xbb],
            bump: 251,
        }
    }

    fn a_record_account(record: &WithdrawalRecord) -> Vec<u8> {
        let mut data = discriminator("account", "WithdrawalRecord").to_vec();
        data.extend_from_slice(&record.index.to_le_bytes());
        data.extend_from_slice(record.owner.as_ref());
        data.extend_from_slice(&record.burned_lamports.to_le_bytes());
        data.extend_from_slice(&record.payout_sats.to_le_bytes());
        data.extend_from_slice(&record.fee_sats.to_le_bytes());
        data.extend_from_slice(&(record.script_pubkey.len() as u32).to_le_bytes());
        data.extend_from_slice(&record.script_pubkey);
        data.push(record.bump);
        data
    }

    #[test]
    fn a_config_round_trips() {
        let config = a_config();
        let data = a_config_account(&config);
        assert_eq!(data.len(), Config::LEN);
        assert_eq!(Config::decode(&data).unwrap(), config);
    }

    #[test]
    fn a_wrong_config_discriminator_fails() {
        let data = vec![0u8; Config::LEN];
        assert!(matches!(
            Config::decode(&data),
            Err(BridgeError::WrongDiscriminator("Config"))
        ));
    }

    #[test]
    fn a_short_config_fails() {
        let data = discriminator("account", "Config").to_vec();
        assert!(matches!(
            Config::decode(&data),
            Err(BridgeError::ShortAccount { .. })
        ));
    }

    #[test]
    fn a_withdrawal_record_round_trips() {
        let record = WithdrawalRecord {
            index: 3,
            owner: Pubkey::new_from_array([5u8; 32]),
            burned_lamports: 1_000_000,
            payout_sats: 99_000,
            fee_sats: 1_000,
            script_pubkey: vec![0x00, 0x14, 0xaa, 0xbb],
            bump: 251,
        };
        assert_eq!(
            WithdrawalRecord::decode(&a_record_account(&record)).unwrap(),
            record
        );
    }

    #[test]
    fn a_lying_script_length_fails() {
        let mut data = discriminator("account", "WithdrawalRecord").to_vec();
        data.extend_from_slice(&0u64.to_le_bytes());
        data.extend_from_slice(&[0u8; 32]);
        data.extend_from_slice(&0u64.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes());
        data.extend_from_slice(&u32::MAX.to_le_bytes());
        data.push(0);
        assert!(matches!(
            WithdrawalRecord::decode(&data),
            Err(BridgeError::ScriptPubkeyOverrun(u32::MAX))
        ));
    }

    #[test]
    fn the_vault_and_the_config_sit_at_other_addresses() {
        let program_id = a_program_id();
        assert_ne!(config_pda(&program_id).0, vault_pda(&program_id).0);
    }

    #[test]
    fn a_config_that_stops_inside_a_field_gives_an_error() {
        let full = a_config_account(&a_config());
        // Every cut after the discriminator must read as a truncated field.
        for cut in 8..full.len() {
            let error = Config::decode(&full[..cut]).unwrap_err();
            assert!(
                matches!(
                    error,
                    BridgeError::ShortAccount { .. } | BridgeError::TruncatedField { .. }
                ),
                "a {cut} byte account gave {error}"
            );
        }
    }

    #[test]
    fn a_record_that_stops_inside_a_field_gives_an_error() {
        let full = a_record_account(&a_record());
        for cut in 8..full.len() {
            let error = WithdrawalRecord::decode(&full[..cut]).unwrap_err();
            assert!(
                matches!(
                    error,
                    BridgeError::ShortAccount { .. }
                        | BridgeError::TruncatedField { .. }
                        | BridgeError::ScriptPubkeyOverrun(_)
                ),
                "a {cut} byte account gave {error}"
            );
        }
    }

    #[test]
    fn the_reader_stops_at_the_end() {
        let mut reader = Reader::new(&[1, 2, 3]);
        assert_eq!(reader.u8(), Some(1));
        assert_eq!(reader.u32(), None);
        assert_eq!(reader.u64(), None);
        assert_eq!(reader.array32(), None);
    }

    #[test]
    fn the_reader_reads_little_endian() {
        let mut reader = Reader::new(&[0x01, 0x00, 0x00, 0x00, 0x02, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(reader.u32(), Some(1));
        assert_eq!(reader.u64(), Some(2));
    }

    #[test]
    fn each_withdrawal_index_gives_another_address() {
        let program_id = a_program_id();
        assert_ne!(
            withdrawal_pda(&program_id, 0).0,
            withdrawal_pda(&program_id, 1).0
        );
    }

    #[test]
    fn the_credit_instruction_carries_the_question_and_one_writable_target_each() {
        let program_id = a_program_id();
        let targets = [
            Pubkey::new_from_array([1u8; 32]),
            Pubkey::new_from_array([2u8; 32]),
        ];
        let ix = credit_deposits_ix(&program_id, 77, &[5u8; 32], 3, 2, &targets);
        assert_eq!(&ix.data[..8], discriminator("global", "credit_deposits"));
        assert_eq!(&ix.data[8..16], &77u64.to_le_bytes());
        assert_eq!(&ix.data[16..48], &[5u8; 32]);
        assert_eq!(&ix.data[48..52], &3u32.to_le_bytes());
        assert_eq!(ix.data[52], 2);
        assert_eq!(ix.data.len(), 53);
        assert_eq!(ix.accounts[2].pubkey, DEPOSIT_ANSWER_ID);
        assert!(!ix.accounts[2].is_writable);
        assert_eq!(ix.accounts[4].pubkey, targets[0]);
        assert_eq!(ix.accounts[5].pubkey, targets[1]);
        assert!(ix.accounts[4..]
            .iter()
            .all(|meta| meta.is_writable && !meta.is_signer));
        assert!(ix.accounts.iter().all(|meta| !meta.is_signer));
    }

    #[test]
    fn the_withdraw_instruction_carries_a_length_prefixed_script() {
        let program_id = a_program_id();
        let user = Pubkey::new_from_array([4u8; 32]);
        let script = [0x00u8, 0x14, 0xaa];
        let ix = withdraw_ix(&program_id, &user, 0, 1_000_000, 500, &script);
        assert_eq!(&ix.data[8..16], &1_000_000u64.to_le_bytes());
        assert_eq!(&ix.data[16..24], &500u64.to_le_bytes());
        assert_eq!(&ix.data[24..28], &3u32.to_le_bytes());
        assert_eq!(&ix.data[28..], &script);
    }

    #[test]
    fn the_settle_instruction_carries_the_height_the_block_hash_and_the_solana_block() {
        let program_id = a_program_id();
        let payee = Pubkey::new_from_array([8u8; 32]);
        let ix = settle_bmm_ix(&program_id, 77, &[5u8; 32], &[6u8; 32], &payee);
        assert_eq!(&ix.data[..8], discriminator("global", "settle_bmm"));
        assert_eq!(&ix.data[8..16], &77u64.to_le_bytes());
        assert_eq!(&ix.data[16..48], &[5u8; 32]);
        assert_eq!(&ix.data[48..], &[6u8; 32]);
        assert_eq!(ix.accounts[2].pubkey, payee);
        assert!(ix.accounts[2].is_writable);
        assert_eq!(ix.accounts[3].pubkey, BMM_ANSWER_ID);
        assert!(!ix.accounts[3].is_writable);
    }

    #[test]
    fn the_newest_recorded_block_has_the_highest_slot() {
        let mut record = vec![0u8; 4 * BLOCK_RECORD_ENTRY_LEN];
        for (index, slot) in [(0usize, 70u64), (1, 130), (2, 99)] {
            let at = index * BLOCK_RECORD_ENTRY_LEN;
            record[at..at + 8].copy_from_slice(&slot.to_le_bytes());
            record[at + 8..at + BLOCK_RECORD_ENTRY_LEN].copy_from_slice(&[slot as u8; 32]);
        }
        assert_eq!(newest_recorded_block(&record), Some((130, [130u8; 32])));
        assert_eq!(newest_recorded_block(&[0u8; 80]), None);
    }

    #[test]
    fn the_commitment_is_sha256_of_the_solana_block_then_the_payee() {
        let payee = Pubkey::new_from_array([2u8; 32]);
        let mut bytes = [1u8; 32].to_vec();
        bytes.extend_from_slice(&[2u8; 32]);
        assert_eq!(
            commitment_of(&[1u8; 32], &payee),
            sha256::Hash::hash(&bytes).to_byte_array()
        );
    }

    #[test]
    fn the_answer_address_is_the_one_the_validator_builds() {
        assert_eq!(
            BMM_ANSWER_ID.to_string(),
            "BmmAnswer1111111111111111111111111111111111"
        );
        assert_eq!(
            BMM_BLOCKS_ID.to_string(),
            "BmmB1ocks1111111111111111111111111111111111"
        );
        assert_eq!(
            DEPOSIT_ANSWER_ID.to_string(),
            "DepositAnswer111111111111111111111111111111"
        );
        assert_ne!(
            treasury_pda(&a_program_id()).0,
            vault_pda(&a_program_id()).0
        );
    }

    #[test]
    fn the_mark_paid_instruction_gives_the_rent_back_to_the_owner() {
        let program_id = a_program_id();
        let oracle = Pubkey::new_from_array([1u8; 32]);
        let owner = Pubkey::new_from_array([6u8; 32]);
        let ix = mark_paid_ix(&program_id, &oracle, &owner, 5);
        assert_eq!(&ix.data[8..16], &5u64.to_le_bytes());
        assert_eq!(ix.accounts[2].pubkey, owner);
        assert!(ix.accounts[2].is_writable);
    }
}
