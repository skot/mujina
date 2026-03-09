//! S19j Pro hashboard support via Bitcrane v3.
//!
//! A Bitcrane v3 bridge exposes one management channel plus three independent
//! ASIC UART channels for three Antminer S19j Pro hashboards. Mujina models
//! that as a single board with shared power and cooling, and one BM13xx thread
//! per populated hashboard channel.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::RwLock;
use tokio::sync::{Mutex, watch};
use tokio_serial::SerialPortBuilderExt;
use tokio_util::codec::{FramedRead, FramedWrite};

use super::{
    Board, BoardDescriptor, BoardError, BoardInfo,
    pattern::{BoardPattern, Match, StringMatch},
};
use crate::{
    api_client::types::{
        BoardState, Fan, HashboardState, MinerState, PowerMeasurement, TemperatureSensor,
        ThreadState,
    },
    asic::{
        bm13xx::{
            self,
            chain_config::{ChainConfig, ChainPeripherals, VoltageRegulator},
            chip_config, thread_v2,
            topology::TopologySpec,
        },
        hash_thread::{AsicEnable, HashThread, HashThreadStatus},
    },
    error::Error,
    hw_trait::gpio::{GpioPin, PinValue},
    mgmt_protocol::{
        Apw12Psu, ControlChannel,
        bitcrane::{
            display::BitcraneDisplay,
            fan::{self, BitcraneFan},
            gpio::{BitcraneGpioController, BitcraneGpioPin, BitcraneGpioPinHandle},
            i2c::BitcraneI2c,
        },
    },
    peripheral::tmp75::{self, Tmp75},
    tracing::prelude::*,
    transport::{UsbDeviceInfo, serial::SerialStream},
};

const S19J_PRO_TARGET_FREQ_MHZ: f32 = 500.0;
const APW12_POWER_RAIL_NAME: &str = "APW12";
const HASHBOARD_COUNT: usize = 3;

const TELEMETRY_INTERVAL: Duration = Duration::from_secs(2);
const DISPLAY_UPDATE_INTERVAL: u32 = 5;

const FAN_MIN_PERCENT: u8 = 45;
const FAN_FAILSAFE_PERCENT: u8 = 80;
const FAN_MAX_PERCENT: u8 = 100;
const FAN_TARGET_TEMP_C: f32 = 62.0;
const FAN_FULL_SPEED_TEMP_C: f32 = 68.0;
const FAN_MAX_TEMP_C: f32 = 70.0;
const FAN_PID_KP: f32 = 6.0;
const FAN_PID_KI: f32 = 0.35;
const FAN_PID_KD: f32 = 3.0;

inventory::submit! {
    BoardDescriptor {
        pattern: BoardPattern {
            vid: Match::Any,
            pid: Match::Any,
            bcd_device: Match::Any,
            manufacturer: Match::Specific(StringMatch::Regex("^(256F|OSMU)$")),
            product: Match::Specific(StringMatch::Regex("^(bitcrane_S19jpro|bitcrane3)$")),
            serial_pattern: Match::Any,
        },
        name: "S19j Pro",
        create_fn: |device| Box::pin(create_from_usb(device)),
    }
}

#[derive(Clone)]
struct S19jProHashboard {
    index: u8,
    data_port_path: String,
    reset_pin: Option<BitcraneGpioPinHandle>,
    plug_pin: Option<BitcraneGpioPinHandle>,
    temp_sensors: Option<(Tmp75, Tmp75)>,
    is_present: bool,
    is_active: bool,
}

impl S19jProHashboard {
    fn state(&self, hashrate: u64) -> HashboardState {
        HashboardState {
            index: self.index,
            serial_port: Some(self.data_port_path.clone()),
            is_present: self.is_present,
            is_active: self.is_active,
            hashrate,
        }
    }
}

#[derive(Clone)]
struct S19jProThreadMonitor {
    index: u8,
    name: String,
    status: Arc<RwLock<HashThreadStatus>>,
}

/// S19j Pro board with shared PSU/fans and up to three hashboard channels.
pub struct S19jPro {
    device_info: UsbDeviceInfo,
    hashboards: Vec<S19jProHashboard>,
    thread_monitors: Vec<S19jProThreadMonitor>,
    control_channel: ControlChannel,
    psu: Option<Arc<Mutex<Apw12Psu>>>,
    state_tx: watch::Sender<BoardState>,
    miner_state_rx: Option<watch::Receiver<MinerState>>,
}

impl S19jPro {
    pub fn new(
        device_info: UsbDeviceInfo,
        control_channel: ControlChannel,
        data_port_paths: Vec<String>,
        state_tx: watch::Sender<BoardState>,
    ) -> Self {
        let hashboards = data_port_paths
            .into_iter()
            .enumerate()
            .map(|(index, data_port_path)| S19jProHashboard {
                index: index as u8,
                data_port_path,
                reset_pin: None,
                plug_pin: None,
                temp_sensors: None,
                is_present: false,
                is_active: false,
            })
            .collect();

        Self {
            device_info,
            hashboards,
            thread_monitors: Vec::new(),
            control_channel,
            psu: None,
            state_tx,
            miner_state_rx: None,
        }
    }

    pub async fn initialize(&mut self) -> Result<(), BoardError> {
        let gpio = BitcraneGpioController::new(self.control_channel.clone());
        let i2c = BitcraneI2c::new(self.control_channel.clone());

        for hashboard in &mut self.hashboards {
            let mut reset_pin = gpio.pin(reset_pin_for_index(hashboard.index));
            reset_pin.write(PinValue::Low).await.map_err(|e| {
                BoardError::InitializationFailed(format!(
                    "Failed to assert reset for HB{}: {}",
                    hashboard.index, e
                ))
            })?;

            let mut plug_pin = gpio.pin(plug_pin_for_index(hashboard.index));
            hashboard.is_present = match plug_pin.read().await {
                Ok(PinValue::High) => true,
                Ok(PinValue::Low) => false,
                Err(e) => {
                    warn!(hashboard = hashboard.index, error = %e, "Plug detect failed; assuming present");
                    true
                }
            };

            hashboard.reset_pin = Some(reset_pin);
            hashboard.plug_pin = Some(plug_pin);
            hashboard.temp_sensors =
                Some(tmp75::sensors_for_hashboard(i2c.clone(), hashboard.index));
        }

        debug!("Initializing APW12 PSU");
        let mut psu = Apw12Psu::new(self.control_channel.clone());
        psu.set_enabled(true).await.map_err(|e| {
            BoardError::InitializationFailed(format!("Failed to enable PSU: {}", e))
        })?;
        tokio::time::sleep(Duration::from_millis(500)).await;

        psu.config_watchdog(0x00).await.map_err(|e| {
            BoardError::InitializationFailed(format!("Failed to configure PSU watchdog: {}", e))
        })?;

        const DEFAULT_VOUT: f32 = 12.6;
        psu.set_voltage(DEFAULT_VOUT).await.map_err(|e| {
            BoardError::InitializationFailed(format!("Failed to set PSU voltage: {}", e))
        })?;

        info!("APW12 PSU enabled, voltage set to {}V", DEFAULT_VOUT);
        tokio::time::sleep(Duration::from_millis(2000)).await;

        match psu.measure_voltage().await {
            Ok(v) => info!("APW12 measured voltage: {:.2}V", v),
            Err(e) => warn!("Failed to measure PSU voltage: {}", e),
        }

        self.psu = Some(Arc::new(Mutex::new(psu)));

        let fans = fan::all_fans(self.control_channel.clone());
        for fan in &fans {
            if let Err(e) = fan.set_speed(FAN_FAILSAFE_PERCENT).await {
                warn!(fan = %fan.name(), error = %e, "Failed to set initial fan speed");
            }
        }
        info!("Fans initialized at {}% speed", FAN_FAILSAFE_PERCENT);

        self.publish_hashboard_state();
        info!("S19j Pro initialized successfully");
        Ok(())
    }

    fn publish_hashboard_state(&self) {
        let hashboards: Vec<HashboardState> = self.hashboard_states();
        let active_hashboard_count = hashboards
            .iter()
            .filter(|hashboard| hashboard.is_active)
            .count() as u8;

        self.state_tx.send_modify(|state| {
            state.frequency_mhz = Some(S19J_PRO_TARGET_FREQ_MHZ);
            state.hashboard_count = Some(hashboard_count_u8(hashboards.len()));
            state.active_hashboard_count = Some(active_hashboard_count);
            state.hashboards = hashboards.clone();
        });
    }

    fn hashboard_states(&self) -> Vec<HashboardState> {
        let hashboard_hashrates = self.hashboard_hashrates();
        self.hashboards
            .iter()
            .map(|hashboard| {
                let hashrate = hashboard_hashrates
                    .iter()
                    .find(|(index, _)| *index == hashboard.index)
                    .map(|(_, hashrate)| *hashrate)
                    .unwrap_or(0);
                hashboard.state(hashrate)
            })
            .collect()
    }

    fn hashboard_hashrates(&self) -> Vec<(u8, u64)> {
        self.thread_monitors
            .iter()
            .map(|monitor| {
                let status = monitor.status.read();
                (monitor.index, effective_hashrate(&status))
            })
            .collect()
    }
}

#[async_trait]
impl Board for S19jPro {
    fn board_info(&self) -> BoardInfo {
        BoardInfo {
            model: "S19j Pro".to_string(),
            firmware_version: None,
            serial_number: self.device_info.serial_number.clone(),
        }
    }

    async fn shutdown(&mut self) -> Result<(), BoardError> {
        for fan in fan::all_fans(self.control_channel.clone()) {
            if let Err(e) = fan.set_speed(0).await {
                warn!(fan = %fan.name(), error = %e, "Failed to set fan to 0% on shutdown");
            }
        }

        for hashboard in &mut self.hashboards {
            hashboard.is_active = false;
            if let Some(reset_pin) = &mut hashboard.reset_pin {
                if let Err(e) = reset_pin.write(PinValue::Low).await {
                    warn!(hashboard = hashboard.index, error = %e, "Failed to assert reset on shutdown");
                }
            }
        }

        if let Some(psu) = &self.psu {
            if let Err(e) = psu.lock().await.set_enabled(false).await {
                warn!("Failed to disable PSU on shutdown: {}", e);
            }
        }

        self.publish_hashboard_state();
        info!("S19j Pro shutdown complete");
        Ok(())
    }

    async fn create_hash_threads(&mut self) -> Result<Vec<Box<dyn HashThread>>, BoardError> {
        let mut threads: Vec<Box<dyn HashThread>> = Vec::new();
        let initialization_lock = Arc::new(Mutex::new(()));

        for hashboard in &mut self.hashboards {
            if !hashboard.is_present {
                info!(
                    hashboard = hashboard.index,
                    "Skipping unpopulated hashboard channel"
                );
                continue;
            }

            let Some(reset_pin) = hashboard.reset_pin.clone() else {
                return Err(BoardError::InitializationFailed(format!(
                    "Reset pin not initialized for HB{}",
                    hashboard.index
                )));
            };

            let data_stream = match SerialStream::new(&hashboard.data_port_path, 115200) {
                Ok(stream) => stream,
                Err(e) => {
                    warn!(
                        hashboard = hashboard.index,
                        port = %hashboard.data_port_path,
                        error = %e,
                        "Failed to open data port"
                    );
                    continue;
                }
            };
            let (data_reader, data_writer, data_control) = data_stream.split();

            if let Err(e) = data_control.flush_input() {
                warn!(
                    hashboard = hashboard.index,
                    port = %hashboard.data_port_path,
                    error = %e,
                    "Failed to flush serial buffer"
                );
                continue;
            }

            let chip_rx = FramedRead::new(data_reader, bm13xx::FrameCodec);
            let chip_tx = FramedWrite::new(data_writer, bm13xx::FrameCodec);

            let serial_prefix = self
                .device_info
                .serial_number
                .as_deref()
                .unwrap_or("unknown");
            let thread_name = format!("S19jPro-{}-HB{}", serial_prefix, hashboard.index);

            let voltage_regulator: Option<Arc<Mutex<dyn VoltageRegulator + Send>>> = self
                .psu
                .as_ref()
                .map(|psu| Arc::clone(psu) as Arc<Mutex<dyn VoltageRegulator + Send>>);

            let config = ChainConfig {
                name: thread_name,
                topology: TopologySpec::uniform_domains(42, 3, false),
                chip_config: chip_config::bm1362(),
                peripherals: ChainPeripherals {
                    asic_enable: Arc::new(Mutex::new(S19jProAsicEnable { reset_pin })),
                    voltage_regulator,
                    initialization_lock: Arc::clone(&initialization_lock),
                },
            };

            match thread_v2::BM13xxThread::new(chip_rx, chip_tx, config) {
                Ok(thread) => {
                    self.thread_monitors.push(S19jProThreadMonitor {
                        index: hashboard.index,
                        name: thread.name().to_string(),
                        status: thread.status_handle(),
                    });
                    hashboard.is_active = true;
                    info!(
                        hashboard = hashboard.index,
                        port = %hashboard.data_port_path,
                        "Started S19j Pro hashboard thread"
                    );
                    threads.push(Box::new(thread));
                }
                Err(e) => {
                    warn!(
                        hashboard = hashboard.index,
                        port = %hashboard.data_port_path,
                        error = %e,
                        "Failed to create S19j Pro hashboard thread"
                    );
                }
            }
        }

        if threads.is_empty() {
            return Err(BoardError::InitializationFailed(
                "No populated S19j Pro hashboards could be started".to_string(),
            ));
        }

        self.publish_hashboard_state();

        let telemetry_hardware = self.hashboards.clone();
        let thread_monitors = self.thread_monitors.clone();
        let psu = self.psu.as_ref().map(Arc::clone);
        let fans = fan::all_fans(self.control_channel.clone());
        let display = BitcraneDisplay::new(self.control_channel.clone());
        let state_tx = self.state_tx.clone();
        let miner_state_rx = self.miner_state_rx.clone();
        tokio::spawn(async move {
            telemetry_task(
                telemetry_hardware,
                thread_monitors,
                psu,
                fans,
                display,
                state_tx,
                miner_state_rx,
            )
            .await;
        });

        Ok(threads)
    }

    fn set_miner_state_rx(&mut self, rx: watch::Receiver<MinerState>) {
        self.miner_state_rx = Some(rx);
    }
}

async fn telemetry_task(
    mut hashboards: Vec<S19jProHashboard>,
    thread_monitors: Vec<S19jProThreadMonitor>,
    psu: Option<Arc<Mutex<Apw12Psu>>>,
    fans: [BitcraneFan; 4],
    display: BitcraneDisplay,
    state_tx: watch::Sender<BoardState>,
    miner_state_rx: Option<watch::Receiver<MinerState>>,
) {
    let mut cycle_count: u32 = 0;
    let mut pid = FanPidController::default();
    let mut fan_target_percent = FAN_FAILSAFE_PERCENT;

    loop {
        let mut temperatures: Vec<TemperatureSensor> = Vec::with_capacity(hashboards.len() * 2);
        let mut hashboard_states: Vec<HashboardState> = Vec::with_capacity(hashboards.len());
        let mut thread_states: Vec<ThreadState> = Vec::with_capacity(thread_monitors.len());
        let mut max_temp_c: Option<f32> = None;
        let hashrates_by_index: Vec<(u8, bool, u64)> = thread_monitors
            .iter()
            .map(|monitor| {
                let status = monitor.status.read();
                let hashrate = effective_hashrate(&status);
                (monitor.index, status.is_active, hashrate)
            })
            .collect();

        for monitor in &thread_monitors {
            let status = monitor.status.read();
            let hashrate = effective_hashrate(&status);
            thread_states.push(ThreadState {
                name: monitor.name.clone(),
                hashrate,
                is_active: status.is_active,
            });
        }

        for hashboard in &mut hashboards {
            if let Some(plug_pin) = &mut hashboard.plug_pin {
                hashboard.is_present = match plug_pin.read().await {
                    Ok(PinValue::High) => true,
                    Ok(PinValue::Low) => false,
                    Err(e) => {
                        debug!(hashboard = hashboard.index, error = %e, "Plug detect read failed");
                        hashboard.is_present
                    }
                };
            }

            if let Some((temp0, temp1)) = &hashboard.temp_sensors {
                let temp0_value = temp0
                    .read_temperature()
                    .await
                    .inspect_err(|e| debug!(sensor = %temp0.name(), error = %e, "Temp read failed"))
                    .ok();
                let temp1_value = temp1
                    .read_temperature()
                    .await
                    .inspect_err(|e| debug!(sensor = %temp1.name(), error = %e, "Temp read failed"))
                    .ok();

                if let Some(temp) = temp0_value {
                    max_temp_c = Some(max_temp_c.map_or(temp, |max| max.max(temp)));
                }
                if let Some(temp) = temp1_value {
                    max_temp_c = Some(max_temp_c.map_or(temp, |max| max.max(temp)));
                }

                temperatures.push(TemperatureSensor {
                    name: temp0.name().to_string(),
                    temperature_c: temp0_value,
                });
                temperatures.push(TemperatureSensor {
                    name: temp1.name().to_string(),
                    temperature_c: temp1_value,
                });
            }

            let (is_active, hashrate) = hashrates_by_index
                .iter()
                .find(|(index, _, _)| *index == hashboard.index)
                .map(|(_, is_active, hashrate)| (*is_active, *hashrate))
                .unwrap_or((false, 0));
            hashboard.is_active = is_active;
            hashboard_states.push(hashboard.state(hashrate));
        }

        let next_target_percent = pid.next_target_percent(max_temp_c, TELEMETRY_INTERVAL);
        if next_target_percent != fan_target_percent {
            for fan in &fans {
                if let Err(e) = fan.set_speed(next_target_percent).await {
                    warn!(fan = %fan.name(), error = %e, "Failed to set fan speed");
                }
            }
            debug!(
                target_percent = next_target_percent,
                max_temp_c, "Updated S19j Pro fan target"
            );
            fan_target_percent = next_target_percent;
        }

        if let Some(max_temp_c) = max_temp_c {
            if max_temp_c >= FAN_MAX_TEMP_C {
                warn!(
                    max_temp_c,
                    target_percent = fan_target_percent,
                    "S19j Pro temperature reached the 70C ceiling"
                );
            }
        }

        let mut fan_states = Vec::with_capacity(4);
        for fan in &fans {
            let rpm_result = fan.read_rpm().await;
            fan_states.push(Fan {
                name: fan.name().to_string(),
                rpm: rpm_result
                    .inspect_err(|e| debug!(fan = %fan.name(), error = %e, "Fan RPM read failed"))
                    .ok(),
                percent: None,
                target_percent: Some(fan_target_percent),
            });
        }

        let voltage_v = if let Some(psu) = &psu {
            let mut psu = psu.lock().await;
            psu.measure_voltage()
                .await
                .inspect_err(
                    |e| debug!(rail = APW12_POWER_RAIL_NAME, error = %e, "Voltage read failed"),
                )
                .ok()
        } else {
            None
        };

        let powers = vec![PowerMeasurement {
            name: APW12_POWER_RAIL_NAME.to_string(),
            voltage_v,
            current_a: None,
            power_w: None,
        }];

        let active_hashboard_count = hashboard_states
            .iter()
            .filter(|hashboard| hashboard.is_active)
            .count() as u8;

        state_tx.send_modify(|state| {
            state.frequency_mhz = Some(S19J_PRO_TARGET_FREQ_MHZ);
            state.hashboard_count = Some(hashboard_count_u8(hashboard_states.len()));
            state.active_hashboard_count = Some(active_hashboard_count);
            state.hashboards = hashboard_states.clone();
            state.temperatures = temperatures.clone();
            state.fans = fan_states.clone();
            state.powers = powers.clone();
            state.threads = thread_states.clone();
        });

        if cycle_count % DISPLAY_UPDATE_INTERVAL == 0 {
            let hashrate_gh = miner_state_rx
                .as_ref()
                .map(|rx| rx.borrow().hashrate as f64 / 1_000_000_000.0)
                .unwrap_or(0.0);

            if let Err(e) = display.display_hashrate(hashrate_gh).await {
                debug!(error = %e, "Failed to update OLED display");
            }
        }
        cycle_count = cycle_count.wrapping_add(1);

        tokio::time::sleep(TELEMETRY_INTERVAL).await;
    }
}

#[derive(Default)]
struct FanPidController {
    integral: f32,
    previous_error: Option<f32>,
}

impl FanPidController {
    fn next_target_percent(&mut self, max_temp_c: Option<f32>, dt: Duration) -> u8 {
        let Some(max_temp_c) = max_temp_c else {
            self.integral = 0.0;
            self.previous_error = None;
            return FAN_FAILSAFE_PERCENT;
        };

        if max_temp_c >= FAN_FULL_SPEED_TEMP_C {
            self.integral = 0.0;
            self.previous_error = Some(max_temp_c - FAN_TARGET_TEMP_C);
            return FAN_MAX_PERCENT;
        }

        let dt_secs = dt.as_secs_f32().max(1.0);
        let error = max_temp_c - FAN_TARGET_TEMP_C;
        self.integral = (self.integral + error * dt_secs).clamp(0.0, 40.0);
        let derivative = self
            .previous_error
            .map(|previous_error| (error - previous_error) / dt_secs)
            .unwrap_or(0.0);
        self.previous_error = Some(error);

        let output = FAN_MIN_PERCENT as f32
            + FAN_PID_KP * error.max(0.0)
            + FAN_PID_KI * self.integral
            + FAN_PID_KD * derivative.max(0.0);

        output
            .round()
            .clamp(FAN_MIN_PERCENT as f32, FAN_MAX_PERCENT as f32) as u8
    }
}

struct S19jProAsicEnable {
    reset_pin: BitcraneGpioPinHandle,
}

#[async_trait]
impl AsicEnable for S19jProAsicEnable {
    async fn enable(&mut self) -> anyhow::Result<()> {
        self.reset_pin
            .write(PinValue::High)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to release reset: {}", e))?;
        tokio::time::sleep(Duration::from_millis(2000)).await;
        Ok(())
    }

    async fn disable(&mut self) -> anyhow::Result<()> {
        self.reset_pin
            .write(PinValue::Low)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to assert reset: {}", e))
    }
}

async fn create_from_usb(
    device: UsbDeviceInfo,
) -> crate::error::Result<(Box<dyn Board + Send>, super::BoardRegistration)> {
    let serial_ports = device.serial_ports()?;

    if serial_ports.len() != HASHBOARD_COUNT + 1 {
        return Err(Error::Hardware(format!(
            "S19j Pro requires exactly 4 serial ports, found {}",
            serial_ports.len()
        )));
    }

    let control_port_path = serial_ports[0].clone();
    let data_port_paths = serial_ports[1..].to_vec();

    debug!(
        serial = ?device.serial_number,
        control = %control_port_path,
        data_ports = ?data_port_paths,
        "S19j Pro serial ports"
    );

    let control_port = tokio_serial::new(&control_port_path, 115200)
        .open_native_async()
        .map_err(|e| Error::Hardware(format!("Failed to open control port: {}", e)))?;
    let control_channel = ControlChannel::new(control_port);

    let serial = device.serial_number.clone();
    let initial_state = BoardState {
        name: format!("s19jpro-{}", serial.as_deref().unwrap_or("unknown")),
        model: "S19j Pro".into(),
        serial,
        frequency_mhz: Some(S19J_PRO_TARGET_FREQ_MHZ),
        hashboard_count: Some(HASHBOARD_COUNT as u8),
        active_hashboard_count: Some(0),
        hashboards: data_port_paths
            .iter()
            .enumerate()
            .map(|(index, data_port_path)| HashboardState {
                index: index as u8,
                serial_port: Some(data_port_path.clone()),
                is_present: false,
                is_active: false,
                hashrate: 0,
            })
            .collect(),
        ..Default::default()
    };
    let (state_tx, state_rx) = watch::channel(initial_state);

    let mut board = S19jPro::new(device, control_channel, data_port_paths, state_tx);
    board
        .initialize()
        .await
        .map_err(|e| Error::Hardware(format!("Failed to initialize board: {}", e)))?;

    let registration = super::BoardRegistration { state_rx };
    Ok((Box::new(board), registration))
}

fn reset_pin_for_index(index: u8) -> BitcraneGpioPin {
    match index {
        0 => BitcraneGpioPin::Rst0,
        1 => BitcraneGpioPin::Rst1,
        2 => BitcraneGpioPin::Rst2,
        _ => panic!("invalid hashboard index {}", index),
    }
}

fn plug_pin_for_index(index: u8) -> BitcraneGpioPin {
    match index {
        0 => BitcraneGpioPin::Plug0,
        1 => BitcraneGpioPin::Plug1,
        2 => BitcraneGpioPin::Plug2,
        _ => panic!("invalid hashboard index {}", index),
    }
}

fn hashboard_count_u8(count: usize) -> u8 {
    count.min(u8::MAX as usize) as u8
}

fn effective_hashrate(status: &HashThreadStatus) -> u64 {
    u64::from(status.hashrate)
}
