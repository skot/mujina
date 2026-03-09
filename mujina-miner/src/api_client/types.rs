//! API data transfer objects.
//!
//! These types define the API contract shared between the server and
//! clients (CLI, TUI). See `docs/api.md` (at the repository root)
//! for the full API contract documentation, including conventions
//! for null values and units.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Full miner state snapshot.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct MinerState {
    pub uptime_secs: u64,
    /// Aggregate hashrate in hashes per second.
    pub hashrate: u64,
    pub shares_submitted: u64,
    pub paused: bool,
    pub boards: Vec<BoardState>,
    pub sources: Vec<SourceState>,
}

/// Board status.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct BoardState {
    /// URL-friendly identifier (e.g. "bitaxe-e2f56f9b").
    pub name: String,
    pub model: String,
    pub serial: Option<String>,
    /// Configured board operating frequency in MHz, if known.
    pub frequency_mhz: Option<f32>,
    /// Number of hashboard channels exposed by this board, if known.
    pub hashboard_count: Option<u8>,
    /// Number of hashboard channels currently active in Mujina, if known.
    pub active_hashboard_count: Option<u8>,
    pub fans: Vec<Fan>,
    pub hashboards: Vec<HashboardState>,
    pub temperatures: Vec<TemperatureSensor>,
    pub powers: Vec<PowerMeasurement>,
    pub threads: Vec<ThreadState>,
}

/// Per-hashboard connectivity and activity state.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct HashboardState {
    pub index: u8,
    /// Serial port assigned to this hashboard data channel, if known.
    pub serial_port: Option<String>,
    /// Whether the bridge currently detects a hashboard on this channel.
    pub is_present: bool,
    /// Whether Mujina has started a worker thread for this channel.
    pub is_active: bool,
    /// Hashrate in hashes per second for this hashboard.
    pub hashrate: u64,
}

/// Fan status.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Fan {
    pub name: String,
    /// Measured RPM, or null if the tachometer read failed.
    pub rpm: Option<u32>,
    /// Measured duty cycle, or null if the read failed.
    pub percent: Option<u8>,
    /// Target duty cycle, or null if the fan is in automatic mode.
    pub target_percent: Option<u8>,
}

/// Temperature sensor reading.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct TemperatureSensor {
    pub name: String,
    pub temperature_c: Option<f32>,
}

/// Voltage, current, and power from a single measurement point.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct PowerMeasurement {
    pub name: String,
    pub voltage_v: Option<f32>,
    pub current_a: Option<f32>,
    pub power_w: Option<f32>,
}

/// Per-thread runtime status.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ThreadState {
    pub name: String,
    /// Hashrate in hashes per second.
    pub hashrate: u64,
    pub is_active: bool,
}

/// Writable fields for `PATCH /api/v0/miner`.
///
/// All fields are optional; only those present in the request body are
/// applied. Read-only fields like `uptime_secs` and `hashrate` are not
/// included and cannot be set.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct MinerPatchRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused: Option<bool>,
}

/// Request body for setting a fan's target duty cycle.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct SetFanTargetRequest {
    /// Target duty cycle percentage (0--100), or null for automatic control.
    pub target_percent: Option<u8>,
}

/// Job source status.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct SourceState {
    pub name: String,
    /// Connection URL (e.g. "stratum+tcp://pool:3333"), if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Current share difficulty set by the source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub difficulty: Option<u64>,
}
