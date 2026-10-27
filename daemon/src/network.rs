use std::{fmt, str::FromStr};

use bitcoin::Network;

/// The network that the peg runs against.
///
/// eCash is Bitcoin with drivechain enabled. Its networks run `chain=main`, so
/// Bitcoin Core and the enforcer both report them as mainnet. The enforcer
/// tells them apart with `--fork`, not with the chain name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PegNetwork {
    /// eCash alphanet.
    Alphanet,
    /// eCash betanet.
    Betanet,
    /// eCash, the network with real coins.
    Ecash,
    Mainnet,
    Testnet,
    Signet,
    Regtest,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "`{0}` is not a network. Use ecash, betanet, alphanet, mainnet, testnet, signet, or regtest."
)]
pub struct UnknownNetwork(String);

impl PegNetwork {
    pub const ALL: [Self; 7] = [
        Self::Alphanet,
        Self::Betanet,
        Self::Ecash,
        Self::Mainnet,
        Self::Testnet,
        Self::Signet,
        Self::Regtest,
    ];

    /// Which network the enforcer reports for this peg network.
    ///
    /// eCash runs `chain=main`, so its networks come back as mainnet. Over
    /// gRPC the enforcer cannot tell the eCash networks and Bitcoin mainnet
    /// apart. The BIP300 thresholds do differ, and `status` prints them.
    pub fn reported_as(self) -> Self {
        match self {
            Self::Alphanet | Self::Betanet | Self::Ecash => Self::Mainnet,
            other => other,
        }
    }

    /// The value of the enforcer `--fork` flag for this network.
    pub fn fork(self) -> &'static str {
        match self {
            Self::Alphanet => "ecash-alphanet",
            Self::Betanet => "ecash-betanet",
            Self::Ecash => "ecash",
            Self::Mainnet | Self::Testnet | Self::Signet | Self::Regtest => "bitcoin",
        }
    }

    pub fn bitcoin(self) -> Network {
        match self {
            Self::Alphanet | Self::Betanet | Self::Ecash | Self::Mainnet => Network::Bitcoin,
            Self::Testnet => Network::Testnet,
            Self::Signet => Network::Signet,
            Self::Regtest => Network::Regtest,
        }
    }

    /// The default `bitcoind` JSON-RPC port of this network.
    pub fn rpc_port(self) -> u16 {
        match self {
            Self::Alphanet | Self::Betanet | Self::Ecash | Self::Mainnet => 8332,
            Self::Testnet => 18332,
            Self::Signet => 38332,
            Self::Regtest => 18443,
        }
    }

    /// The default peer-to-peer port of this network.
    pub fn p2p_port(self) -> u16 {
        match self {
            Self::Alphanet | Self::Betanet | Self::Ecash | Self::Mainnet => 8333,
            Self::Testnet => 18333,
            Self::Signet => 38333,
            Self::Regtest => 18444,
        }
    }

    /// The BIP44 coin type. Every test network shares coin type 1.
    pub fn coin_type(self) -> u32 {
        match self {
            Self::Alphanet | Self::Betanet | Self::Ecash | Self::Mainnet => 0,
            _ => 1,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Alphanet => "alphanet",
            Self::Betanet => "betanet",
            Self::Ecash => "ecash",
            Self::Mainnet => "mainnet",
            Self::Testnet => "testnet",
            Self::Signet => "signet",
            Self::Regtest => "regtest",
        }
    }

    /// The default D: how many eCash confirmations a deposit waits before a
    /// credit.
    ///
    /// An eCash reorg that drops a credited deposit rolls the Solana chain
    /// back. So a chain that holds real money waits much longer than the BMM
    /// depth N. Alphanet and betanet are test networks, and they wait 6
    /// blocks.
    pub fn default_confirmations(self) -> u32 {
        match self {
            Self::Mainnet => 100,
            Self::Ecash => 12,
            Self::Alphanet | Self::Betanet | Self::Testnet | Self::Signet => 6,
            Self::Regtest => 1,
        }
    }
}

impl FromStr for PegNetwork {
    type Err = UnknownNetwork;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.to_ascii_lowercase().as_str() {
            "alphanet" => Ok(Self::Alphanet),
            "betanet" => Ok(Self::Betanet),
            "ecash" => Ok(Self::Ecash),
            "mainnet" | "bitcoin" | "main" => Ok(Self::Mainnet),
            "testnet" | "test" => Ok(Self::Testnet),
            "signet" => Ok(Self::Signet),
            "regtest" => Ok(Self::Regtest),
            _ => Err(UnknownNetwork(text.to_owned())),
        }
    }
}

impl fmt::Display for PegNetwork {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(self.name())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AddressError {
    #[error("`{0}` is not a Bitcoin address")]
    NotAnAddress(String),
    #[error("`{address}` belongs to another network than {network}")]
    WrongNetwork { address: String, network: String },
}

/// Turns a Bitcoin address into the script pubkey that a withdrawal stores.
pub fn script_pubkey_of(address: &str, network: PegNetwork) -> Result<Vec<u8>, AddressError> {
    let parsed = address
        .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()
        .map_err(|_| AddressError::NotAnAddress(address.to_owned()))?;
    let checked =
        parsed
            .require_network(network.bitcoin())
            .map_err(|_| AddressError::WrongNetwork {
                address: address.to_owned(),
                network: network.name().to_owned(),
            })?;
    Ok(checked.script_pubkey().to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const A_REGTEST_ADDRESS: &str = "bcrt1qyn82l59xn9t8v606qkprvcl6hfsa3z6l7u9meg";
    const A_MAINNET_ADDRESS: &str = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";

    #[test]
    fn a_regtest_address_gives_a_p2wpkh_script() {
        let script = script_pubkey_of(A_REGTEST_ADDRESS, PegNetwork::Regtest).unwrap();
        assert_eq!(script.len(), 22);
        assert_eq!(script[0], 0x00, "the witness version");
        assert_eq!(script[1], 0x14, "twenty bytes follow");
    }

    #[test]
    fn a_mainnet_address_works_on_the_ecash_networks() {
        for network in [
            PegNetwork::Mainnet,
            PegNetwork::Alphanet,
            PegNetwork::Betanet,
            PegNetwork::Ecash,
        ] {
            assert!(script_pubkey_of(A_MAINNET_ADDRESS, network).is_ok());
        }
    }

    #[test]
    fn an_address_of_another_network_fails() {
        let error = script_pubkey_of(A_REGTEST_ADDRESS, PegNetwork::Mainnet).unwrap_err();
        assert!(matches!(error, AddressError::WrongNetwork { .. }));
    }

    #[test]
    fn a_word_that_is_no_address_fails() {
        let error = script_pubkey_of("not an address", PegNetwork::Regtest).unwrap_err();
        assert!(matches!(error, AddressError::NotAnAddress(_)));
    }

    #[test]
    fn every_network_parses_from_its_name() {
        for network in PegNetwork::ALL {
            assert_eq!(network.name().parse::<PegNetwork>().unwrap(), network);
        }
    }

    #[test]
    fn the_parser_takes_upper_case() {
        assert_eq!(
            "REGTEST".parse::<PegNetwork>().unwrap(),
            PegNetwork::Regtest
        );
    }

    #[test]
    fn the_parser_takes_the_bitcoin_core_names() {
        assert_eq!("main".parse::<PegNetwork>().unwrap(), PegNetwork::Mainnet);
        assert_eq!("test".parse::<PegNetwork>().unwrap(), PegNetwork::Testnet);
    }

    #[test]
    fn another_word_fails() {
        assert!("drynet4".parse::<PegNetwork>().is_err());
    }

    #[test]
    fn the_ecash_networks_report_as_mainnet() {
        assert_eq!(PegNetwork::Alphanet.reported_as(), PegNetwork::Mainnet);
        assert_eq!(PegNetwork::Betanet.reported_as(), PegNetwork::Mainnet);
        assert_eq!(PegNetwork::Ecash.reported_as(), PegNetwork::Mainnet);
    }

    #[test]
    fn every_other_network_reports_as_itself() {
        for network in [
            PegNetwork::Mainnet,
            PegNetwork::Testnet,
            PegNetwork::Signet,
            PegNetwork::Regtest,
        ] {
            assert_eq!(network.reported_as(), network);
        }
    }

    #[test]
    fn each_network_names_its_enforcer_fork() {
        assert_eq!(PegNetwork::Alphanet.fork(), "ecash-alphanet");
        assert_eq!(PegNetwork::Betanet.fork(), "ecash-betanet");
        assert_eq!(PegNetwork::Ecash.fork(), "ecash");
        for network in [
            PegNetwork::Mainnet,
            PegNetwork::Testnet,
            PegNetwork::Signet,
            PegNetwork::Regtest,
        ] {
            assert_eq!(network.fork(), "bitcoin");
        }
    }

    #[test]
    fn the_ecash_networks_run_the_bitcoin_chain() {
        for network in [PegNetwork::Alphanet, PegNetwork::Betanet, PegNetwork::Ecash] {
            assert_eq!(network.bitcoin(), Network::Bitcoin);
            assert_eq!(network.coin_type(), 0);
        }
    }

    #[test]
    fn each_network_holds_its_own_name() {
        let mut names: Vec<&str> = PegNetwork::ALL.iter().map(|n| n.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), PegNetwork::ALL.len());
    }

    #[test]
    fn the_ecash_networks_share_the_mainnet_ports() {
        for network in [PegNetwork::Alphanet, PegNetwork::Betanet, PegNetwork::Ecash] {
            assert_eq!(network.rpc_port(), PegNetwork::Mainnet.rpc_port());
            assert_eq!(network.p2p_port(), PegNetwork::Mainnet.p2p_port());
        }
    }

    #[test]
    fn the_rpc_ports_match_bitcoin_core() {
        assert_eq!(PegNetwork::Mainnet.rpc_port(), 8332);
        assert_eq!(PegNetwork::Testnet.rpc_port(), 18332);
        assert_eq!(PegNetwork::Signet.rpc_port(), 38332);
        assert_eq!(PegNetwork::Regtest.rpc_port(), 18443);
    }

    #[test]
    fn the_p2p_ports_match_bitcoin_core() {
        assert_eq!(PegNetwork::Mainnet.p2p_port(), 8333);
        assert_eq!(PegNetwork::Testnet.p2p_port(), 18333);
        assert_eq!(PegNetwork::Signet.p2p_port(), 38333);
        assert_eq!(PegNetwork::Regtest.p2p_port(), 18444);
    }

    #[test]
    fn only_mainnet_uses_coin_type_zero() {
        assert_eq!(PegNetwork::Mainnet.coin_type(), 0);
        for network in [PegNetwork::Testnet, PegNetwork::Signet, PegNetwork::Regtest] {
            assert_eq!(network.coin_type(), 1);
        }
        assert_eq!(PegNetwork::Alphanet.coin_type(), 0);
    }

    #[test]
    fn mainnet_waits_the_longest_for_a_deposit() {
        assert_eq!(PegNetwork::Mainnet.default_confirmations(), 100);
        assert_eq!(PegNetwork::Ecash.default_confirmations(), 12);
        assert_eq!(PegNetwork::Betanet.default_confirmations(), 6);
        assert_eq!(PegNetwork::Alphanet.default_confirmations(), 6);
        assert_eq!(PegNetwork::Regtest.default_confirmations(), 1);
        // A deposit on a chain with real money waits much longer than a BMM
        // settle, because a reorg that drops a credit rolls the chain back.
        assert!(PegNetwork::Mainnet.default_confirmations() > 6);
        assert!(PegNetwork::Ecash.default_confirmations() > 6);
    }

    #[test]
    fn each_network_maps_to_a_bitcoin_network() {
        assert_eq!(PegNetwork::Mainnet.bitcoin(), Network::Bitcoin);
        assert_eq!(PegNetwork::Regtest.bitcoin(), Network::Regtest);
    }
}
