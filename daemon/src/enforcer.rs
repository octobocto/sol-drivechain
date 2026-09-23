use bitcoin::{BlockHash, Transaction, Txid};
use tonic::transport::Channel;

use crate::network::PegNetwork;
use crate::proto::mainchain::{
    block_producer_service_client::BlockProducerServiceClient,
    get_bmm_h_star_commitment_response::Result as CommitmentResult,
    get_ctip_response,
    mining_service_client::MiningServiceClient,
    sidechain_declaration::{SidechainDeclaration as DeclarationKind, V0},
    subscribe_events_response::event::Event as StreamEvent,
    validator_service_client::ValidatorServiceClient,
    wallet_service_client::WalletServiceClient,
    withdrawal_bundle_event::event::Event as BundleEventKind,
    AckAllProposalsPolicy, CreateBmmCriticalDataTransactionRequest,
    CreateDepositTransactionRequest, CreateNewAddressRequest, GenerateToAddressRequest,
    GetBalanceRequest, GetBlockHeaderInfoRequest, GetBmmHStarCommitmentRequest,
    GetChainInfoRequest, GetChainTipRequest, GetCtipRequest, GetSidechainProposalsRequest,
    GetSidechainsRequest, GetWithdrawalBundleProposalsRequest, Network as ProtoNetwork,
    ProposeWithdrawalBundleRequest, SetAckAllProposalsRequest, SetSidechainAckRequest,
    SetWithdrawalBundlePolicyRequest, SidechainDeclaration, SubmitSidechainProposalRequest,
    SubscribeEventsRequest, SubscribeEventsResponse, WithdrawalBundlePolicy,
};

pub type Result<T> = std::result::Result<T, EnforcerError>;

#[derive(Debug, thiserror::Error)]
pub enum EnforcerError {
    #[error("the enforcer url `{0}` is not valid")]
    BadUrl(String),
    #[error("the enforcer call `{call}` failed")]
    Call {
        call: &'static str,
        #[source]
        source: tonic::Status,
    },
    #[error("the enforcer left the field `{0}` empty")]
    MissingField(&'static str),
    #[error("the title holds {found} bytes, and an M1 title takes at most {want}")]
    TitleTooLong { found: usize, want: usize },
    #[error("the enforcer sent `{value}` for `{field}`, which is not hex")]
    NotHex { field: &'static str, value: String },
    #[error("the paid transaction for bundle {0} does not decode")]
    NotATransaction(Txid),
    #[error("the enforcer runs on `{found}`, but the daemon runs on `{want}`")]
    WrongNetwork { want: &'static str, found: String },
    #[error("the enforcer holds no block at height {0} on the active chain")]
    NoBlockAtHeight(u32),
    #[error("the enforcer does not know block {0}")]
    BlockNotFound(BlockHash),
    #[error("the enforcer sent a commitment of {0} bytes, not 32")]
    BadCommitmentLength(usize),
}

/// One deposit that the mainchain confirmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositEvent {
    pub sequence_number: u64,
    pub address: Vec<u8>,
    pub value_sats: u64,
}

/// What the mainchain did with a withdrawal bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BundleEvent {
    Submitted(Txid),
    /// The mainchain paid the bundle. `paid` is the M6 that it included.
    Succeeded {
        m6id: Txid,
        paid: Transaction,
    },
    Failed(Txid),
}

/// One item from the enforcer event stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PegEvent {
    Connect {
        block_hash: BlockHash,
        height: u32,
        deposits: Vec<DepositEvent>,
        bundles: Vec<BundleEvent>,
    },
    Disconnect {
        block_hash: BlockHash,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ctip {
    pub value_sats: u64,
    pub sequence_number: u64,
}

/// One open sidechain proposal, before a slot activates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProposalInfo {
    pub sidechain_number: u32,
    pub vote_count: u32,
    pub proposal_height: u32,
    pub proposal_age: u32,
    /// The hash that names this proposal, in the hex the enforcer sent. The
    /// daemon hands it back unchanged, so no byte order can go wrong.
    pub description_hash: String,
}

/// What the M1 declares about a sidechain.
///
/// BIP300 gives the declaration two optional hashes. `hash_id_1` names the
/// release tarball, and `hash_id_2` names the build commit. Both go into the
/// proposal hash, so a change to either makes a different proposal.
///
/// The fields stay private, because a declaration that cannot encode must not
/// exist. Build one with [`Declaration::new`] or [`Declaration::with_hashes`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    title: String,
    description: String,
    hash_id_1: [u8; 32],
    hash_id_2: [u8; 20],
}

/// The M1 writes the title length in one byte.
pub const MAX_TITLE_LEN: usize = u8::MAX as usize;

impl Declaration {
    /// A declaration that names no release and no commit.
    pub fn new(title: &str, description: &str) -> Result<Self> {
        Self::with_hashes(title, description, [0u8; 32], [0u8; 20])
    }

    /// A declaration that names a release tarball and a build commit.
    pub fn with_hashes(
        title: &str,
        description: &str,
        hash_id_1: [u8; 32],
        hash_id_2: [u8; 20],
    ) -> Result<Self> {
        if title.len() > MAX_TITLE_LEN {
            return Err(EnforcerError::TitleTooLong {
                found: title.len(),
                want: MAX_TITLE_LEN,
            });
        }
        Ok(Self {
            title: title.to_owned(),
            description: description.to_owned(),
            hash_id_1,
            hash_id_2,
        })
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn hash_id_1(&self) -> &[u8; 32] {
        &self.hash_id_1
    }

    pub fn hash_id_2(&self) -> &[u8; 20] {
        &self.hash_id_2
    }

    /// The M1 bytes: `version || title length || title || description ||
    /// hash_id_1 || hash_id_2`.
    pub fn bytes(&self) -> Vec<u8> {
        // The constructor holds the title to one byte, so this cast keeps
        // every digit.
        let title_len = self.title.len() as u8;
        let mut out = Vec::with_capacity(2 + self.title.len() + self.description.len() + 52);
        out.push(0u8);
        out.push(title_len);
        out.extend_from_slice(self.title.as_bytes());
        out.extend_from_slice(self.description.as_bytes());
        out.extend_from_slice(&self.hash_id_1);
        out.extend_from_slice(&self.hash_id_2);
        out
    }

    /// The double SHA-256 of [`Self::bytes`], in the internal byte order.
    pub fn digest(&self) -> [u8; 32] {
        use bitcoin::hashes::{sha256d, Hash as _};

        sha256d::Hash::hash(&self.bytes()).to_byte_array()
    }

    /// The hash that names this proposal, in the hex the enforcer prints.
    pub fn proposal_hash(&self) -> String {
        let mut bytes = self.digest();
        bytes.reverse();
        crate::hex::encode(&bytes)
    }

    /// The coinbase output script that claims a slot.
    ///
    /// A miner who runs no enforcer builds this output. The script is
    /// `OP_RETURN` and one push of `tag || slot || declaration`.
    pub fn m1_script(&self, slot: u8) -> String {
        let mut payload = M1_TAG.to_vec();
        payload.push(slot);
        payload.extend_from_slice(&self.bytes());
        crate::hex::encode(&op_return(&payload))
    }

    /// The coinbase output script that votes for this proposal.
    ///
    /// The hash goes in the internal order, which reads backwards from
    /// [`Self::proposal_hash`].
    pub fn m2_script(&self, slot: u8) -> String {
        let mut payload = M2_TAG.to_vec();
        payload.push(slot);
        payload.extend_from_slice(&self.digest());
        crate::hex::encode(&op_return(&payload))
    }
}

/// One sidechain slot, as the mainchain sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotInfo {
    pub sidechain_number: u32,
    pub vote_count: u32,
    pub proposal_height: u32,
    pub activation_height: Option<u32>,
    /// The title that the M1 declared.
    pub title: Option<String>,
    /// The release tarball hash, 256 bits.
    pub hash_id_1: Option<String>,
    /// The build commit hash, 160 bits.
    pub hash_id_2: Option<String>,
}

/// What the mainchain asks for before a slot activates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainInfo {
    pub network: Option<PegNetwork>,
    pub unused_slot_activation_threshold: u32,
    pub unused_slot_proposal_max_age: u32,
    pub withdrawal_bundle_max_age: u32,
    pub withdrawal_bundle_inclusion_threshold: u32,
}

#[derive(Clone)]
pub struct Enforcer {
    validator: ValidatorServiceClient<Channel>,
    block_producer: BlockProducerServiceClient<Channel>,
    mining: MiningServiceClient<Channel>,
    wallet: WalletServiceClient<Channel>,
    sidechain_id: u32,
}

impl Enforcer {
    pub async fn connect(url: String, sidechain_id: u32) -> Result<Self> {
        let channel = Channel::from_shared(url.clone())
            .map_err(|_| EnforcerError::BadUrl(url))?
            .connect_lazy();
        Ok(Self {
            validator: ValidatorServiceClient::new(channel.clone()),
            block_producer: BlockProducerServiceClient::new(channel.clone()),
            mining: MiningServiceClient::new(channel.clone()),
            wallet: WalletServiceClient::new(channel),
            sidechain_id,
        })
    }

    /// Reads the network and the BIP300 thresholds.
    pub async fn chain_info(&mut self) -> Result<ChainInfo> {
        let response = self
            .validator
            .get_chain_info(GetChainInfoRequest {})
            .await
            .map_err(|source| EnforcerError::Call {
                call: "GetChainInfo",
                source,
            })?
            .into_inner();
        let constants = response.bip300_constants.unwrap_or_default();
        Ok(ChainInfo {
            network: peg_network(response.network),
            unused_slot_activation_threshold: constants.unused_sidechain_slot_activation_threshold,
            unused_slot_proposal_max_age: constants.unused_sidechain_slot_proposal_max_age,
            withdrawal_bundle_max_age: constants.withdrawal_bundle_max_age,
            withdrawal_bundle_inclusion_threshold: constants.withdrawal_bundle_inclusion_threshold,
        })
    }

    /// Sends the M1 that claims the sidechain slot.
    pub async fn submit_sidechain_proposal(&mut self, declared: &Declaration) -> Result<()> {
        let declaration = SidechainDeclaration {
            sidechain_declaration: Some(DeclarationKind::V0(V0 {
                title: Some(declared.title.clone()),
                description: Some(declared.description.clone()),
                hash_id_1: Some(crate::proto::common::ConsensusHex {
                    hex: Some(crate::hex::encode(&declared.hash_id_1)),
                }),
                hash_id_2: Some(crate::proto::common::Hex {
                    hex: Some(crate::hex::encode(&declared.hash_id_2)),
                }),
            })),
        };
        let result = self
            .block_producer
            .submit_sidechain_proposal(SubmitSidechainProposalRequest {
                sidechain_id: Some(self.sidechain_id),
                declaration: Some(declaration),
            })
            .await;
        match result {
            Ok(_) => Ok(()),
            // The enforcer already holds this proposal, which is the wanted
            // state. It reaches the chain in the next block it builds.
            Err(status) if status.code() == tonic::Code::AlreadyExists => Ok(()),
            Err(source) => Err(EnforcerError::Call {
                call: "SubmitSidechainProposal",
                source,
            }),
        }
    }

    /// Votes for one named proposal, and for no other.
    pub async fn set_sidechain_ack(
        &mut self,
        sidechain_number: u32,
        description_hash: &str,
        ack: bool,
    ) -> Result<()> {
        let result = self
            .block_producer
            .set_sidechain_ack(SetSidechainAckRequest {
                sidechain_number: Some(sidechain_number),
                description_sha256d_hash: Some(crate::proto::common::ReverseHex {
                    hex: Some(description_hash.to_owned()),
                }),
                ack,
            })
            .await;
        match result {
            Ok(_) => Ok(()),
            // A second ACK of one proposal hits the unique index. The vote
            // already stands, so that answer is the wanted state.
            Err(status) if is_repeat_ack(&status) => Ok(()),
            Err(source) => Err(EnforcerError::Call {
                call: "SetSidechainAck",
                source,
            }),
        }
    }

    /// Sets how the miner votes on every open sidechain proposal.
    pub async fn set_ack_all_proposals(&mut self, policy: AckAllProposalsPolicy) -> Result<()> {
        self.block_producer
            .set_ack_all_proposals(SetAckAllProposalsRequest {
                policy: policy as i32,
            })
            .await
            .map_err(|source| EnforcerError::Call {
                call: "SetAckAllProposals",
                source,
            })?;
        Ok(())
    }

    /// Mines blocks whose coinbase carries the BIP300 messages.
    pub async fn generate_to_address(
        &mut self,
        blocks: u32,
        address: &str,
    ) -> Result<Vec<BlockHash>> {
        let response = self
            .mining
            .generate_to_address(GenerateToAddressRequest {
                blocks: Some(blocks),
                address: address.to_owned(),
            })
            .await
            .map_err(|source| EnforcerError::Call {
                call: "GenerateToAddress",
                source,
            })?
            .into_inner();
        response
            .block_hashes
            .into_iter()
            .map(|hash| decode_reverse_hash(hash.hex, "block_hash"))
            .collect()
    }

    /// Takes a new address from the enforcer wallet.
    pub async fn new_wallet_address(&mut self) -> Result<String> {
        let response = self
            .wallet
            .create_new_address(CreateNewAddressRequest {})
            .await
            .map_err(|source| EnforcerError::Call {
                call: "CreateNewAddress",
                source,
            })?
            .into_inner();
        Ok(response.address)
    }

    /// Reads the confirmed and the unconfirmed balance of the enforcer wallet.
    pub async fn wallet_balance(&mut self) -> Result<(u64, u64)> {
        let response = self
            .wallet
            .get_balance(GetBalanceRequest {})
            .await
            .map_err(|source| EnforcerError::Call {
                call: "GetBalance",
                source,
            })?
            .into_inner();
        Ok((response.confirmed_sats, response.pending_sats))
    }

    /// Sends a deposit to this sidechain slot.
    ///
    /// The enforcer writes `address` into the OP_RETURN with no change, so
    /// pass the full `s<slot>_<address>_<checksum>` form.
    pub async fn create_deposit(
        &mut self,
        address: &str,
        value_sats: u64,
        fee_sats: u64,
    ) -> Result<Txid> {
        let response = self
            .wallet
            .create_deposit_transaction(CreateDepositTransactionRequest {
                sidechain_id: Some(self.sidechain_id),
                address: Some(address.to_owned()),
                value_sats: Some(value_sats),
                fee_sats: Some(fee_sats),
            })
            .await
            .map_err(|source| EnforcerError::Call {
                call: "CreateDepositTransaction",
                source,
            })?
            .into_inner();
        decode_reverse_txid(response.txid.and_then(|hex| hex.hex), "txid")
    }

    /// Reads every open sidechain proposal.
    pub async fn sidechain_proposals(&mut self) -> Result<Vec<ProposalInfo>> {
        let response = self
            .validator
            .get_sidechain_proposals(GetSidechainProposalsRequest {})
            .await
            .map_err(|source| EnforcerError::Call {
                call: "GetSidechainProposals",
                source,
            })?
            .into_inner();
        Ok(response
            .sidechain_proposals
            .into_iter()
            .map(|proposal| ProposalInfo {
                sidechain_number: proposal.sidechain_number.unwrap_or_default(),
                vote_count: proposal.vote_count.unwrap_or_default(),
                proposal_height: proposal.proposal_height.unwrap_or_default(),
                proposal_age: proposal.proposal_age.unwrap_or_default(),
                description_hash: proposal
                    .description_sha256d_hash
                    .and_then(|hash| hash.hex)
                    .unwrap_or_default(),
            })
            .collect())
    }

    /// Reads every sidechain slot that the mainchain knows.
    pub async fn sidechains(&mut self) -> Result<Vec<SlotInfo>> {
        let response = self
            .validator
            .get_sidechains(GetSidechainsRequest {})
            .await
            .map_err(|source| EnforcerError::Call {
                call: "GetSidechains",
                source,
            })?
            .into_inner();
        Ok(response
            .sidechains
            .into_iter()
            .map(|slot| {
                let declared = declaration_v0(slot.declaration);
                SlotInfo {
                    sidechain_number: slot.sidechain_number.unwrap_or_default(),
                    vote_count: slot.vote_count.unwrap_or_default(),
                    proposal_height: slot.proposal_height.unwrap_or_default(),
                    activation_height: slot.activation_height,
                    title: declared.as_ref().and_then(|v0| v0.title.clone()),
                    hash_id_1: declared
                        .as_ref()
                        .and_then(|v0| v0.hash_id_1.as_ref())
                        .and_then(|hash| hash.hex.clone()),
                    hash_id_2: declared
                        .as_ref()
                        .and_then(|v0| v0.hash_id_2.as_ref())
                        .and_then(|hash| hash.hex.clone()),
                }
            })
            .collect())
    }

    /// Fails when the enforcer runs another Bitcoin network than the daemon.
    pub async fn check_network(&mut self, want: PegNetwork) -> Result<()> {
        let response = self
            .validator
            .get_chain_info(GetChainInfoRequest {})
            .await
            .map_err(|source| EnforcerError::Call {
                call: "GetChainInfo",
                source,
            })?
            .into_inner();
        let found = peg_network(response.network);
        if found != Some(want.reported_as()) {
            return Err(EnforcerError::WrongNetwork {
                want: want.name(),
                found: match found {
                    Some(network) => network.name().to_owned(),
                    None => format!("code {}", response.network),
                },
            });
        }
        Ok(())
    }

    pub async fn chain_tip(&mut self) -> Result<BlockHash> {
        let response = self
            .validator
            .get_chain_tip(GetChainTipRequest {})
            .await
            .map_err(|source| EnforcerError::Call {
                call: "GetChainTip",
                source,
            })?
            .into_inner();
        let header = response
            .block_header_info
            .ok_or(EnforcerError::MissingField("block_header_info"))?;
        decode_reverse_hash(header.block_hash.and_then(|hash| hash.hex), "block_hash")
    }

    /// Reads the hash and the height of the mainchain tip.
    pub async fn tip(&mut self) -> Result<(BlockHash, u32)> {
        let response = self
            .validator
            .get_chain_tip(GetChainTipRequest {})
            .await
            .map_err(|source| EnforcerError::Call {
                call: "GetChainTip",
                source,
            })?
            .into_inner();
        let header = response
            .block_header_info
            .ok_or(EnforcerError::MissingField("block_header_info"))?;
        let hash = decode_reverse_hash(header.block_hash.and_then(|hash| hash.hex), "block_hash")?;
        Ok((hash, header.height))
    }

    /// Reads the headers and the BMM commitments of `block_hash` and up to
    /// `max_ancestors` of its ancestors, newest first. A test uses it to see
    /// how many the enforcer gives.
    pub async fn headers_and_commitments(
        &mut self,
        block_hash: &BlockHash,
        max_ancestors: u32,
    ) -> Result<(usize, usize)> {
        let headers = self
            .validator
            .get_block_header_info(GetBlockHeaderInfoRequest {
                block_hash: Some(crate::proto::common::ReverseHex {
                    hex: Some(block_hash.to_string()),
                }),
                max_ancestors: Some(max_ancestors),
            })
            .await
            .map_err(|source| EnforcerError::Call {
                call: "GetBlockHeaderInfo",
                source,
            })?
            .into_inner()
            .header_infos;
        let response = self
            .validator
            .get_bmm_h_star_commitment(GetBmmHStarCommitmentRequest {
                block_hash: Some(crate::proto::common::ReverseHex {
                    hex: Some(block_hash.to_string()),
                }),
                sidechain_id: Some(self.sidechain_id),
                max_ancestors: Some(max_ancestors),
            })
            .await
            .map_err(|source| EnforcerError::Call {
                call: "GetBmmHStarCommitment",
                source,
            })?
            .into_inner();
        let commitments = match response.result {
            Some(CommitmentResult::Commitment(found)) => found.ancestor_commitments.len() + 1,
            _ => 0,
        };
        Ok((headers.len(), commitments))
    }

    /// Reads the active block at `height` below `tip`, and the BMM
    /// commitment that its coinbase holds for this sidechain.
    pub async fn active_block(
        &mut self,
        tip: (BlockHash, u32),
        height: u32,
    ) -> Result<(BlockHash, Option<[u8; 32]>)> {
        let depth = tip
            .1
            .checked_sub(height)
            .ok_or(EnforcerError::NoBlockAtHeight(height))?;
        let headers = self
            .validator
            .get_block_header_info(GetBlockHeaderInfoRequest {
                block_hash: Some(crate::proto::common::ReverseHex {
                    hex: Some(tip.0.to_string()),
                }),
                max_ancestors: Some(depth),
            })
            .await
            .map_err(|source| EnforcerError::Call {
                call: "GetBlockHeaderInfo",
                source,
            })?
            .into_inner()
            .header_infos;
        let header = headers
            .into_iter()
            .find(|header| header.height == height)
            .ok_or(EnforcerError::NoBlockAtHeight(height))?;
        let block_hash =
            decode_reverse_hash(header.block_hash.and_then(|hash| hash.hex), "block_hash")?;

        let response = self
            .validator
            .get_bmm_h_star_commitment(GetBmmHStarCommitmentRequest {
                block_hash: Some(crate::proto::common::ReverseHex {
                    hex: Some(block_hash.to_string()),
                }),
                sidechain_id: Some(self.sidechain_id),
                max_ancestors: Some(0),
            })
            .await
            .map_err(|source| EnforcerError::Call {
                call: "GetBmmHStarCommitment",
                source,
            })?
            .into_inner();
        let commitment = match response.result {
            Some(CommitmentResult::Commitment(found)) => match found.commitment.and_then(|c| c.hex)
            {
                Some(hex) => {
                    let bytes = decode_hex(&hex, "commitment")?;
                    let length = bytes.len();
                    Some(
                        bytes
                            .try_into()
                            .map_err(|_| EnforcerError::BadCommitmentLength(length))?,
                    )
                }
                None => None,
            },
            Some(CommitmentResult::BlockNotFound(_)) => {
                return Err(EnforcerError::BlockNotFound(block_hash))
            }
            None => return Err(EnforcerError::MissingField("result")),
        };
        Ok((block_hash, commitment))
    }

    /// Sends a BIP301 BMM request. It pays `bid_sats` to the miner of the
    /// block after `tip`, and only if that coinbase holds `h_star`.
    pub async fn create_bmm_request(
        &mut self,
        bid_sats: u64,
        tip: (BlockHash, u32),
        h_star: &[u8; 32],
    ) -> Result<Txid> {
        let response = self
            .wallet
            .create_bmm_critical_data_transaction(CreateBmmCriticalDataTransactionRequest {
                sidechain_id: Some(self.sidechain_id),
                value_sats: Some(bid_sats),
                height: Some(tip.1),
                critical_hash: Some(crate::proto::common::ConsensusHex {
                    hex: Some(crate::hex::encode(h_star)),
                }),
                prev_bytes: Some(crate::proto::common::ReverseHex {
                    hex: Some(tip.0.to_string()),
                }),
            })
            .await
            .map_err(|source| EnforcerError::Call {
                call: "CreateBmmCriticalDataTransaction",
                source,
            })?
            .into_inner();
        decode_reverse_txid(response.txid.and_then(|hex| hex.hex), "txid")
    }

    pub async fn ctip(&mut self) -> Result<Option<Ctip>> {
        let response = self
            .validator
            .get_ctip(GetCtipRequest {
                sidechain_number: Some(self.sidechain_id),
            })
            .await
            .map_err(|source| EnforcerError::Call {
                call: "GetCtip",
                source,
            })?
            .into_inner();
        Ok(response.ctip.map(
            |get_ctip_response::Ctip {
                 value,
                 sequence_number,
                 ..
             }| Ctip {
                value_sats: value,
                sequence_number,
            },
        ))
    }

    pub async fn subscribe_events(&mut self) -> Result<tonic::Streaming<SubscribeEventsResponse>> {
        let response = self
            .validator
            .subscribe_events(SubscribeEventsRequest {
                sidechain_id: Some(self.sidechain_id),
            })
            .await
            .map_err(|source| EnforcerError::Call {
                call: "SubscribeEvents",
                source,
            })?;
        Ok(response.into_inner())
    }

    /// Hands a blinded M6 to the enforcer, in the legacy zero-input frame.
    pub async fn propose_withdrawal_bundle(&mut self, blinded: &Transaction) -> Result<()> {
        self.block_producer
            .propose_withdrawal_bundle(ProposeWithdrawalBundleRequest {
                sidechain_id: Some(self.sidechain_id),
                transaction: Some(crate::m6::serialize_legacy(blinded)),
            })
            .await
            .map_err(|source| EnforcerError::Call {
                call: "ProposeWithdrawalBundle",
                source,
            })?;
        Ok(())
    }

    /// `KNOWN` upvotes only a bundle whose transaction this node holds.
    pub async fn set_withdrawal_bundle_policy(
        &mut self,
        policy: WithdrawalBundlePolicy,
    ) -> Result<()> {
        self.block_producer
            .set_withdrawal_bundle_policy(SetWithdrawalBundlePolicyRequest {
                policy: policy as i32,
            })
            .await
            .map_err(|source| EnforcerError::Call {
                call: "SetWithdrawalBundlePolicy",
                source,
            })?;
        Ok(())
    }

    /// Returns each open bundle proposal with its vote count.
    pub async fn bundle_proposals(&mut self) -> Result<Vec<(Txid, u32)>> {
        let response = self
            .validator
            .get_withdrawal_bundle_proposals(GetWithdrawalBundleProposalsRequest {
                sidechain_id: Some(self.sidechain_id),
            })
            .await
            .map_err(|source| EnforcerError::Call {
                call: "GetWithdrawalBundleProposals",
                source,
            })?
            .into_inner();
        response
            .proposals
            .into_iter()
            .map(|proposal| {
                let txid = decode_txid(proposal.m6id.and_then(|hex| hex.hex), "m6id")?;
                Ok((txid, proposal.vote_count.unwrap_or_default()))
            })
            .collect()
    }
}

/// Reads the Bitcoin network out of the enforcer answer.
pub fn peg_network(value: i32) -> Option<PegNetwork> {
    match ProtoNetwork::try_from(value).ok()? {
        ProtoNetwork::Mainnet => Some(PegNetwork::Mainnet),
        ProtoNetwork::Testnet => Some(PegNetwork::Testnet),
        ProtoNetwork::Signet => Some(PegNetwork::Signet),
        ProtoNetwork::Regtest => Some(PegNetwork::Regtest),
        ProtoNetwork::Unspecified | ProtoNetwork::Unknown => None,
    }
}

/// Turns one stream item into a domain event.
pub fn peg_event(response: SubscribeEventsResponse) -> Result<PegEvent> {
    let event = response
        .event
        .ok_or(EnforcerError::MissingField("event"))?
        .event
        .ok_or(EnforcerError::MissingField("event.event"))?;
    match event {
        StreamEvent::DisconnectBlock(disconnect) => Ok(PegEvent::Disconnect {
            block_hash: decode_reverse_hash(
                disconnect.block_hash.and_then(|hash| hash.hex),
                "block_hash",
            )?,
        }),
        StreamEvent::ConnectBlock(connect) => {
            let header = connect
                .header_info
                .ok_or(EnforcerError::MissingField("header_info"))?;
            let block_hash =
                decode_reverse_hash(header.block_hash.and_then(|hash| hash.hex), "block_hash")?;
            let block_info = connect
                .block_info
                .ok_or(EnforcerError::MissingField("block_info"))?;

            let mut deposits = Vec::new();
            let mut bundles = Vec::new();
            for item in block_info.events {
                match item
                    .event
                    .ok_or(EnforcerError::MissingField("block_info.event"))?
                {
                    crate::proto::mainchain::block_info::event::Event::Deposit(deposit) => {
                        deposits.push(deposit_event(deposit)?);
                    }
                    crate::proto::mainchain::block_info::event::Event::WithdrawalBundle(bundle) => {
                        bundles.push(bundle_event(bundle)?);
                    }
                }
            }
            Ok(PegEvent::Connect {
                block_hash,
                height: header.height,
                deposits,
                bundles,
            })
        }
    }
}

fn deposit_event(deposit: crate::proto::mainchain::Deposit) -> Result<DepositEvent> {
    let output = deposit
        .output
        .ok_or(EnforcerError::MissingField("deposit.output"))?;
    let hex = output
        .address
        .and_then(|address| address.hex)
        .ok_or(EnforcerError::MissingField("deposit.output.address"))?;
    Ok(DepositEvent {
        sequence_number: deposit
            .sequence_number
            .ok_or(EnforcerError::MissingField("deposit.sequence_number"))?,
        address: decode_hex(&hex, "deposit.output.address")?,
        value_sats: output
            .value_sats
            .ok_or(EnforcerError::MissingField("deposit.output.value_sats"))?,
    })
}

fn bundle_event(bundle: crate::proto::mainchain::WithdrawalBundleEvent) -> Result<BundleEvent> {
    let m6id = decode_txid(bundle.m6id.and_then(|hex| hex.hex), "m6id")?;
    let kind = bundle
        .event
        .ok_or(EnforcerError::MissingField("withdrawal_bundle.event"))?
        .event
        .ok_or(EnforcerError::MissingField("withdrawal_bundle.event.event"))?;
    Ok(match kind {
        BundleEventKind::Submitted(_) => BundleEvent::Submitted(m6id),
        BundleEventKind::Failed(_) => BundleEvent::Failed(m6id),
        BundleEventKind::Succeeded(succeeded) => {
            let hex = succeeded
                .transaction
                .and_then(|hex| hex.hex)
                .ok_or(EnforcerError::MissingField("succeeded.transaction"))?;
            let bytes = decode_hex(&hex, "succeeded.transaction")?;
            let paid = bitcoin::consensus::deserialize(&bytes)
                .map_err(|_| EnforcerError::NotATransaction(m6id))?;
            BundleEvent::Succeeded { m6id, paid }
        }
    })
}

/// Reads the answer that says the vote already stands.
fn is_repeat_ack(status: &tonic::Status) -> bool {
    status
        .message()
        .contains("UNIQUE constraint failed: sidechain_acks")
}

/// Reads the version 0 body of a sidechain declaration.
///
/// A later version reads as `None`, because this daemon knows only version 0.
fn declaration_v0(
    declaration: Option<crate::proto::mainchain::SidechainDeclaration>,
) -> Option<crate::proto::mainchain::sidechain_declaration::V0> {
    use crate::proto::mainchain::sidechain_declaration::SidechainDeclaration;

    match declaration?.sidechain_declaration? {
        SidechainDeclaration::V0(v0) => Some(v0),
    }
}

/// The M1 tag that BIP300 gives to a sidechain proposal.
const M1_TAG: [u8; 4] = [0xd5, 0xe0, 0xc4, 0xaf];

/// The M2 tag that BIP300 gives to a proposal vote.
const M2_TAG: [u8; 4] = [0xd6, 0xe1, 0xc5, 0xdf];

/// Wraps a payload as an `OP_RETURN` output script.
fn op_return(payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0x6a];
    match payload.len() {
        length @ 0..=75 => out.push(length as u8),
        length @ 76..=255 => out.extend_from_slice(&[0x4c, length as u8]),
        length => {
            out.push(0x4d);
            out.extend_from_slice(&(length as u16).to_le_bytes());
        }
    }
    out.extend_from_slice(payload);
    out
}

fn decode_hex(hex: &str, field: &'static str) -> Result<Vec<u8>> {
    /// `to_digit(16)` answers 0 to 15, so the value always fits a `u8`.
    fn digit(digit: char, hex: &str, field: &'static str) -> Result<u8> {
        digit
            .to_digit(16)
            .map(|value| value as u8)
            .ok_or_else(|| EnforcerError::NotHex {
                field,
                value: hex.to_owned(),
            })
    }

    let digits: Vec<char> = hex.chars().collect();
    if !digits.len().is_multiple_of(2) {
        return Err(EnforcerError::NotHex {
            field,
            value: hex.to_owned(),
        });
    }

    let mut bytes = Vec::with_capacity(digits.len() / 2);
    for pair in digits.chunks_exact(2) {
        bytes.push(digit(pair[0], hex, field)? * 16 + digit(pair[1], hex, field)?);
    }
    Ok(bytes)
}

fn decode_reverse_hash(hex: Option<String>, field: &'static str) -> Result<BlockHash> {
    use bitcoin::hashes::Hash as _;

    let hex = hex.ok_or(EnforcerError::MissingField(field))?;
    let mut bytes = decode_hex(&hex, field)?;
    bytes.reverse();
    let bytes: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| EnforcerError::NotHex { field, value: hex })?;
    Ok(BlockHash::from_byte_array(bytes))
}

/// A txid arrives byte-reversed, the way Bitcoin prints it.
fn decode_reverse_txid(hex: Option<String>, field: &'static str) -> Result<Txid> {
    use bitcoin::hashes::Hash as _;

    let hex = hex.ok_or(EnforcerError::MissingField(field))?;
    let mut bytes = decode_hex(&hex, field)?;
    bytes.reverse();
    let bytes: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| EnforcerError::NotHex { field, value: hex })?;
    Ok(Txid::from_byte_array(bytes))
}

fn decode_txid(hex: Option<String>, field: &'static str) -> Result<Txid> {
    use bitcoin::hashes::Hash as _;

    let hex = hex.ok_or(EnforcerError::MissingField(field))?;
    let bytes = decode_hex(&hex, field)?;
    let bytes: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| EnforcerError::NotHex { field, value: hex })?;
    Ok(Txid::from_byte_array(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The enforcer answered with this hash for the same title and
    /// description on a regtest chain.
    const A_KNOWN_PROPOSAL_HASH: &str =
        "b9d701b92e684570ebff6a395b9fb27bfac184879106aefb56596fc737a5905e";

    /// The scripts that `bip300301_enforcer` writes for slot 8 of this chain.
    /// `lib/messages.rs` holds both tags and both layouts.
    const A_KNOWN_M1_SCRIPT: &str = "6a4c5bd5e0c4af08000e736f6c2d6472697665636861696e4120536f6c616e612073696465636861696e00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
    const A_KNOWN_M2_SCRIPT: &str =
        "6a25d6e1c5df085e90a537c76f5956fbae06918784c1fa7bb29f5b396affeb7045682eb901d7b9";

    fn a_v0_declaration(
        title: &str,
        tarball: Option<&str>,
        commit: Option<&str>,
    ) -> crate::proto::mainchain::SidechainDeclaration {
        use crate::proto::common::{ConsensusHex, Hex};
        use crate::proto::mainchain::sidechain_declaration::{SidechainDeclaration, V0};

        crate::proto::mainchain::SidechainDeclaration {
            sidechain_declaration: Some(SidechainDeclaration::V0(V0 {
                title: Some(title.to_owned()),
                description: Some(String::new()),
                hash_id_1: tarball.map(|hex| ConsensusHex {
                    hex: Some(hex.to_owned()),
                }),
                hash_id_2: commit.map(|hex| Hex {
                    hex: Some(hex.to_owned()),
                }),
            })),
        }
    }

    #[test]
    fn a_declaration_gives_its_title_and_both_hashes() {
        let declared = declaration_v0(Some(a_v0_declaration(
            "sol-drivechain",
            Some("aa"),
            Some("bb"),
        )))
        .expect("version 0 reads");
        assert_eq!(declared.title.as_deref(), Some("sol-drivechain"));
        assert_eq!(
            declared.hash_id_1.and_then(|hash| hash.hex).as_deref(),
            Some("aa")
        );
        assert_eq!(
            declared.hash_id_2.and_then(|hash| hash.hex).as_deref(),
            Some("bb")
        );
    }

    #[test]
    fn a_declaration_without_hashes_reads() {
        let declared =
            declaration_v0(Some(a_v0_declaration("thunder", None, None))).expect("version 0 reads");
        assert!(declared.hash_id_1.is_none());
        assert!(declared.hash_id_2.is_none());
    }

    #[test]
    fn a_missing_declaration_reads_as_none() {
        assert!(declaration_v0(None).is_none());
    }

    #[test]
    fn an_empty_declaration_reads_as_none() {
        let empty = crate::proto::mainchain::SidechainDeclaration {
            sidechain_declaration: None,
        };
        assert!(declaration_v0(Some(empty)).is_none());
    }

    fn a_sol_declaration() -> Declaration {
        Declaration::new("sol-drivechain", "A Solana sidechain").expect("the title fits one byte")
    }

    #[test]
    fn the_m1_script_matches_the_enforcer() {
        assert_eq!(a_sol_declaration().m1_script(8), A_KNOWN_M1_SCRIPT);
    }

    #[test]
    fn the_m2_script_matches_the_enforcer() {
        assert_eq!(a_sol_declaration().m2_script(8), A_KNOWN_M2_SCRIPT);
    }

    #[test]
    fn the_m1_script_starts_with_the_m1_tag() {
        // OP_RETURN, PUSHDATA1, the length, then the tag.
        assert!(a_sol_declaration()
            .m1_script(8)
            .starts_with("6a4c5bd5e0c4af08"));
    }

    #[test]
    fn the_m2_script_carries_the_hash_in_the_internal_order() {
        let script = a_sol_declaration().m2_script(8);
        let mut reversed: Vec<String> = A_KNOWN_PROPOSAL_HASH
            .as_bytes()
            .chunks(2)
            .map(|pair| String::from_utf8_lossy(pair).into_owned())
            .collect();
        reversed.reverse();
        assert!(script.ends_with(&reversed.concat()));
    }

    #[test]
    fn the_m1_script_grows_with_the_title() {
        let short = Declaration::new("a", "").unwrap().m1_script(8);
        let long = Declaration::new("a-longer-title", "").unwrap().m1_script(8);
        assert!(long.len() > short.len());
    }

    #[test]
    fn the_slot_sits_after_the_tag() {
        assert!(a_sol_declaration()
            .m1_script(24)
            .starts_with("6a4c5bd5e0c4af18"));
        assert!(a_sol_declaration()
            .m2_script(24)
            .starts_with("6a25d6e1c5df18"));
    }

    #[test]
    fn a_declaration_with_no_hashes_keeps_the_known_proposal() {
        assert_eq!(a_sol_declaration().proposal_hash(), A_KNOWN_PROPOSAL_HASH);
    }

    #[test]
    fn a_release_hash_changes_the_proposal() {
        let mut hash = [0u8; 32];
        hash[0] = 1;
        let declared =
            Declaration::with_hashes("sol-drivechain", "A Solana sidechain", hash, [0u8; 20])
                .unwrap();
        assert_ne!(declared.proposal_hash(), A_KNOWN_PROPOSAL_HASH);
    }

    #[test]
    fn a_commit_hash_changes_the_proposal() {
        let mut hash = [0u8; 20];
        hash[0] = 1;
        let declared =
            Declaration::with_hashes("sol-drivechain", "A Solana sidechain", [0u8; 32], hash)
                .unwrap();
        assert_ne!(declared.proposal_hash(), A_KNOWN_PROPOSAL_HASH);
    }

    #[test]
    fn the_declaration_bytes_carry_both_hashes_at_the_end() {
        let declared = Declaration::with_hashes(
            "sol-drivechain",
            "A Solana sidechain",
            [0xaa; 32],
            [0xbb; 20],
        )
        .unwrap();
        let bytes = declared.bytes();
        assert_eq!(&bytes[bytes.len() - 20..], &[0xbb; 20]);
        assert_eq!(&bytes[bytes.len() - 52..bytes.len() - 20], &[0xaa; 32]);
    }

    #[test]
    fn a_title_of_the_greatest_length_still_builds() {
        let title = "t".repeat(MAX_TITLE_LEN);
        let declared = Declaration::new(&title, "").unwrap();
        assert_eq!(declared.bytes()[1], u8::MAX);
    }

    #[test]
    fn a_title_above_the_greatest_length_fails() {
        let title = "t".repeat(MAX_TITLE_LEN + 1);
        let error = Declaration::new(&title, "").unwrap_err();
        assert!(matches!(
            error,
            EnforcerError::TitleTooLong {
                found: 256,
                want: 255
            }
        ));
    }

    #[test]
    fn the_title_length_byte_counts_bytes_and_not_characters() {
        // Two bytes of UTF-8 for one character, so the length byte must read 2.
        let declared = Declaration::new("é", "").unwrap();
        assert_eq!(declared.bytes()[1], 2);
    }

    #[test]
    fn a_declaration_keeps_what_it_holds() {
        let declared =
            Declaration::with_hashes("a title", "a text", [0xaa; 32], [0xbb; 20]).unwrap();
        assert_eq!(declared.title(), "a title");
        assert_eq!(declared.description(), "a text");
        assert_eq!(declared.hash_id_1(), &[0xaa; 32]);
        assert_eq!(declared.hash_id_2(), &[0xbb; 20]);
    }

    #[test]
    fn the_declaration_bytes_start_with_the_version_and_the_title_length() {
        let bytes = a_sol_declaration().bytes();
        assert_eq!(bytes[0], 0);
        assert_eq!(bytes[1], "sol-drivechain".len() as u8);
    }

    #[test]
    fn the_proposal_hash_reads_backwards_from_the_digest() {
        let declared = a_sol_declaration();
        let mut digest = declared.digest();
        digest.reverse();
        assert_eq!(crate::hex::encode(&digest), declared.proposal_hash());
    }

    #[test]
    fn a_short_payload_takes_a_direct_push() {
        assert_eq!(crate::hex::encode(&op_return(&[1, 2, 3])), "6a03010203");
    }

    #[test]
    fn a_payload_above_75_bytes_takes_pushdata1() {
        let script = crate::hex::encode(&op_return(&[7u8; 76]));
        assert!(script.starts_with("6a4c4c"));
    }

    #[test]
    fn a_payload_above_255_bytes_takes_pushdata2() {
        let script = crate::hex::encode(&op_return(&[7u8; 300]));
        assert!(script.starts_with("6a4d2c01"));
    }

    #[test]
    fn a_repeat_ack_reads_as_the_wanted_state() {
        let status = tonic::Status::internal(
            "UNIQUE constraint failed: sidechain_acks.number, sidechain_acks.data_hash",
        );
        assert!(is_repeat_ack(&status));
    }

    #[test]
    fn another_failure_does_not_read_as_a_repeat_ack() {
        assert!(!is_repeat_ack(&tonic::Status::internal("disk full")));
        assert!(!is_repeat_ack(&tonic::Status::not_found("no such slot")));
    }

    #[test]
    fn the_proposal_hash_matches_the_enforcer() {
        assert_eq!(a_sol_declaration().proposal_hash(), A_KNOWN_PROPOSAL_HASH);
    }

    #[test]
    fn the_description_holds_the_version_and_the_title_length() {
        let bytes = Declaration::new("abc", "hello").unwrap().bytes();
        assert_eq!(bytes[0], 0, "the version");
        assert_eq!(bytes[1], 3, "the title length");
        assert_eq!(&bytes[2..5], b"abc");
        assert_eq!(&bytes[5..10], b"hello");
        assert_eq!(bytes.len(), 2 + 3 + 5 + 32 + 20);
    }

    #[test]
    fn another_title_gives_another_hash() {
        assert_ne!(
            a_sol_declaration().proposal_hash(),
            Declaration::new("sol-drivechain", "Another text")
                .unwrap()
                .proposal_hash()
        );
    }

    #[test]
    fn hex_of_writes_two_digits_for_each_byte() {
        assert_eq!(crate::hex::encode(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(crate::hex::encode(&[0u8; 32]).len(), 64);
        assert_eq!(crate::hex::encode(&[0u8; 20]).len(), 40);
    }

    #[test]
    fn hex_of_and_decode_hex_round_trip() {
        let bytes = vec![1u8, 2, 250, 0, 255];
        assert_eq!(decode_hex(&crate::hex::encode(&bytes), "x").unwrap(), bytes);
    }

    #[test]
    fn every_network_maps_back_from_the_enforcer() {
        for network in PegNetwork::ALL {
            let code = match network.reported_as() {
                PegNetwork::Mainnet => ProtoNetwork::Mainnet,
                PegNetwork::Testnet => ProtoNetwork::Testnet,
                PegNetwork::Signet => ProtoNetwork::Signet,
                PegNetwork::Regtest => ProtoNetwork::Regtest,
                other => panic!("{other} must report as one of the four chains"),
            };
            assert_eq!(peg_network(code as i32), Some(network.reported_as()));
        }
    }

    #[test]
    fn an_unknown_network_maps_to_nothing() {
        assert_eq!(peg_network(ProtoNetwork::Unknown as i32), None);
        assert_eq!(peg_network(ProtoNetwork::Unspecified as i32), None);
        assert_eq!(peg_network(99), None);
    }

    #[test]
    fn hex_decodes_to_bytes() {
        assert_eq!(decode_hex("00ff10", "x").unwrap(), vec![0x00, 0xff, 0x10]);
    }

    #[test]
    fn an_odd_hex_length_fails() {
        assert!(decode_hex("abc", "x").is_err());
    }

    #[test]
    fn a_bad_hex_digit_fails() {
        assert!(decode_hex("zz", "x").is_err());
    }

    #[test]
    fn an_empty_hex_gives_no_bytes() {
        assert_eq!(decode_hex("", "x").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn a_block_hash_reverses() {
        use bitcoin::hashes::Hash as _;

        let hex = "00".repeat(31) + "ff";
        let hash = decode_reverse_hash(Some(hex), "x").unwrap();
        assert_eq!(hash.to_byte_array()[0], 0xff);
    }
}
