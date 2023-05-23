use super::PriceError;

use anyhow::format_err;
use async_trait::async_trait;
use chrono::Utc;
use num::rational::Ratio;

use std::time::{Duration, Instant};

use zksync_storage::ConnectionPool;
use zksync_types::{Token, TokenId, TokenPrice};

pub mod coingecko;
pub mod coinmarkercap;

const UPDATE_PRICE_INTERVAL_SECS: u64 = 10 * 60;
/// The limit of time we are willing to wait for response.
pub const REQUEST_TIMEOUT: Duration = Duration::from_millis(700);
/// Configuration parameter of the reqwest Client
pub const CONNECTION_TIMEOUT: Duration = Duration::from_millis(700);

#[async_trait]
pub trait TokenPriceAPI {
    async fn get_price(&self, token: &Token) -> Result<TokenPrice, PriceError>;
}

/// Api responsible for querying for TokenPrices
#[async_trait]
pub trait FeeTickerAPI {
    async fn keep_price_updated(self);
}

#[derive(Debug, Clone)]
pub(super) struct TickerApi<T: TokenPriceAPI> {
    db_pool: ConnectionPool,

    token_price_api: T,
}

impl<T: TokenPriceAPI> TickerApi<T> {
    pub fn new(db_pool: ConnectionPool, token_price_api: T) -> Self {
        Self {
            db_pool,
            token_price_api,
        }
    }

    async fn get_all_tokens(&self) -> Result<Vec<Token>, PriceError> {
        let mut storage = self
            .db_pool
            .access_storage()
            .await
            .map_err(PriceError::db_error)?;
        let tokens = storage
            .tokens_schema()
            .load_tokens()
            .await
            .map_err(|err| PriceError::DBError(err.to_string()))?;
        Ok(tokens.into_values().collect())
    }
    async fn update_stored_value(
        &self,
        token_id: TokenId,
        price: TokenPrice,
    ) -> Result<(), anyhow::Error> {
        let mut storage = self
            .db_pool
            .access_storage()
            .await
            .map_err(|e| format_err!("Can't access storage: {}", e))?;

        storage
            .tokens_schema()
            .update_historical_ticker_price(token_id, price)
            .await
            .map_err(|e| format_err!("Can't update historical ticker price from storage: {}", e))?;

        Ok(())
    }
    async fn update_price(&self, token: &Token) -> Result<(), PriceError> {
        let start = Instant::now();
        let api_price = match self.token_price_api.get_price(token).await {
            Ok(api_price) => api_price,

            // Database contain this token, but is not listed in CoinGecko(CoinMarketCap)
            Err(PriceError::TokenNotFound(_)) => TokenPrice {
                usd_price: Ratio::from_integer(0u32.into()),
                last_updated: Utc::now(),
            },
            Err(e) => return Err(e),
        };

        self.update_stored_value(token.id, api_price.clone())
            .await
            .map_err(|err| PriceError::DBError(err.to_string()))?;
        metrics::histogram!("ticker.update_price", start.elapsed());
        Ok(())
    }
}

#[async_trait]
impl<T: TokenPriceAPI + Send + Sync> FeeTickerAPI for TickerApi<T> {
    /// Get last price from ticker
    async fn get_last_quote(&self, token: TokenLike) -> Result<TokenPrice, PriceError> {
        let start = Instant::now();
        let token = self
            .token_db_cache
            .get_token(
                &mut self
                    .db_pool
                    .access_storage()
                    .await
                    .map_err(PriceError::db_error)?,
                token.clone(),
            )
            .await
            .map_err(PriceError::db_error)?
            .ok_or_else(|| PriceError::token_not_found(format!("Token not found: {:?}", token)))?;

        if token.symbol == "RDOC" {
            metrics::histogram!("ticker.get_last_quote", start.elapsed());
            return Ok(TokenPrice {
                usd_price: Ratio::from_integer(1u32.into()),
                last_updated: Utc::now(),
            });
        }

        if let Some(cached_value) = self.get_stored_value(token.id).await {
            metrics::histogram!("ticker.get_last_quote", start.elapsed());
            return Ok(cached_value);
        }

        let historical_price = self
            .get_historical_ticker_price(token.id)
            .await
            .map_err(|e| vlog::warn!("Failed to get historical ticker price: {}", e));

        if let Ok(Some(historical_price)) = historical_price {
            self.update_stored_value(token.id, historical_price.clone())
                .await;
            metrics::histogram!("ticker.get_last_quote", start.elapsed());
            return Ok(historical_price);
        }

        Err(PriceError::db_error("No price stored in database"))
    }

    /// Get current gas price in ETH
    async fn get_gas_price_wei(&self) -> Result<BigUint, anyhow::Error> {
        let start = Instant::now();
        let mut cached_value = self.gas_price_cache.lock().await;

        if let Some((cached_gas_price, cache_time)) = cached_value.take() {
            if cache_time.elapsed() < Duration::from_secs(API_PRICE_EXPIRATION_TIME_SECS as u64) {
                *cached_value = Some((cached_gas_price.clone(), cache_time));
                return Ok(cached_gas_price);
            }
        }
        drop(cached_value);

        let mut storage = self
            .db_pool
            .access_storage()
            .await
            .map_err(|e| format_err!("Can't access storage: {}", e))?;
        let average_gas_price = storage
            .ethereum_schema()
            .load_average_gas_price()
            .await?
            .unwrap_or_default()
            .as_u64();
        let average_gas_price = BigUint::from(average_gas_price);

        *self.gas_price_cache.lock().await = Some((average_gas_price.clone(), Instant::now()));
        metrics::histogram!("ticker.get_gas_price_wei", start.elapsed());
        Ok(average_gas_price)
    }

    async fn get_token(&self, token: TokenLike) -> Result<Token, anyhow::Error> {
        let start = Instant::now();
        let result = self
            .token_db_cache
            .get_token(&mut self.db_pool.access_storage().await?, token.clone())
            .await?
            .ok_or_else(|| format_err!("Token not found: {:?}", token));
        metrics::histogram!("ticker.get_token", start.elapsed());
        result
    }

    async fn keep_price_updated(self) {
        loop {
            if let Ok(tokens) = self.get_all_tokens().await {
                for token in &tokens {
                    if let Err(e) = self.update_price(token).await {
                        vlog::error!(
                            "Can't update price for token {}. Error: {}",
                            token.symbol,
                            e
                        );
                    };
                }
            } else {
                vlog::warn!("Can't get info from the database; waiting for the next iteration");
            };
            tokio::time::sleep(Duration::from_secs(UPDATE_PRICE_INTERVAL_SECS)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bigdecimal::ToPrimitive;
    use std::env;
    use zksync_types::{Address, Token, TokenId, TokenKind, TokenPrice};

    #[tokio::test]
    async fn should_return_one_for_rdoc() {
        const DATABASE_URL: &str = "postgres://postgres@localhost/plasma";
        const RDOC_SYMBOL: &str = "RDOC";
        const RDOC_VALUE: u32 = 1;

        env::set_var("DATABASE_URL", DATABASE_URL);

        struct FakeTickerApi;

        #[async_trait::async_trait]
        impl TokenPriceAPI for FakeTickerApi {
            async fn get_price(&self, _token: &Token) -> Result<TokenPrice, PriceError> {
                Err(PriceError::token_not_found("Wrong token"))
            }
        }

        let token = TokenLike::Symbol(String::from(RDOC_SYMBOL));

        let connection_pool = ConnectionPool::new(Some(1));
        let ticker_api = TickerApi::new(connection_pool, FakeTickerApi);

        let actual_qoute = FeeTickerAPI::get_last_quote(&ticker_api, token)
            .await
            .unwrap();

        assert_eq!(actual_qoute.usd_price.to_u32().unwrap(), RDOC_VALUE);
    }

    #[tokio::test]
    async fn should_return_value_from_cache() {
        const DATABASE_URL: &str = "postgres://postgres@localhost/plasma";
        const TEST_TOKEN_SYMBOL: &str = "TEST";
        const TOKEN_VALUE: u32 = 5;

        env::set_var("DATABASE_URL", DATABASE_URL);

        struct FakeTickerApi;

        #[async_trait::async_trait]
        impl TokenPriceAPI for FakeTickerApi {
            async fn get_price(&self, _token: &Token) -> Result<TokenPrice, PriceError> {
                Err(PriceError::token_not_found("Wrong token"))
            }
        }

        let test_token_symbol = TokenLike::Symbol(String::from(TEST_TOKEN_SYMBOL));

        let test_token_price = TokenPrice {
            usd_price: Ratio::from_integer(TOKEN_VALUE.into()),
            last_updated: Utc::now(),
        };

        let test_token = Token {
            id: TokenId(2),
            address: Address::random(),
            symbol: String::from(TEST_TOKEN_SYMBOL),
            decimals: 18,
            kind: TokenKind::ERC20,
            is_nft: false,
        };

        let connection_pool = ConnectionPool::new(Some(1));
        let ticker_api = TickerApi::new(connection_pool, FakeTickerApi);

        let price_cache_map = HashMap::from([(
            test_token.id,
            TokenCacheEntry::new(test_token_price.clone(), Instant::now()),
        )]);
        let price_cache = Arc::new(Mutex::new(price_cache_map));

        let ticker_api = TickerApi::with_price_cache(ticker_api, price_cache);

        let actual_qoute = FeeTickerAPI::get_last_quote(&ticker_api, test_token_symbol)
            .await
            .unwrap();

        assert_eq!(actual_qoute.usd_price.to_u32().unwrap(), TOKEN_VALUE);
    }
}
