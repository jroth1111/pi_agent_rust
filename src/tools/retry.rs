//! HTTP retry with exponential backoff for transient errors.

use crate::error::{Error, Result};
use crate::http::client::{Client, Response};
use std::time::Duration;

/// Retry configuration.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retries (default: 3).
    pub max_retries: u32,
    /// Base delay in milliseconds (default: 500).
    pub base_delay_ms: u64,
    /// Maximum delay in milliseconds (default: 5000).
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay_ms: 500,
            max_delay_ms: 5000,
        }
    }
}

impl RetryConfig {
    /// Create a builder for custom configuration.
    pub fn builder() -> RetryConfigBuilder {
        RetryConfigBuilder::default()
    }
}

#[derive(Default)]
pub struct RetryConfigBuilder {
    max_retries: Option<u32>,
    base_delay_ms: Option<u64>,
    max_delay_ms: Option<u64>,
}

impl RetryConfigBuilder {
    pub const fn max_retries(mut self, n: u32) -> Self {
        self.max_retries = Some(n);
        self
    }

    pub const fn base_delay_ms(mut self, ms: u64) -> Self {
        self.base_delay_ms = Some(ms);
        self
    }

    pub const fn max_delay_ms(mut self, ms: u64) -> Self {
        self.max_delay_ms = Some(ms);
        self
    }

    pub fn build(self) -> RetryConfig {
        RetryConfig {
            max_retries: self.max_retries.unwrap_or(3),
            base_delay_ms: self.base_delay_ms.unwrap_or(500),
            max_delay_ms: self.max_delay_ms.unwrap_or(5000),
        }
    }
}

/// Execute a request with retry logic using a factory closure.
///
/// Since `RequestBuilder` takes `self` by value and cannot be cloned,
/// we use a closure that creates a fresh builder for each attempt.
///
/// # Example
/// ```ignore
/// let config = RetryConfig::default();
/// let response = fetch_with_retry(&client, config, |c| c.get(url)).await?;
/// ```
pub async fn fetch_with_retry<F, Fut>(
    client: &Client,
    config: &RetryConfig,
    build_request: F,
) -> Result<Response>
where
    F: Fn(&Client) -> Fut,
    Fut: std::future::Future<Output = Result<Response>>,
{
    let mut delay = config.base_delay_ms;

    for attempt in 0..=config.max_retries {
        match build_request(client).await {
            Ok(resp) => {
                let status = resp.status();
                // Retry on 5xx server errors
                if status >= 500 && attempt < config.max_retries {
                    asupersync::time::sleep(
                        asupersync::time::wall_now(),
                        Duration::from_millis(delay),
                    )
                    .await;
                    delay = (delay * 2).min(config.max_delay_ms);
                    continue;
                }
                return Ok(resp);
            }
            Err(_e) if attempt < config.max_retries => {
                // Retry on transient errors
                asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(delay))
                    .await;
                delay = (delay * 2).min(config.max_delay_ms);
                continue;
            }
            Err(e) => return Err(e),
        }
    }

    Err(Error::tool("retry", "Max retries exceeded".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_config_default() {
        let config = RetryConfig::default();
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.base_delay_ms, 500);
        assert_eq!(config.max_delay_ms, 5000);
    }

    #[test]
    fn retry_config_builder() {
        let config = RetryConfig::builder()
            .max_retries(5)
            .base_delay_ms(100)
            .max_delay_ms(10000)
            .build();
        assert_eq!(config.max_retries, 5);
        assert_eq!(config.base_delay_ms, 100);
        assert_eq!(config.max_delay_ms, 10000);
    }

    #[test]
    fn retry_config_builder_partial() {
        let config = RetryConfig::builder().max_retries(10).build();
        assert_eq!(config.max_retries, 10);
        assert_eq!(config.base_delay_ms, 500); // default
        assert_eq!(config.max_delay_ms, 5000); // default
    }
}
