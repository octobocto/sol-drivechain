//! The JSON-RPC that BitWindow and drivechaind use to bid for the sidechain:
//! JSON-RPC 2.0 over HTTP POST. drivechaind builds and pays the M8 from the
//! template, and the BMM loop settles the wins.

use std::{collections::HashMap, str::FromStr as _, sync::Arc};

use axum::{body::Bytes, extract::State, http::header, response::IntoResponse, Router};
use bitcoin::{hex::FromHex as _, BlockHash};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use solana_sdk::pubkey::Pubkey;
use tokio::{net::TcpListener, sync::Mutex};

use crate::bmm::{self, BidRules, BmmError, Chain, Memory, NoTemplate, Pair};

/// The daemon cannot give an answer yet: the slot is not active, no block
/// passes rule 3, or the enforcer or the validator does not answer.
pub const NOT_READY: i64 = -32001;
/// Solana settled the eCash height of the template already.
pub const SETTLED: i64 = -32002;
pub const PARSE_ERROR: i64 = -32700;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    fn not_ready(error: impl std::fmt::Display) -> Self {
        Self {
            code: NOT_READY,
            message: error.to_string(),
        }
    }

    fn invalid_params(message: String) -> Self {
        Self {
            code: INVALID_PARAMS,
            message,
        }
    }
}

impl From<NoTemplate> for RpcError {
    fn from(refusal: NoTemplate) -> Self {
        match refusal {
            NoTemplate::Settled(_) => Self {
                code: SETTLED,
                message: refusal.to_string(),
            },
            refusal => Self::not_ready(refusal),
        }
    }
}

impl From<BmmError> for RpcError {
    fn from(error: BmmError) -> Self {
        Self::not_ready(error)
    }
}

/// The sidechain block of a template, as the JSON shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockJson {
    /// The eCash tip in the display byte order.
    pub prev_main_hash: String,
    /// The eCash height of the block that can carry the commitment.
    pub height: u32,
    /// The Solana bank hash in base58.
    pub bank_hash: String,
    /// The payee in base58.
    pub payee: String,
    /// The sidechain slot on eCash.
    pub slot: u8,
}

impl BlockJson {
    fn new(prev_main_hash: BlockHash, pair: &Pair, slot: u8) -> Result<Self, RpcError> {
        Ok(Self {
            prev_main_hash: prev_main_hash.to_string(),
            height: u32::try_from(pair.height)
                .map_err(|_| RpcError::not_ready(BmmError::HeightTooLarge(pair.height)))?,
            bank_hash: Pubkey::new_from_array(pair.block).to_string(),
            payee: pair.payee.to_string(),
            slot,
        })
    }

    fn pair(&self) -> Result<Pair, RpcError> {
        let key = |field: &str, value: &str| {
            Pubkey::from_str(value).map_err(|_| {
                RpcError::invalid_params(format!("`{field}` is not base58: `{value}`"))
            })
        };
        Ok(Pair {
            height: u64::from(self.height),
            block: key("bank_hash", &self.bank_hash)?.to_bytes(),
            payee: key("payee", &self.payee)?,
        })
    }
}

fn block_hash(value: &str) -> Result<BlockHash, RpcError> {
    BlockHash::from_str(value)
        .map_err(|_| RpcError::invalid_params(format!("`{value}` is not an eCash block hash")))
}

fn params<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|error| RpcError::invalid_params(error.to_string()))
}

pub struct Server<C> {
    pub chain: Arc<C>,
    pub memory: Arc<Mutex<Memory>>,
    pub rules: BidRules,
    pub sidechain_id: u8,
}

impl<C> Clone for Server<C> {
    fn clone(&self) -> Self {
        Self {
            chain: self.chain.clone(),
            memory: self.memory.clone(),
            rules: self.rules,
            sidechain_id: self.sidechain_id,
        }
    }
}

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

impl<C: Chain> Server<C> {
    /// The JSON-RPC 2.0 answer to one request body.
    pub async fn reply(&self, body: &[u8]) -> Value {
        let (id, result) = match serde_json::from_slice::<Request>(body) {
            Ok(request) => (request.id, self.call(&request.method, request.params).await),
            Err(error) => (
                Value::Null,
                Err(RpcError {
                    code: PARSE_ERROR,
                    message: error.to_string(),
                }),
            ),
        };
        match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": error.code, "message": error.message},
            }),
        }
    }

    pub async fn call(&self, method: &str, params_value: Value) -> Result<Value, RpcError> {
        match method {
            "get_block_template" => self.get_block_template().await,
            "connect_block" => {
                let (block, main_block_hash): (BlockJson, String) = params(params_value)?;
                self.connect_block(&block, &main_block_hash)
                    .await
                    .map(Value::Bool)
            }
            "get_bmm_inclusions" => {
                let (critical_hash,): (String,) = params(params_value)?;
                self.get_bmm_inclusions(&critical_hash)
                    .await
                    .map(|hashes| json!(hashes))
            }
            "template_valid" => {
                let (block,): (BlockJson,) = params(params_value)?;
                self.template_valid(&block).await.map(Value::Bool)
            }
            "getblockcount" => Ok(json!(self
                .chain
                .config()
                .await?
                .bmm_next_height
                .saturating_sub(1))),
            _ => Err(RpcError {
                code: METHOD_NOT_FOUND,
                message: format!("the method `{method}` does not exist"),
            }),
        }
    }

    /// The template for the next eCash block. Its pair goes to the validators
    /// before the answer, so no commitment can win without its pair.
    pub async fn get_block_template(&self) -> Result<Value, RpcError> {
        let tip = self.chain.tip().await?;
        let template = bmm::template(&*self.chain, &self.memory, &self.rules, tip).await?;
        self.memory.lock().await.remember(template.pair);
        self.chain.publish(&[template.pair]).await?;
        Ok(json!({
            "critical_hash": crate::hex::encode(&template.pair.commitment()),
            "block": BlockJson::new(tip.0, &template.pair, self.sidechain_id)?,
            "fees_sats": i64::try_from(template.fees_sats).unwrap_or(i64::MAX),
        }))
    }

    /// True when the active eCash block at the height of `block` is
    /// `main_block_hash` and commits to the pair of `block`.
    pub async fn connect_block(
        &self,
        block: &BlockJson,
        main_block_hash: &str,
    ) -> Result<bool, RpcError> {
        let pair = block.pair()?;
        let main_block_hash = block_hash(main_block_hash)?;
        let tip = self.chain.tip().await?;
        let connected = self
            .chain
            .active_block(tip, pair.height)
            .await?
            .is_some_and(|(hash, commitment)| {
                hash == main_block_hash && commitment == Some(pair.commitment())
            });
        if connected {
            self.memory.lock().await.remember(pair);
            self.chain.publish(&[pair]).await?;
        }
        Ok(connected)
    }

    /// The active eCash blocks that carry `critical_hash`, in the display
    /// byte order.
    pub async fn get_bmm_inclusions(&self, critical_hash: &str) -> Result<Vec<String>, RpcError> {
        let commitment = <[u8; 32]>::from_hex(critical_hash).map_err(|_| {
            RpcError::invalid_params(format!("`{critical_hash}` is not 32 bytes of hex"))
        })?;
        let Some(pair) = self.memory.lock().await.pair(&commitment) else {
            return Ok(Vec::new());
        };
        let tip = self.chain.tip().await?;
        Ok(self
            .chain
            .active_block(tip, pair.height)
            .await?
            .filter(|(_, found)| found == &Some(commitment))
            .map(|(hash, _)| hash.to_string())
            .into_iter()
            .collect())
    }

    /// True while the block record holds the bank of `block`, and that bank
    /// passes rule 3 for the height of `block`.
    pub async fn template_valid(&self, block: &BlockJson) -> Result<bool, RpcError> {
        let pair = block.pair()?;
        let tip = self.chain.tip().await?;
        let lag = self.chain.config().await?.deposit_lag;
        let Some(recorded) = self
            .chain
            .recorded_blocks()
            .await?
            .into_iter()
            .find(|recorded| recorded.bank_hash == pair.block)
        else {
            return Ok(false);
        };
        Ok(bmm::passes_rule_3_on(
            &*self.chain,
            tip,
            &recorded,
            pair.height,
            lag,
            &mut HashMap::new(),
        )
        .await?)
    }
}

async fn handle<C: Chain + 'static>(
    State(server): State<Server<C>>,
    body: Bytes,
) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/json")],
        server.reply(&body).await.to_string(),
    )
}

/// Serves the JSON-RPC on `listener`, on every path.
pub async fn serve<C: Chain + 'static>(
    listener: TcpListener,
    server: Server<C>,
) -> Result<(), BmmError> {
    let app = Router::new().fallback(handle::<C>).with_state(server);
    axum::serve(listener, app).await.map_err(BmmError::Serve)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bitcoin::hashes::Hash as _;
    use solana_rpc_client::nonblocking::rpc_client::RpcClient;
    use solana_rpc_client_api::{
        client_error::ErrorKind,
        request::{RpcError as ClientRpcError, RpcRequest},
    };

    use super::*;
    use crate::{
        bmm::ActiveBlock,
        bridge::{Config, RecordedBlock},
    };

    struct Fake {
        tip: (BlockHash, u32),
        slot_active: bool,
        config: Config,
        treasury: u64,
        treasury_reads: usize,
        recorded: Vec<RecordedBlock>,
        blocks: BTreeMap<u64, (BlockHash, Option<[u8; 32]>)>,
        published: Vec<Pair>,
    }

    struct FakeChain(std::sync::Mutex<Fake>);

    impl FakeChain {
        fn with<T>(&self, f: impl FnOnce(&mut Fake) -> T) -> T {
            f(&mut self.0.lock().unwrap())
        }
    }

    impl Chain for FakeChain {
        async fn tip(&self) -> Result<(BlockHash, u32), BmmError> {
            Ok(self.with(|fake| fake.tip))
        }

        async fn slot_is_active(&self) -> Result<bool, BmmError> {
            Ok(self.with(|fake| fake.slot_active))
        }

        async fn config(&self) -> Result<Config, BmmError> {
            Ok(self.with(|fake| fake.config))
        }

        async fn treasury(&self) -> Result<u64, BmmError> {
            Ok(self.with(|fake| {
                fake.treasury_reads += 1;
                fake.treasury
            }))
        }

        async fn recorded_blocks(&self) -> Result<Vec<RecordedBlock>, BmmError> {
            Ok(self.with(|fake| fake.recorded.clone()))
        }

        async fn block_hash_at(
            &self,
            tip: (BlockHash, u32),
            height: u64,
        ) -> Result<Option<[u8; 32]>, BmmError> {
            Ok(self
                .active_block(tip, height)
                .await?
                .map(|(hash, _)| hash.to_byte_array()))
        }

        async fn active_block(
            &self,
            tip: (BlockHash, u32),
            height: u64,
        ) -> Result<Option<ActiveBlock>, BmmError> {
            Ok(self.with(|fake| {
                (height <= u64::from(tip.1))
                    .then(|| fake.blocks.get(&height).copied())
                    .flatten()
            }))
        }

        async fn publish(&self, pairs: &[Pair]) -> Result<(), BmmError> {
            self.with(|fake| fake.published.extend_from_slice(pairs));
            Ok(())
        }
    }

    const TIP_HEIGHT: u32 = 100;
    const LAG: u64 = 5;
    const BANK: [u8; 32] = [9u8; 32];

    fn ecash_hash(byte: u8) -> BlockHash {
        let mut bytes = [byte; 32];
        bytes[0] = 0x01;
        BlockHash::from_byte_array(bytes)
    }

    fn payee() -> Pubkey {
        Pubkey::new_from_array([7u8; 32])
    }

    fn config(next_height: u64) -> Config {
        Config {
            oracle: Pubkey::default(),
            deposit_high_water: 0,
            pegged_lamports: 0,
            withdrawal_count: 0,
            bmm_next_height: next_height,
            bmm_paid_total: 0,
            deposit_confirmations: 0,
            deposit_lag: LAG,
            credited_height: 0,
            credited_block: [0u8; 32],
            credit_index: 0,
            credit_part_block: [0u8; 32],
            stranded_lamports: 0,
            bump: 0,
            vault_bump: 0,
            treasury_bump: 0,
        }
    }

    fn recorded(bank_hash: [u8; 32], credited_height: u64) -> RecordedBlock {
        RecordedBlock {
            slot: 640,
            bank_hash,
            credited_height,
            credited_block: ecash_hash(credited_height as u8).to_byte_array(),
            part_block: [0u8; 32],
        }
    }

    fn server() -> Server<FakeChain> {
        let blocks = (90..=u64::from(TIP_HEIGHT))
            .map(|height| (height, (ecash_hash(height as u8), None)))
            .collect();
        Server {
            chain: Arc::new(FakeChain(std::sync::Mutex::new(Fake {
                tip: (ecash_hash(TIP_HEIGHT as u8), TIP_HEIGHT),
                slot_active: true,
                config: config(90),
                treasury: 0,
                treasury_reads: 0,
                recorded: vec![recorded(BANK, 98)],
                blocks,
                published: Vec::new(),
            }))),
            memory: Arc::new(Mutex::new(Memory::default())),
            rules: BidRules {
                payee: payee(),
                confirmations: 6,
                bid_percent: 50,
                min_bid_sats: 1,
            },
            sidechain_id: 8,
        }
    }

    fn pair() -> Pair {
        Pair {
            height: u64::from(TIP_HEIGHT) + 1,
            block: BANK,
            payee: payee(),
        }
    }

    fn block_json() -> BlockJson {
        BlockJson::new(ecash_hash(TIP_HEIGHT as u8), &pair(), 8).unwrap()
    }

    /// Mines the next eCash block with `commitment` in its coinbase.
    fn mine(server: &Server<FakeChain>, commitment: Option<[u8; 32]>) -> BlockHash {
        let hash = ecash_hash(0xee);
        server.chain.with(|fake| {
            fake.tip = (hash, fake.tip.1 + 1);
            fake.blocks
                .insert(u64::from(fake.tip.1), (hash, commitment));
        });
        hash
    }

    async fn code_of(server: &Server<FakeChain>, method: &str, params: Value) -> i64 {
        server.call(method, params).await.unwrap_err().code
    }

    #[tokio::test]
    async fn a_template_names_the_next_block_and_publishes_its_pair() {
        let server = server();
        let template = server.get_block_template().await.unwrap();
        assert_eq!(template["block"], json!(block_json()));
        assert_eq!(template["block"]["height"], json!(101));
        assert_eq!(template["block"]["slot"], json!(8));
        assert_eq!(
            template["critical_hash"],
            json!(crate::hex::encode(&crate::bridge::commitment_of(
                &BANK,
                &payee()
            )))
        );
        assert_eq!(template["fees_sats"], json!(0));
        assert_eq!(server.chain.with(|fake| fake.published.clone()), [pair()]);
        assert_eq!(
            server.memory.lock().await.pair(&pair().commitment()),
            Some(pair())
        );
    }

    #[tokio::test]
    async fn the_hex_orders_give_the_bytes_of_the_m8() {
        let server = server();
        let template = server.get_block_template().await.unwrap();
        let critical = <[u8; 32]>::from_hex(template["critical_hash"].as_str().unwrap()).unwrap();
        let prev = block_hash(template["block"]["prev_main_hash"].as_str().unwrap()).unwrap();
        let m8 = [&[0x00, 0xbf, 0x00, 8][..], &critical, &prev.to_byte_array()].concat();
        let want = [
            &[0x00, 0xbf, 0x00, 8][..],
            &crate::bridge::commitment_of(&BANK, &payee()),
            &ecash_hash(TIP_HEIGHT as u8).to_byte_array(),
        ]
        .concat();
        assert_eq!(m8, want);
        assert_eq!(
            template["block"]["prev_main_hash"],
            json!(ecash_hash(TIP_HEIGHT as u8).to_string())
        );
        assert_ne!(
            crate::hex::encode(&ecash_hash(TIP_HEIGHT as u8).to_byte_array()),
            ecash_hash(TIP_HEIGHT as u8).to_string()
        );
    }

    #[tokio::test]
    async fn the_fees_stay_the_same_for_the_whole_tip() {
        let server = server();
        server.memory.lock().await.observe(93, 0);
        server.chain.with(|fake| fake.treasury = 7_000_000);
        let first = server.get_block_template().await.unwrap();
        assert_eq!(first["fees_sats"], json!(50_000));

        server.chain.with(|fake| fake.treasury = 70_000_000);
        let again = server.get_block_template().await.unwrap();
        assert_eq!(again["fees_sats"], json!(50_000));
        assert_eq!(server.chain.with(|fake| fake.treasury_reads), 1);

        mine(&server, None);
        let next = server.get_block_template().await.unwrap();
        assert_ne!(next["fees_sats"], json!(50_000));
        assert_eq!(server.chain.with(|fake| fake.treasury_reads), 2);
    }

    #[tokio::test]
    async fn an_inactive_slot_is_not_ready() {
        let server = server();
        server.chain.with(|fake| fake.slot_active = false);
        assert_eq!(
            code_of(&server, "get_block_template", json!([])).await,
            NOT_READY
        );
    }

    #[tokio::test]
    async fn no_block_that_passes_rule_3_is_not_ready() {
        let server = server();
        server
            .chain
            .with(|fake| fake.recorded = vec![recorded(BANK, 90)]);
        assert_eq!(
            code_of(&server, "get_block_template", json!(null)).await,
            NOT_READY
        );
        assert!(server.chain.with(|fake| fake.published.is_empty()));
    }

    #[tokio::test]
    async fn a_settled_height_has_its_own_code() {
        let server = server();
        server.chain.with(|fake| fake.config = config(105));
        assert_eq!(
            code_of(&server, "get_block_template", json!([])).await,
            SETTLED
        );
    }

    #[tokio::test]
    async fn bad_requests_get_the_standard_codes() {
        let server = server();
        assert_eq!(
            code_of(&server, "no_such_method", json!([])).await,
            METHOD_NOT_FOUND
        );
        assert_eq!(
            code_of(&server, "connect_block", json!([1])).await,
            INVALID_PARAMS
        );
        assert_eq!(
            code_of(&server, "get_bmm_inclusions", json!(["abcd"])).await,
            INVALID_PARAMS
        );
        let reply = server.reply(b"{not json").await;
        assert_eq!(reply["error"]["code"], json!(PARSE_ERROR));
    }

    #[tokio::test]
    async fn connect_block_is_true_only_for_the_block_with_the_commitment() {
        let server = server();
        let block = block_json();
        let won = mine(&server, Some(pair().commitment()));
        assert!(server
            .connect_block(&block, &won.to_string())
            .await
            .unwrap());
        assert_eq!(server.chain.with(|fake| fake.published.clone()), [pair()]);

        let other = ecash_hash(0x55).to_string();
        assert!(!server.connect_block(&block, &other).await.unwrap());
        let reversed = crate::hex::encode(&won.to_byte_array());
        assert!(!server.connect_block(&block, &reversed).await.unwrap());
    }

    #[tokio::test]
    async fn connect_block_is_false_for_another_commitment_or_no_block() {
        let server = server();
        let block = block_json();
        assert!(!server
            .connect_block(&block, &ecash_hash(0xee).to_string())
            .await
            .unwrap());
        let mined = mine(&server, Some([1u8; 32]));
        assert!(!server
            .connect_block(&block, &mined.to_string())
            .await
            .unwrap());
        assert!(server.chain.with(|fake| fake.published.is_empty()));
    }

    #[tokio::test]
    async fn inclusions_name_the_active_blocks_with_the_commitment() {
        let server = server();
        let template = server.get_block_template().await.unwrap();
        let critical_hash = template["critical_hash"].as_str().unwrap();
        assert!(server
            .get_bmm_inclusions(critical_hash)
            .await
            .unwrap()
            .is_empty());

        let won = mine(&server, Some(pair().commitment()));
        assert_eq!(
            server.get_bmm_inclusions(critical_hash).await.unwrap(),
            [won.to_string()]
        );
        assert!(server
            .get_bmm_inclusions(&crate::hex::encode(&[3u8; 32]))
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn inclusions_are_empty_when_another_commitment_won() {
        let server = server();
        let template = server.get_block_template().await.unwrap();
        mine(&server, Some([1u8; 32]));
        assert!(server
            .get_bmm_inclusions(template["critical_hash"].as_str().unwrap())
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn a_template_stays_valid_when_a_newer_block_comes() {
        let server = server();
        server.chain.with(|fake| {
            fake.recorded.insert(0, recorded([4u8; 32], 99));
        });
        assert!(server.template_valid(&block_json()).await.unwrap());
    }

    #[tokio::test]
    async fn a_template_is_invalid_when_its_bank_leaves_the_record() {
        let server = server();
        server
            .chain
            .with(|fake| fake.recorded = vec![recorded([4u8; 32], 99)]);
        assert!(!server.template_valid(&block_json()).await.unwrap());
    }

    #[tokio::test]
    async fn a_template_is_invalid_when_its_credited_block_left_the_chain() {
        let server = server();
        server.chain.with(|fake| {
            fake.blocks.insert(98, (ecash_hash(0x77), None));
        });
        assert!(!server.template_valid(&block_json()).await.unwrap());
    }

    #[tokio::test]
    async fn the_block_count_is_the_last_settled_height() {
        let server = server();
        assert_eq!(server.call("getblockcount", json!([])).await, Ok(json!(89)));
    }

    #[tokio::test]
    async fn a_round_trip_over_http_gives_results_and_error_codes() {
        let server = server();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let serving = tokio::spawn(serve(listener, server.clone()));
        let client = RpcClient::new(url);

        let template: Value = client
            .send(
                RpcRequest::Custom {
                    method: "get_block_template",
                },
                json!([]),
            )
            .await
            .unwrap();
        assert_eq!(template["block"], json!(block_json()));

        let won = mine(&server, Some(pair().commitment()));
        let connected: bool = client
            .send(
                RpcRequest::Custom {
                    method: "connect_block",
                },
                json!([template["block"], won.to_string()]),
            )
            .await
            .unwrap();
        assert!(connected);

        let inclusions: Vec<String> = client
            .send(
                RpcRequest::Custom {
                    method: "get_bmm_inclusions",
                },
                json!([template["critical_hash"]]),
            )
            .await
            .unwrap();
        assert_eq!(inclusions, [won.to_string()]);

        let count: u64 = client
            .send(
                RpcRequest::Custom {
                    method: "getblockcount",
                },
                Value::Null,
            )
            .await
            .unwrap();
        assert_eq!(count, 89);

        server.chain.with(|fake| fake.config = config(200));
        let refused = client
            .send::<Value>(
                RpcRequest::Custom {
                    method: "get_block_template",
                },
                json!([]),
            )
            .await
            .unwrap_err();
        match refused.kind() {
            ErrorKind::RpcError(ClientRpcError::RpcResponseError { code, .. }) => {
                assert_eq!(*code, SETTLED)
            }
            other => panic!("the client got {other:?}"),
        }
        serving.abort();
    }
}
