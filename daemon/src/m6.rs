use bitcoin::{
    absolute::LockTime, opcodes::all::OP_RETURN, script::Builder, transaction::Version, Amount,
    ScriptBuf, Transaction, TxOut, Txid, VarInt,
};

/// One payout in a withdrawal bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payout {
    pub script_pubkey: ScriptBuf,
    pub sats: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum M6Error {
    #[error("a bundle must hold at least one payout")]
    NoPayouts,
    #[error("the total payout is zero")]
    ZeroPayout,
    #[error("the total payout overflows a u64")]
    PayoutOverflow,
}

/// Builds the blinded M6 transaction for a withdrawal bundle.
///
/// Output 0 carries the mainchain fee as eight big-endian bytes. The enforcer
/// reads the fee with `u64::from_be_bytes`, and every other Bitcoin field is
/// little-endian, so a wrong order gives a wrong M6ID and no error.
pub fn build_blinded_m6(fee_sats: u64, payouts: &[Payout]) -> Result<Transaction, M6Error> {
    if payouts.is_empty() {
        return Err(M6Error::NoPayouts);
    }

    let mut total: u64 = 0;
    for payout in payouts {
        total = total
            .checked_add(payout.sats)
            .ok_or(M6Error::PayoutOverflow)?;
    }
    if total == 0 {
        return Err(M6Error::ZeroPayout);
    }

    let mut output = Vec::with_capacity(payouts.len() + 1);
    output.push(fee_output(fee_sats));
    for payout in payouts {
        output.push(TxOut {
            value: Amount::from_sat(payout.sats),
            script_pubkey: payout.script_pubkey.clone(),
        });
    }

    Ok(Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: Vec::new(),
        output,
    })
}

/// The M6ID is the txid of the blinded transaction.
pub fn m6id(blinded: &Transaction) -> Txid {
    blinded.compute_txid()
}

/// Serializes a blinded M6 as `version || 00 || output count || outputs || lock time`.
///
/// rust-bitcoin writes BIP141 marker and flag bytes for a transaction with no
/// inputs, to keep the frame unambiguous. Bitcoin Core and the enforcer test
/// suite both expect the legacy frame instead, so write it by hand.
///
/// # Panics
///
/// Never in practice. The encoder writes into a `Vec`, which takes every byte.
pub fn serialize_legacy(blinded: &Transaction) -> Vec<u8> {
    fn write(blinded: &Transaction, bytes: &mut Vec<u8>) -> std::io::Result<()> {
        use bitcoin::consensus::Encodable;

        blinded.version.consensus_encode(bytes)?;
        VarInt(0).consensus_encode(bytes)?;
        VarInt(blinded.output.len() as u64).consensus_encode(bytes)?;
        for output in &blinded.output {
            output.consensus_encode(bytes)?;
        }
        blinded.lock_time.consensus_encode(bytes)?;
        Ok(())
    }

    let mut bytes = Vec::new();
    // The only error kind is an IO error, and a `Vec` accepts every byte.
    write(blinded, &mut bytes).expect("a Vec never fails to accept bytes");
    bytes
}

fn fee_output(fee_sats: u64) -> TxOut {
    TxOut {
        value: Amount::ZERO,
        script_pubkey: Builder::new()
            .push_opcode(OP_RETURN)
            .push_slice(fee_sats.to_be_bytes())
            .into_script(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::consensus::Encodable;

    fn a_payout(sats: u64) -> Payout {
        Payout {
            script_pubkey: ScriptBuf::from_bytes(
                vec![0x00, 0x14].into_iter().chain([9u8; 20]).collect(),
            ),
            sats,
        }
    }

    #[test]
    fn the_bundle_holds_no_inputs() {
        let tx = build_blinded_m6(1_000, &[a_payout(50_000)]).unwrap();
        assert!(tx.input.is_empty());
    }

    #[test]
    fn the_version_is_two_and_the_lock_time_is_zero() {
        let tx = build_blinded_m6(1_000, &[a_payout(50_000)]).unwrap();
        assert_eq!(tx.version, Version::TWO);
        assert_eq!(tx.lock_time, LockTime::ZERO);
    }

    #[test]
    fn the_fee_output_is_ten_bytes_and_carries_zero_value() {
        let tx = build_blinded_m6(1_000, &[a_payout(50_000)]).unwrap();
        let fee_out = &tx.output[0];
        assert_eq!(fee_out.value, Amount::ZERO);
        assert_eq!(fee_out.script_pubkey.len(), 10);
        assert_eq!(fee_out.script_pubkey.as_bytes()[0], OP_RETURN.to_u8());
        assert_eq!(fee_out.script_pubkey.as_bytes()[1], 8);
    }

    #[test]
    fn the_fee_bytes_are_big_endian() {
        let fee_sats = 0x0102_0304_0506_0708u64;
        let tx = build_blinded_m6(fee_sats, &[a_payout(50_000)]).unwrap();
        let bytes = &tx.output[0].script_pubkey.as_bytes()[2..];
        assert_eq!(bytes, &fee_sats.to_be_bytes());
        assert_ne!(bytes, &fee_sats.to_le_bytes());
    }

    #[test]
    fn the_enforcer_reads_the_same_fee_back() {
        let fee_sats = 123_456_789u64;
        let tx = build_blinded_m6(fee_sats, &[a_payout(50_000)]).unwrap();
        let bytes: [u8; 8] = tx.output[0].script_pubkey.as_bytes()[2..]
            .try_into()
            .unwrap();
        assert_eq!(u64::from_be_bytes(bytes), fee_sats);
    }

    #[test]
    fn the_payouts_follow_the_fee_output() {
        let payouts = [a_payout(50_000), a_payout(70_000)];
        let tx = build_blinded_m6(1_000, &payouts).unwrap();
        assert_eq!(tx.output.len(), 3);
        assert_eq!(tx.output[1].value, Amount::from_sat(50_000));
        assert_eq!(tx.output[2].value, Amount::from_sat(70_000));
    }

    #[test]
    fn the_m6id_is_the_txid() {
        let tx = build_blinded_m6(1_000, &[a_payout(50_000)]).unwrap();
        assert_eq!(m6id(&tx), tx.compute_txid());
    }

    #[test]
    fn a_zero_payout_fails() {
        let error = build_blinded_m6(1_000, &[a_payout(0)]).unwrap_err();
        assert!(matches!(error, M6Error::ZeroPayout));
    }

    #[test]
    fn an_empty_bundle_fails() {
        let error = build_blinded_m6(1_000, &[]).unwrap_err();
        assert!(matches!(error, M6Error::NoPayouts));
    }

    #[test]
    fn a_payout_overflow_fails() {
        let payouts = [a_payout(u64::MAX), a_payout(1)];
        let error = build_blinded_m6(1_000, &payouts).unwrap_err();
        assert!(matches!(error, M6Error::PayoutOverflow));
    }

    #[test]
    fn rust_bitcoin_frames_a_zero_input_transaction_as_segwit() {
        let tx = build_blinded_m6(1_000, &[a_payout(50_000)]).unwrap();
        let mut bytes = Vec::new();
        tx.consensus_encode(&mut bytes).unwrap();
        assert_eq!(&bytes[4..6], &[0x00, 0x01], "the BIP141 marker and flag");
    }

    #[test]
    fn the_legacy_frame_holds_a_zero_input_count() {
        let tx = build_blinded_m6(1_000, &[a_payout(50_000)]).unwrap();
        let bytes = serialize_legacy(&tx);
        assert_eq!(&bytes[..4], &2u32.to_le_bytes());
        assert_eq!(bytes[4], 0x00, "the input count is zero");
        assert_eq!(bytes[5], 0x02, "the fee output plus one payout");
        assert_eq!(
            &bytes[bytes.len() - 4..],
            &[0x00; 4],
            "the lock time is zero"
        );
    }

    #[test]
    fn the_legacy_frame_is_shorter_than_the_segwit_frame() {
        let tx = build_blinded_m6(1_000, &[a_payout(50_000)]).unwrap();
        let mut segwit = Vec::new();
        tx.consensus_encode(&mut segwit).unwrap();
        assert_eq!(serialize_legacy(&tx).len() + 2, segwit.len());
    }

    #[test]
    fn rust_bitcoin_cannot_decode_the_legacy_frame() {
        let tx = build_blinded_m6(9_999, &[a_payout(50_000), a_payout(60_000)]).unwrap();
        let result = bitcoin::consensus::deserialize::<Transaction>(&serialize_legacy(&tx));
        assert!(
            result.is_err(),
            "the decoder reads the zero input count as a BIP141 marker"
        );
    }

    #[test]
    fn the_segwit_frame_decodes_back_to_the_same_m6id() {
        let tx = build_blinded_m6(9_999, &[a_payout(50_000), a_payout(60_000)]).unwrap();
        let bytes = bitcoin::consensus::serialize(&tx);
        let decoded: Transaction = bitcoin::consensus::deserialize(&bytes).unwrap();
        assert_eq!(m6id(&decoded), m6id(&tx));
    }
}
