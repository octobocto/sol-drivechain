use bitcoin::hashes::{sha256, Hash as _};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};

/// The System Program id is thirty-two zero bytes.
pub const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::new_from_array([0u8; 32]);

/// The peg is 1 SOL to 1 BTC, so one satoshi is ten lamports.
pub const LAMPORTS_PER_SAT: u64 = 10;

/// The most records in one bundle. It must match the bridge program.
pub const MAX_BUNDLE_RECORDS: u64 = 1_500;

/// The lowest mainchain fee of one withdrawal. It must match the bridge program.
pub const MIN_FEE_SATS: u64 = 1_000;

/// The slots that an unpaired commitment waits after the cursor reaches it.
/// It must match `SKIP_GRACE_SLOTS` of the bridge program.
pub const SKIP_GRACE_SLOTS: u64 = 300;

pub const CONFIG_SEED: &[u8] = b"config";
pub const VAULT_SEED: &[u8] = b"vault";
pub const WITHDRAWAL_SEED: &[u8] = b"withdrawal";
pub const TREASURY_SEED: &[u8] = b"treasury";

/// The account that the patched validator builds for each settle. It must
/// match `BMM_ANSWER_ID` in the Agave patch and the bridge program.
pub const BMM_ANSWER_ID: Pubkey =
    Pubkey::from_str_const("BmmAnswer1111111111111111111111111111111111");

/// The account that the patched validator builds for each `bundle_paid`. It
/// must match `BMM_PAID_ID` in the Agave patch and the bridge program.
pub const BMM_PAID_ID: Pubkey =
    Pubkey::from_str_const("BmmPaid111111111111111111111111111111111111");

/// The account that the patched validator builds for each credit. It must
/// match `DEPOSIT_ANSWER_ID` in the Agave patch and the bridge program.
pub const DEPOSIT_ANSWER_ID: Pubkey =
    Pubkey::from_str_const("DepositAnswer111111111111111111111111111111");

/// The block record of the patched validator: the last block of each stride
/// of slots on the fork, with the deposit credits of its bank. A settle pays
/// only a commitment to a block in it. It must match `BMM_BLOCKS_ID` in the
/// Agave patch.
pub const BMM_BLOCKS_ID: Pubkey =
    Pubkey::from_str_const("BmmB1ocks1111111111111111111111111111111111");

/// The owner of every sysvar, and of the block record.
pub const SYSVAR_OWNER_ID: Pubkey =
    Pubkey::from_str_const("Sysvar1111111111111111111111111111111111111");

/// One record entry: the slot, the bank hash, the credited eCash height, the
/// credited eCash block, and the eCash block of the parts so far.
const BLOCK_RECORD_ENTRY_LEN: usize = 8 + 32 + 8 + 32 + 32;

/// The size of the block record account. It must match `BLOCK_RECORD_LEN` in
/// the Agave patch.
pub const BLOCK_RECORD_LEN: usize = 2048 * BLOCK_RECORD_ENTRY_LEN;

/// One block of the block record, with the deposit credits of its bank.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordedBlock {
    pub slot: u64,
    pub bank_hash: [u8; 32],
    /// The highest eCash height whose deposits the bank credited in full.
    pub credited_height: u64,
    /// The eCash block at `credited_height`, in the internal byte order.
    /// Zero before the first credit.
    pub credited_block: [u8; 32],
    /// The eCash block at `credited_height + 1` whose first deposits the
    /// bank credited. Zero when no part is open.
    pub part_block: [u8; 32],
}

/// The blocks of a block record, newest first.
pub fn recorded_blocks(record: &[u8]) -> Vec<RecordedBlock> {
    let mut blocks: Vec<RecordedBlock> = record
        .chunks_exact(BLOCK_RECORD_ENTRY_LEN)
        .filter_map(|entry| {
            let mut reader = Reader::new(entry);
            let block = RecordedBlock {
                slot: reader.u64()?,
                bank_hash: reader.array32()?,
                credited_height: reader.u64()?,
                credited_block: reader.array32()?,
                part_block: reader.array32()?,
            };
            (block.bank_hash != [0u8; 32]).then_some(block)
        })
        .collect();
    blocks.sort_by_key(|block| std::cmp::Reverse(block.slot));
    blocks
}

/// The rent-exempt minimum of an account with `data_len` bytes, for a
/// genesis with `--lamports-per-byte-year`. Agave 4 takes that value as the
/// price of one byte, with an exemption threshold of one.
pub fn rent_exempt_lamports(data_len: usize, lamports_per_byte_year: u64) -> Option<u64> {
    const ACCOUNT_STORAGE_OVERHEAD: u64 = 128;
    ACCOUNT_STORAGE_OVERHEAD
        .checked_add(u64::try_from(data_len).ok()?)?
        .checked_mul(lamports_per_byte_year)
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
    #[error(
        "the config holds {found} bytes, and the current layout holds {want}. Only a chain of \
         the current layout migrates"
    )]
    NotTheLayout { want: usize, found: usize },
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
    pub credit_part_block: [u8; 32],
    pub stranded_lamports: u64,
    pub bump: u8,
    pub vault_bump: u8,
    pub treasury_bump: u8,
    pub vault_supply: u64,
    pub float_lamports: u64,
    pub paid_through: u64,
    pub bundled_through: u64,
    pub bundle_sealed_height: u64,
    pub bmm_skip_blocks: u64,
    pub bmm_next_since_slot: u64,
    pub bundle_m6id: [u8; 32],
}

impl Config {
    pub const LEN: usize = 8 + 32 + 8 * 5 + 8 * 3 + 32 + 8 + 32 + 8 + 1 + 1 + 1 + 8 * 7 + 32;

    /// The record indices of the live bundle, if one is live.
    pub fn live_bundle(&self) -> Option<std::ops::Range<u64>> {
        (self.bundled_through > self.paid_through)
            .then_some(self.paid_through..self.bundled_through)
    }

    /// The lamports that gifts put in the vault: `vault + pegged - supply`.
    pub fn vault_surplus(&self, vault: u64) -> Option<u64> {
        vault
            .checked_add(self.pegged_lamports)?
            .checked_sub(self.vault_supply)
    }

    /// True when gifts to the vault back the whole float, so withdrawals are
    /// open.
    pub fn float_backed(&self, vault: u64) -> bool {
        self.vault_surplus(vault)
            .is_some_and(|surplus| surplus >= self.float_lamports)
    }

    /// The account data in the Anchor layout, as the genesis writes it.
    pub fn encode(&self) -> Vec<u8> {
        let mut data = discriminator("account", "Config").to_vec();
        data.extend_from_slice(self.oracle.as_ref());
        for value in [
            self.deposit_high_water,
            self.pegged_lamports,
            self.withdrawal_count,
            self.bmm_next_height,
            self.bmm_paid_total,
            self.deposit_confirmations,
            self.deposit_lag,
            self.credited_height,
        ] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data.extend_from_slice(&self.credited_block);
        data.extend_from_slice(&self.credit_index.to_le_bytes());
        data.extend_from_slice(&self.credit_part_block);
        data.extend_from_slice(&self.stranded_lamports.to_le_bytes());
        data.extend_from_slice(&[self.bump, self.vault_bump, self.treasury_bump]);
        for value in [
            self.vault_supply,
            self.float_lamports,
            self.paid_through,
            self.bundled_through,
            self.bundle_sealed_height,
            self.bmm_skip_blocks,
            self.bmm_next_since_slot,
        ] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data.extend_from_slice(&self.bundle_m6id);
        data
    }

    pub fn decode(data: &[u8]) -> Result<Self, BridgeError> {
        if data.len() != Self::LEN {
            return Err(BridgeError::NotTheLayout {
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
            credit_part_block: reader.array32().ok_or_else(|| cut("credit_part_block"))?,
            stranded_lamports: reader.u64().ok_or_else(|| cut("stranded_lamports"))?,
            bump: reader.u8().ok_or_else(|| cut("bump"))?,
            vault_bump: reader.u8().ok_or_else(|| cut("vault_bump"))?,
            treasury_bump: reader.u8().ok_or_else(|| cut("treasury_bump"))?,
            vault_supply: reader.u64().ok_or_else(|| cut("vault_supply"))?,
            float_lamports: reader.u64().ok_or_else(|| cut("float_lamports"))?,
            paid_through: reader.u64().ok_or_else(|| cut("paid_through"))?,
            bundled_through: reader.u64().ok_or_else(|| cut("bundled_through"))?,
            bundle_sealed_height: reader.u64().ok_or_else(|| cut("bundle_sealed_height"))?,
            bmm_skip_blocks: reader.u64().ok_or_else(|| cut("bmm_skip_blocks"))?,
            bmm_next_since_slot: reader.u64().ok_or_else(|| cut("bmm_next_since_slot"))?,
            bundle_m6id: reader.array32().ok_or_else(|| cut("bundle_m6id"))?,
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

/// The Compute Budget program.
pub const COMPUTE_BUDGET_ID: Pubkey =
    Pubkey::from_str_const("ComputeBudget111111111111111111111111111111");

/// Sets the compute limit of the tx to `units`. A limit with no unit price
/// adds no fee.
pub fn compute_unit_limit_ix(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET_ID,
        accounts: Vec::new(),
        data,
    }
}

/// A System Program transfer of `lamports` from `from` to `to`.
pub fn transfer_ix(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = 2u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: SYSTEM_PROGRAM_ID,
        accounts: vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
        data,
    }
}

/// Puts the open records up to `through` into the live bundle, whose blinded
/// M6 has the ID `m6id` in the internal byte order. The oracle signs.
pub fn seal_bundle_ix(
    program_id: &Pubkey,
    oracle: &Pubkey,
    through: u64,
    m6id: &[u8; 32],
) -> Instruction {
    let mut data = discriminator("global", "seal_bundle").to_vec();
    data.extend_from_slice(&through.to_le_bytes());
    data.extend_from_slice(m6id);
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(config_pda(program_id).0, false),
            AccountMeta::new_readonly(*oracle, true),
        ],
        data,
    }
}

/// Marks the live bundle, which ends at `through`, paid. eCash block
/// `block_hash` at `height` holds the Succeeded event for `m6id`.
/// `block_hash` is in the internal byte order. The oracle signs.
pub fn bundle_paid_ix(
    program_id: &Pubkey,
    oracle: &Pubkey,
    through: u64,
    height: u64,
    block_hash: &[u8; 32],
    m6id: &[u8; 32],
) -> Instruction {
    let mut data = discriminator("global", "bundle_paid").to_vec();
    data.extend_from_slice(&through.to_le_bytes());
    data.extend_from_slice(&height.to_le_bytes());
    data.extend_from_slice(block_hash);
    data.extend_from_slice(m6id);
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(config_pda(program_id).0, false),
            AccountMeta::new_readonly(*oracle, true),
            AccountMeta::new_readonly(BMM_PAID_ID, false),
        ],
        data,
    }
}

/// Closes the record of a paid withdrawal, and gives its rent back to the
/// owner. Anyone can send it.
pub fn close_record_ix(program_id: &Pubkey, owner: &Pubkey, index: u64) -> Instruction {
    let mut data = discriminator("global", "close_record").to_vec();
    data.extend_from_slice(&index.to_le_bytes());
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new_readonly(config_pda(program_id).0, false),
            AccountMeta::new(withdrawal_pda(program_id, index).0, false),
            AccountMeta::new(*owner, false),
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
            "credit_deposits",
            "withdraw",
            "seal_bundle",
            "bundle_paid",
            "close_record",
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
            credit_part_block: [5u8; 32],
            stranded_lamports: 50,
            bump: 254,
            vault_bump: 253,
            treasury_bump: 252,
            vault_supply: 21_000_000_000_000_000,
            float_lamports: 120_000,
            paid_through: 4,
            bundled_through: 9,
            bundle_sealed_height: 990,
            bmm_skip_blocks: 6,
            bmm_next_since_slot: 600,
            bundle_m6id: [6u8; 32],
        }
    }

    fn a_config_account(config: &Config) -> Vec<u8> {
        config.encode()
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
            Err(BridgeError::NotTheLayout { .. })
        ));
    }

    #[test]
    fn a_config_of_an_old_layout_fails() {
        let current = a_config().encode();
        for old in [&current[..187], &current[..75]] {
            assert!(matches!(
                Config::decode(old),
                Err(BridgeError::NotTheLayout { found, .. }) if found == old.len()
            ));
        }
        let mut long = current;
        long.push(0);
        assert!(Config::decode(&long).is_err());
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
                    BridgeError::NotTheLayout { .. } | BridgeError::TruncatedField { .. }
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

    fn an_entry(slot: u64, credited_height: u64) -> Vec<u8> {
        let mut entry = slot.to_le_bytes().to_vec();
        entry.extend_from_slice(&[slot as u8; 32]);
        entry.extend_from_slice(&credited_height.to_le_bytes());
        entry.extend_from_slice(&[credited_height as u8; 32]);
        entry.extend_from_slice(&[credited_height as u8 + 1; 32]);
        entry
    }

    #[test]
    fn the_record_lists_its_blocks_newest_first_with_their_credits() {
        let mut record = Vec::new();
        for (slot, credited) in [(70u64, 10u64), (130, 12), (99, 11)] {
            record.extend(an_entry(slot, credited));
        }
        record.extend_from_slice(&[0u8; BLOCK_RECORD_ENTRY_LEN]);
        let blocks = recorded_blocks(&record);
        let slots: Vec<u64> = blocks.iter().map(|block| block.slot).collect();
        assert_eq!(slots, vec![130, 99, 70]);
        assert_eq!(
            blocks[0],
            RecordedBlock {
                slot: 130,
                bank_hash: [130u8; 32],
                credited_height: 12,
                credited_block: [12u8; 32],
                part_block: [13u8; 32],
            }
        );
        assert!(recorded_blocks(&[0u8; 224]).is_empty());
    }

    #[test]
    fn the_record_has_the_size_of_the_agave_patch() {
        assert_eq!(BLOCK_RECORD_LEN, 229_376);
    }

    #[test]
    fn the_rent_of_an_account_is_its_bytes_and_overhead_at_the_byte_price() {
        // The chain gave 512 for 0 bytes.
        assert_eq!(rent_exempt_lamports(0, 4), Some(512));
        assert_eq!(rent_exempt_lamports(BLOCK_RECORD_LEN, 4), Some(918_016));
        assert_eq!(rent_exempt_lamports(0, 6_960), Some(890_880));
        assert_eq!(rent_exempt_lamports(usize::MAX, 4), None);
    }

    #[test]
    fn the_config_sits_at_the_offsets_that_the_validator_reads() {
        let data = a_config().encode();
        let at = |offset: usize| u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
        assert_eq!(data.len(), 275);
        assert_eq!(&data[8..40], &[7u8; 32]);
        assert_eq!(at(64), 900);
        assert_eq!(at(80), 6);
        assert_eq!(at(88), 100);
        assert_eq!(at(96), 1_000);
        assert_eq!(&data[104..136], &[4u8; 32]);
        assert_eq!(at(136), 2);
        assert_eq!(&data[144..176], &[5u8; 32]);
        assert_eq!(data[184..187], [254, 253, 252]);
        assert_eq!(at(187), 21_000_000_000_000_000);
        assert_eq!(at(195), 120_000);
        assert_eq!(at(203), 4);
        assert_eq!(at(211), 9);
        assert_eq!(at(219), 990);
        assert_eq!(at(227), 6);
        assert_eq!(at(235), 600);
        assert_eq!(&data[243..275], &[6u8; 32]);
    }

    #[test]
    fn the_live_bundle_spans_the_sealed_records_that_are_not_paid() {
        let mut config = a_config();
        assert_eq!(config.live_bundle(), Some(4..9));
        config.paid_through = 9;
        assert_eq!(config.live_bundle(), None);
    }

    #[test]
    fn the_float_is_backed_once_the_vault_surplus_covers_it() {
        let config = a_config();
        let even = config.vault_supply - config.pegged_lamports;
        assert_eq!(config.vault_surplus(even), Some(0));
        assert!(!config.float_backed(even));
        assert!(!config.float_backed(even + 119_999));
        assert!(config.float_backed(even + 120_000));
        assert_eq!(config.vault_surplus(even - 1), None);
        assert!(!config.float_backed(even - 1));
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
    fn the_close_instruction_gives_the_rent_back_to_the_owner() {
        let program_id = a_program_id();
        let owner = Pubkey::new_from_array([6u8; 32]);
        let ix = close_record_ix(&program_id, &owner, 5);
        assert_eq!(&ix.data[8..16], &5u64.to_le_bytes());
        assert_eq!(ix.accounts[1].pubkey, withdrawal_pda(&program_id, 5).0);
        assert_eq!(ix.accounts[2].pubkey, owner);
        assert!(ix.accounts[2].is_writable);
        assert!(ix.accounts.iter().all(|meta| !meta.is_signer));
    }

    #[test]
    fn the_compute_limit_is_the_borsh_form_of_set_compute_unit_limit() {
        let ix = compute_unit_limit_ix(1_400_000);
        assert_eq!(
            ix.program_id.to_string(),
            "ComputeBudget111111111111111111111111111111"
        );
        assert_eq!(ix.data, [2, 0xc0, 0x5c, 0x15, 0x00]);
        assert!(ix.accounts.is_empty());
    }

    #[test]
    fn the_oracle_signs_the_seal_and_the_payment() {
        let program_id = a_program_id();
        let oracle = Pubkey::new_from_array([1u8; 32]);
        let seal = seal_bundle_ix(&program_id, &oracle, 9, &[6u8; 32]);
        assert_eq!(seal.data[..8], discriminator("global", "seal_bundle"));
        assert_eq!(seal.data[8..16], 9u64.to_le_bytes());
        assert_eq!(seal.data[16..], [6u8; 32]);
        let paid = bundle_paid_ix(&program_id, &oracle, 9, 300, &[4u8; 32], &[6u8; 32]);
        // The patch reads `bundle_paid_question` in this order.
        assert_eq!(paid.data.len(), 88);
        assert_eq!(paid.data[..8], discriminator("global", "bundle_paid"));
        assert_eq!(paid.data[8..16], 9u64.to_le_bytes());
        assert_eq!(paid.data[16..24], 300u64.to_le_bytes());
        assert_eq!(paid.data[24..56], [4u8; 32]);
        assert_eq!(paid.data[56..88], [6u8; 32]);
        assert_eq!(paid.accounts[2].pubkey, BMM_PAID_ID);
        assert!(!paid.accounts[2].is_writable && !paid.accounts[2].is_signer);
        for ix in [seal, paid] {
            assert!(ix.accounts[0].is_writable);
            assert_eq!(ix.accounts[1].pubkey, oracle);
            assert!(ix.accounts[1].is_signer);
        }
    }
}
