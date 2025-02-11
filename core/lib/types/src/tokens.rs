use chrono::{DateTime, Utc};
use num::{rational::Ratio, BigUint};
use serde::{Deserialize, Serialize};
use std::{convert::TryFrom, fmt, fs::read_to_string, path::PathBuf, str::FromStr};
use thiserror::Error;

/// ID of the RBTC token in zkSync network.
pub use zksync_crypto::params::RBTC_TOKEN_ID;
use zksync_utils::{parse_env, UnsignedRatioSerializeAsDecimal};

use crate::{tx::ChangePubKeyType, AccountId, Address, Log, TokenId, H256, U256};

#[derive(Debug, Error)]
pub enum NewTokenEventParseError {
    #[error("Cannot parse log for New Token Event {0:?}")]
    ParseError(Log),
}

// Order of the fields is important (from more specific types to less specific types)
/// Set of values that can be interpreted as a token descriptor.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash)]
#[serde(untagged, rename_all = "camelCase")]
pub enum TokenLike {
    /// ID of the token in the zkSync network.
    Id(TokenId),
    /// Address of the token in the L1.
    Address(Address),
    /// Symbol associated with token, e.g. "RBTC".
    Symbol(String),
}

impl From<TokenId> for TokenLike {
    fn from(id: TokenId) -> Self {
        Self::Id(id)
    }
}

impl From<Address> for TokenLike {
    fn from(address: Address) -> Self {
        Self::Address(address)
    }
}

impl From<&str> for TokenLike {
    fn from(symbol: &str) -> Self {
        Self::Symbol(symbol.to_string())
    }
}

impl From<&TokenLike> for TokenLike {
    fn from(inner: &TokenLike) -> Self {
        inner.to_owned()
    }
}

impl fmt::Display for TokenLike {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenLike::Id(id) => id.fmt(f),
            TokenLike::Address(addr) => write!(f, "{:#x}", addr),
            TokenLike::Symbol(symbol) => symbol.fmt(f),
        }
    }
}

impl TokenLike {
    pub fn parse(value: &str) -> Self {
        // Try to interpret an address as the token ID.
        if let Ok(id) = u32::from_str(value) {
            return Self::Id(TokenId(id));
        }
        // Try to interpret a token as the token address with or without a prefix.
        let maybe_address = if let Some(value) = value.strip_prefix("0x") {
            value
        } else {
            value
        };
        if let Ok(address) = Address::from_str(maybe_address) {
            return Self::Address(address);
        }
        // Otherwise interpret a string as the token symbol.
        Self::Symbol(value.to_string())
    }

    /// Checks if the token is Rootstock.
    pub fn is_rbtc(&self) -> bool {
        match self {
            Self::Symbol(symbol) => {
                // Case-insensitive comparison against `RBTC`.
                symbol
                    .chars()
                    .map(|c| c.to_ascii_lowercase())
                    .eq("rbtc".chars())
            }
            Self::Address(address) => *address == Address::zero(),
            Self::Id(id) => **id == 0,
        }
    }

    /// Makes request case-insensitive (lowercase).
    /// Used to compare queries against keys in the cache.
    pub fn to_lowercase(&self) -> Self {
        match self {
            Self::Id(id) => Self::Id(*id),
            Self::Address(address) => Self::Address(*address),
            Self::Symbol(symbol) => Self::Symbol(symbol.to_lowercase()),
        }
    }
}

/// Token supported in zkSync protocol
#[derive(Default, Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Token {
    /// id is used for tx signature and serialization
    pub id: TokenId,
    /// Contract address of ERC20 token or Address::zero() for "RBTC"
    pub address: Address,
    /// Token symbol (e.g. "RBTC" or "RDOC")
    pub symbol: String,
    /// Token precision (e.g. 18 for "RBTC" so "1.0" RBTC = 10e18 as U256 number)
    pub decimals: u8,
    pub kind: TokenKind,
    pub is_nft: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq)]
pub enum TokenKind {
    ERC20,
    NFT,
    None,
}

impl Default for TokenKind {
    fn default() -> Self {
        Self::ERC20
    }
}

impl Token {
    pub fn new(id: TokenId, address: Address, symbol: &str, decimals: u8, kind: TokenKind) -> Self {
        Self {
            id,
            address,
            symbol: symbol.to_string(),
            decimals,
            kind,
            is_nft: matches!(kind, TokenKind::NFT),
        }
    }

    pub fn new_nft(id: TokenId, symbol: &str) -> Self {
        Self {
            id,
            address: Default::default(),
            symbol: symbol.to_string(),
            decimals: 0,
            kind: TokenKind::NFT,
            is_nft: true,
        }
    }
}

/// ERC-20 standard token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenInfo {
    /// Address (prefixed with 0x)
    pub address: Address,
    /// Powers of 10 in 1.0 token (18 for default RBTC-like tokens)
    pub decimals: u8,
    /// Token symbol
    pub symbol: String,
}

impl TokenInfo {
    pub fn new(address: Address, symbol: &str, decimals: u8) -> Self {
        Self {
            address,
            symbol: symbol.to_string(),
            decimals,
        }
    }
}

/// Tokens that added through a contract.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NewTokenEvent {
    pub eth_block_number: u64,
    pub address: Address,
    pub id: TokenId,
}

impl TryFrom<Log> for NewTokenEvent {
    type Error = NewTokenEventParseError;

    fn try_from(event: Log) -> Result<NewTokenEvent, NewTokenEventParseError> {
        // `event NewToken(address indexed token, uint16 indexed tokenId)`
        //  Event has such a signature, so let's check that the number of topics is equal to the number of parameters + 1.
        if event.topics.len() != 3 {
            return Err(NewTokenEventParseError::ParseError(event));
        }

        let eth_block_number = match event.block_number {
            Some(block_number) => block_number.as_u64(),
            None => {
                return Err(NewTokenEventParseError::ParseError(event));
            }
        };

        Ok(NewTokenEvent {
            eth_block_number,
            address: Address::from_slice(&event.topics[1].as_fixed_bytes()[12..]),
            id: TokenId(U256::from_big_endian(&event.topics[2].as_fixed_bytes()[..]).as_u32()),
        })
    }
}

// Hidden as it relies on the filesystem structure, which can be different for reverse dependencies.
#[doc(hidden)]
pub fn get_genesis_token_list(network: &str) -> Result<Vec<TokenInfo>, GetGenesisTokenListError> {
    let mut file_path = parse_env::<PathBuf>("ZKSYNC_HOME");
    file_path.push("etc");
    file_path.push("tokens");
    file_path.push(network);
    file_path.set_extension("json");
    Ok(serde_json::from_str(&read_to_string(file_path)?)?)
}

/// Token price known to the zkSync network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenPrice {
    #[serde(with = "UnsignedRatioSerializeAsDecimal")]
    pub usd_price: Ratio<BigUint>,
    pub last_updated: DateTime<Utc>,
}

/// Token price known to the zkSync network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenMarketVolume {
    #[serde(with = "UnsignedRatioSerializeAsDecimal")]
    pub market_volume: Ratio<BigUint>,
    pub last_updated: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Hash, Eq)]
#[serde(untagged)]
pub enum ChangePubKeyFeeTypeArg {
    PreContracts4Version {
        #[serde(rename = "onchainPubkeyAuth")]
        onchain_pubkey_auth: bool,
    },
    ContractsV4Version(ChangePubKeyType),
}

/// Type of transaction fees that exist in the zkSync network.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Hash, Eq)]
pub enum TxFeeTypes {
    /// Fee for the `WithdrawNFT` transaction.
    WithdrawNFT,
    /// Fee for the `WithdrawNFT` operation that requires fast processing.
    FastWithdrawNFT,
    /// Fee for the `Withdraw` or `ForcedExit` transaction.
    Withdraw,
    /// Fee for the `Withdraw` operation that requires fast processing.
    FastWithdraw,
    /// Fee for the `Transfer` operation.
    Transfer,
    /// Fee for the `ChangePubKey` operation.
    ChangePubKey(ChangePubKeyFeeTypeArg),
    /// Fee for the `Swap` operation
    Swap,
    /// Fee for the `MintNFT` operation.
    MintNFT,
}

/// NFT supported in zkSync protocol
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct NFT {
    /// id is used for tx signature and serialization
    pub id: TokenId,
    /// id for enforcing uniqueness token address
    pub serial_id: u32,
    /// id of nft creator
    pub creator_address: Address,
    /// id of nft creator
    pub creator_id: AccountId,
    /// L2 token address
    pub address: Address,
    /// token symbol
    pub symbol: String,
    /// hash of content for nft token
    pub content_hash: H256,
}

impl NFT {
    pub fn new(
        token_id: TokenId,
        serial_id: u32,
        creator_id: AccountId,
        creator_address: Address,
        address: Address,
        symbol: Option<String>,
        content_hash: H256,
    ) -> Self {
        let symbol = symbol.unwrap_or_else(|| format!("NFT-{}", token_id));
        Self {
            id: token_id,
            serial_id,
            creator_address,
            creator_id,
            address,
            symbol,
            content_hash,
        }
    }
}

#[derive(Debug, Error, PartialEq)]
#[error("Incorrect ProverJobStatus number: {0}")]
pub struct IncorrectProverJobStatus(pub i32);

#[derive(Debug, Error)]
pub enum GetGenesisTokenListError {
    #[error(transparent)]
    IOError(#[from] std::io::Error),
    #[error(transparent)]
    SerdeError(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_fee_type_deserialize_old_type() {
        let deserialized: TxFeeTypes =
            serde_json::from_str(r#"{ "ChangePubKey": { "onchainPubkeyAuth": true }}"#).unwrap();

        assert_eq!(
            deserialized,
            TxFeeTypes::ChangePubKey(ChangePubKeyFeeTypeArg::PreContracts4Version {
                onchain_pubkey_auth: true,
            })
        );

        let deserialized: TxFeeTypes =
            serde_json::from_str(r#"{ "ChangePubKey": { "onchainPubkeyAuth": false }}"#).unwrap();
        assert_eq!(
            deserialized,
            TxFeeTypes::ChangePubKey(ChangePubKeyFeeTypeArg::PreContracts4Version {
                onchain_pubkey_auth: false,
            })
        );
    }

    #[test]
    fn tx_fee_type_deserialize() {
        let deserialized: TxFeeTypes =
            serde_json::from_str(r#"{ "ChangePubKey": "Onchain" }"#).unwrap();

        assert_eq!(
            deserialized,
            TxFeeTypes::ChangePubKey(ChangePubKeyFeeTypeArg::ContractsV4Version(
                ChangePubKeyType::Onchain
            ))
        );

        let deserialized: TxFeeTypes =
            serde_json::from_str(r#"{ "ChangePubKey": "ECDSA" }"#).unwrap();

        assert_eq!(
            deserialized,
            TxFeeTypes::ChangePubKey(ChangePubKeyFeeTypeArg::ContractsV4Version(
                ChangePubKeyType::ECDSA
            ))
        );

        let deserialized: TxFeeTypes =
            serde_json::from_str(r#"{ "ChangePubKey": "CREATE2" }"#).unwrap();

        assert_eq!(
            deserialized,
            TxFeeTypes::ChangePubKey(ChangePubKeyFeeTypeArg::ContractsV4Version(
                ChangePubKeyType::CREATE2
            ))
        );
    }

    #[test]
    fn token_like_is_rbtc() {
        let tokens = vec![
            TokenLike::Address(Address::zero()),
            TokenLike::Id(TokenId(0)),
            TokenLike::Symbol("RBTC".into()),
            TokenLike::Symbol("rbtc".into()),
        ];

        for token in tokens {
            assert!(token.is_rbtc());
        }
    }

    #[test]
    fn token_like_to_case_insensitive() {
        assert_eq!(
            TokenLike::Symbol("RBTC".into()).to_lowercase(),
            TokenLike::Symbol("rbtc".into())
        );
    }
}
