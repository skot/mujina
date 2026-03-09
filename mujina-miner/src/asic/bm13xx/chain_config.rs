//! Chain configuration for BM13xx hash thread.
//!
//! This module defines the configuration that boards provide to the hash thread
//! implementation. A board may have multiple chains, each with its own config.
//!
//! # Example
//!
//! ```ignore
//! // Board creates configuration
//! let config = ChainConfig {
//!     name: "EmberOne".to_string(),
//!     topology: TopologySpec::individual_domains(12, false),
//!     chip_config: bm13xx::bm1362(),
//!     peripherals: ChainPeripherals {
//!         asic_enable: Arc::new(Mutex::new(gpio_enable)),
//!         voltage_regulator: Some(Arc::new(Mutex::new(tps546))),
//!     },
//! };
//!
//! // Hash thread uses configuration
//! let mut chain = Chain::from_topology(&config.topology);
//! chain.assign_addresses()?;
//! let sequencer = Sequencer::new(config.chip_config.clone());
//! ```

use std::sync::Arc;

use tokio::sync::Mutex;

use super::chip_config::ChipConfig;
use super::topology::TopologySpec;

// Re-export from parent module
pub use crate::asic::hash_thread::{AsicEnable, VoltageRegulator};

/// Configuration for a BM13xx ASIC chain.
///
/// Provided by the board, used by the hash thread implementation. Contains
/// all information needed to initialize and operate the chain.
pub struct ChainConfig {
    /// Human-readable name for logging.
    pub name: String,

    /// Physical topology (chain routing, domains).
    pub topology: TopologySpec,

    /// Chip configuration with board-specific overrides.
    pub chip_config: ChipConfig,

    /// Hardware control interfaces for this chain.
    pub peripherals: ChainPeripherals,
}

/// Hardware interfaces for a chain.
///
/// These are trait objects with Arc because:
/// - The board may retain control over enable (shared with hash thread)
/// - Voltage regulators may be shared among multiple chains/threads
/// - Shared ownership naturally uses Arc<dyn Trait> with type erasure
///
/// Uses `tokio::sync::Mutex` rather than `std::sync::Mutex` to avoid
/// blocking worker threads. While peripheral I/O is fast, the async mutex
/// ensures we never accidentally block other tasks on the same worker.
pub struct ChainPeripherals {
    /// Enable/disable the ASIC chain.
    ///
    /// This abstraction covers different mechanisms:
    /// - Reset GPIO (assert = inactive/low-power, deassert = active)
    /// - Power enable (cut power = inactive, power on = active)
    /// - Board-specific implementations
    ///
    /// When disabled, chips are in low-power state. When enabled, chips
    /// need full re-initialization (all configuration is lost).
    pub asic_enable: Arc<Mutex<dyn AsicEnable + Send>>,

    /// Voltage regulator control (optional, may be shared across chains).
    pub voltage_regulator: Option<Arc<Mutex<dyn VoltageRegulator + Send>>>,

    /// Serializes full cold-start initialization when multiple chains share
    /// board-level control hardware such as a PSU or reset/control bridge.
    pub initialization_lock: Arc<Mutex<()>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    /// Mock ASIC enable for testing.
    struct MockAsicEnable {
        enabled: bool,
    }

    #[async_trait]
    impl AsicEnable for MockAsicEnable {
        async fn enable(&mut self) -> anyhow::Result<()> {
            self.enabled = true;
            Ok(())
        }

        async fn disable(&mut self) -> anyhow::Result<()> {
            self.enabled = false;
            Ok(())
        }
    }

    #[tokio::test]
    async fn peripherals_can_be_shared() {
        let enable = Arc::new(Mutex::new(MockAsicEnable { enabled: false }));

        // Simulate board keeping a reference
        let board_ref = Arc::clone(&enable);

        // Hash thread gets its reference via ChainPeripherals
        let peripherals = ChainPeripherals {
            asic_enable: enable,
            voltage_regulator: None,
            initialization_lock: Arc::new(Mutex::new(())),
        };

        // Hash thread enables
        peripherals.asic_enable.lock().await.enable().await.unwrap();

        // Board can observe the state change
        assert!(board_ref.lock().await.enabled);
    }
}
