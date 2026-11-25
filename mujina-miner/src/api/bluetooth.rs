//! Nano3S Bluetooth speaker controls on the BlueZ system bus.

mod playback;

use std::{collections::HashMap, time::Duration};

use axum::{Json, Router, http::StatusCode, routing};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use tokio::{sync::Mutex, time::timeout};
use zbus::{
    Connection, Proxy,
    fdo::{ManagedObjects, ObjectManagerProxy},
    zvariant::{ObjectPath, OwnedObjectPath, Value},
};

use super::server::SharedState;

const BLUEZ: &str = "org.bluez";
const ADAPTER: &str = "/org/bluez/hci0";
const DEVICE: &str = "org.bluez.Device1";
const AUDIO_SINK: &str = "0000110b-0000-1000-8000-00805f9b34fb";
const AGENT: &str = "/org/mujina/BluetoothAgent";
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const ACTION_TIMEOUT: Duration = Duration::from_secs(30);
const SCAN_DURATION: Duration = Duration::from_secs(15);
static RADIO_ACTION: Mutex<()> = Mutex::const_new(());

type ApiError = (StatusCode, Json<JsonValue>);
type ApiResult<T> = Result<Json<T>, ApiError>;

#[derive(Serialize)]
struct Speaker {
    address: String,
    name: String,
    paired: bool,
    connected: bool,
    #[serde(skip)]
    path: OwnedObjectPath,
}

#[derive(Deserialize)]
struct SpeakerRequest {
    address: String,
}

// Require JSON for all radio-changing requests, including scans.
#[derive(Deserialize)]
struct ScanRequest {}

pub(super) fn routes() -> Router<SharedState> {
    Router::new()
        .route("/bluetooth/speakers", routing::get(speakers))
        .route("/bluetooth/scan", routing::post(scan))
        .route("/bluetooth/pair", routing::post(pair))
        .route("/bluetooth/connect", routing::post(connect))
        .merge(playback::routes())
}

fn error(status: StatusCode, message: impl Into<String>) -> ApiError {
    (status, Json(json!({ "error": message.into() })))
}

async fn audio_bus() -> Result<Connection, ApiError> {
    let ready = async {
        let bus = Connection::system().await?;
        let dbus = zbus::fdo::DBusProxy::new(&bus).await?;
        for name in [BLUEZ, "org.bluealsa"] {
            if !dbus.name_has_owner(name.try_into()?).await? {
                anyhow::bail!("{name} is not running");
            }
        }
        Ok::<_, anyhow::Error>(bus)
    };
    if let Ok(Ok(bus)) = timeout(CALL_TIMEOUT, ready).await {
        Ok(bus)
    } else {
        Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Bluetooth audio is unavailable. It starts after Wi-Fi setup when the audio services are installed.",
        ))
    }
}

async fn managed_objects(bus: &Connection) -> anyhow::Result<ManagedObjects> {
    Ok(ObjectManagerProxy::new(bus, BLUEZ, "/")
        .await?
        .get_managed_objects()
        .await?)
}

fn speaker_list(objects: &ManagedObjects) -> Vec<Speaker> {
    let mut speakers = Vec::new();
    for (path, interfaces) in objects {
        if !path.as_str().starts_with("/org/bluez/hci0/dev_") {
            continue;
        }
        let Some(device) = interfaces.get(DEVICE) else {
            continue;
        };
        let prop = |name| device.get(name);
        let flag = |name| {
            prop(name)
                .and_then(|v| bool::try_from(v).ok())
                .unwrap_or_default()
        };
        if prop("AddressType").and_then(|v| <&str>::try_from(v).ok()) != Some("public") {
            continue;
        }
        let paired = flag("Paired");
        let has_audio_sink = prop("UUIDs")
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| Vec::<String>::try_from(v).ok())
            .is_some_and(|uuids| {
                uuids
                    .iter()
                    .any(|uuid| uuid.eq_ignore_ascii_case(AUDIO_SINK))
            });
        // Classic inquiry exposes Class of Device before SDP exposes UUIDs.
        // Include speakers/headphones, Hi-Fi and portable audio until paired.
        let class = prop("Class")
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        if !(has_audio_sink || (!paired && audio_class(class))) {
            continue;
        }
        let Some(address) = prop("Address").and_then(|v| <&str>::try_from(v).ok()) else {
            continue;
        };
        let name = prop("Alias")
            .and_then(|v| <&str>::try_from(v).ok())
            .unwrap_or(address);
        let connected = flag("Connected")
            && objects.values().any(|interfaces| {
                interfaces
                    .get("org.bluez.MediaTransport1")
                    .and_then(|props| props.get("Device"))
                    .and_then(|value| <&ObjectPath>::try_from(value).ok())
                    .is_some_and(|device_path| device_path.as_str() == path.as_str())
            });
        speakers.push(Speaker {
            address: address.to_string(),
            name: name.to_string(),
            paired,
            connected,
            path: path.clone(),
        });
    }
    speakers.sort_by(|a, b| {
        b.connected
            .cmp(&a.connected)
            .then_with(|| b.paired.cmp(&a.paired))
            .then_with(|| a.name.cmp(&b.name))
    });
    speakers
}

fn audio_class(class: u32) -> bool {
    class & 0x1f00 == 0x0400 && matches!((class >> 2) & 0x3f, 1 | 2 | 5..=8 | 10 | 15)
}

fn valid_address(address: &str) -> bool {
    address.len() == 17
        && address.bytes().enumerate().all(|(i, b)| {
            if i % 3 == 2 {
                b == b':'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

async fn snapshot(bus: &Connection) -> ApiResult<Vec<Speaker>> {
    match timeout(CALL_TIMEOUT, managed_objects(bus)).await {
        Ok(Ok(objects)) if !objects.keys().any(|path| path.as_str() == ADAPTER) => Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Bluetooth controller hci0 is unavailable.",
        )),
        Ok(Ok(objects)) => Ok(Json(speaker_list(&objects))),
        Ok(Err(err)) => Err(error(StatusCode::SERVICE_UNAVAILABLE, err.to_string())),
        Err(_) => Err(error(
            StatusCode::GATEWAY_TIMEOUT,
            "Bluetooth status timed out.",
        )),
    }
}

async fn speakers() -> ApiResult<Vec<Speaker>> {
    snapshot(&audio_bus().await?).await
}

async fn scan(Json(_): Json<ScanRequest>) -> ApiResult<Vec<Speaker>> {
    let _guard = RADIO_ACTION.try_lock().map_err(|_| busy())?;
    let bus = audio_bus().await?;
    let scan = async {
        let adapter = Proxy::new(&bus, BLUEZ, ADAPTER, "org.bluez.Adapter1").await?;
        adapter.set_property("Powered", true).await?;
        let filter = HashMap::from([("Transport", Value::from("bredr"))]);
        adapter
            .call::<_, _, ()>("SetDiscoveryFilter", &(filter,))
            .await?;
        adapter.call::<_, _, ()>("StartDiscovery", &()).await?;
        tokio::time::sleep(SCAN_DURATION).await;
        // Snapshot while BlueZ still owns the discovery session.
        let devices = snapshot(&bus).await;
        adapter.call::<_, _, ()>("StopDiscovery", &()).await?;
        Ok::<_, anyhow::Error>(devices)
    };
    // Dropping this request's bus connection also releases its discovery session.
    match timeout(ACTION_TIMEOUT, scan).await {
        Ok(Ok(devices)) => devices,
        Ok(Err(err)) => Err(error(
            StatusCode::BAD_GATEWAY,
            format!("Scan failed. {err}"),
        )),
        Err(_) => Err(error(
            StatusCode::GATEWAY_TIMEOUT,
            "Bluetooth scan timed out.",
        )),
    }
}

fn busy() -> ApiError {
    error(
        StatusCode::CONFLICT,
        "Another Bluetooth action is in progress.",
    )
}

async fn selected_speaker(bus: &Connection, address: &str) -> Result<Speaker, ApiError> {
    snapshot(bus)
        .await?
        .0
        .into_iter()
        .find(|speaker| speaker.address.eq_ignore_ascii_case(address))
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Scan for this speaker first."))
}

// Register an agent only on the connection making Pair, as recommended by
// BlueZ. Headless Just Works speakers need no PIN or confirmation callback.
struct SpeakerAgent;

#[zbus::interface(name = "org.bluez.Agent1")]
impl SpeakerAgent {
    fn release(&self) {}
    fn cancel(&self) {}
}

async fn pair(Json(request): Json<SpeakerRequest>) -> ApiResult<Vec<Speaker>> {
    change_speaker(request, false).await
}

async fn connect(Json(request): Json<SpeakerRequest>) -> ApiResult<Vec<Speaker>> {
    change_speaker(request, true).await
}

async fn change_speaker(request: SpeakerRequest, connect: bool) -> ApiResult<Vec<Speaker>> {
    if !valid_address(&request.address) {
        return Err(error(StatusCode::BAD_REQUEST, "Invalid Bluetooth address."));
    }
    let _guard = RADIO_ACTION.try_lock().map_err(|_| busy())?;
    let bus = audio_bus().await?;
    let speaker = selected_speaker(&bus, &request.address).await?;
    if connect && !speaker.paired {
        return Err(error(StatusCode::CONFLICT, "Pair with this speaker first."));
    }
    if (connect && speaker.connected) || (!connect && speaker.paired) {
        return snapshot(&bus).await;
    }

    let action = async {
        let adapter = Proxy::new(&bus, BLUEZ, ADAPTER, "org.bluez.Adapter1").await?;
        if !adapter.get_property::<bool>("Powered").await? {
            adapter.set_property("Powered", true).await?;
        }
        let device = Proxy::new(&bus, BLUEZ, speaker.path.as_ref(), DEVICE).await?;
        if connect {
            device
                .call::<_, _, ()>("ConnectProfile", &(AUDIO_SINK,))
                .await?;
        } else {
            let agent = ObjectPath::try_from(AGENT)?;
            bus.object_server().at(agent.clone(), SpeakerAgent).await?;
            let manager = Proxy::new(&bus, BLUEZ, "/org/bluez", "org.bluez.AgentManager1").await?;
            manager
                .call::<_, _, ()>("RegisterAgent", &(agent, "NoInputNoOutput"))
                .await?;
            match timeout(
                ACTION_TIMEOUT - CALL_TIMEOUT,
                device.call::<_, _, ()>("Pair", &()),
            )
            .await
            {
                Ok(result) => result?,
                Err(_) => {
                    let _ =
                        timeout(CALL_TIMEOUT, device.call::<_, _, ()>("CancelPairing", &())).await;
                    anyhow::bail!(
                        "Pairing timed out. Put the speaker in pairing mode and try again."
                    );
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    match timeout(ACTION_TIMEOUT + CALL_TIMEOUT, action).await {
        Ok(Ok(())) => snapshot(&bus).await,
        Ok(Err(err)) => Err(error(StatusCode::BAD_GATEWAY, err.to_string())),
        Err(_) => Err(error(
            StatusCode::GATEWAY_TIMEOUT,
            "Bluetooth action timed out.",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::{names::OwnedInterfaceName, zvariant::OwnedValue};

    #[test]
    fn discovers_classic_speakers_and_reports_the_audio_transport() {
        let path = OwnedObjectPath::try_from("/org/bluez/hci0/dev_00_11_22_33_44_55").unwrap();
        let device = OwnedInterfaceName::try_from(DEVICE).unwrap();
        let mut properties = HashMap::from([
            (
                "Address".into(),
                OwnedValue::try_from(Value::from("00:11:22:33:44:55")).unwrap(),
            ),
            (
                "AddressType".into(),
                OwnedValue::try_from(Value::from("public")).unwrap(),
            ),
            ("Class".into(), OwnedValue::from(0x240414_u32)),
        ]);
        let snapshot = |properties| {
            HashMap::from([(path.clone(), HashMap::from([(device.clone(), properties)]))])
        };
        assert_eq!(speaker_list(&snapshot(properties.clone())).len(), 1);

        properties.insert("Paired".into(), OwnedValue::from(true));
        properties.insert("Connected".into(), OwnedValue::from(true));
        properties.insert(
            "UUIDs".into(),
            OwnedValue::try_from(Value::from(vec![AUDIO_SINK])).unwrap(),
        );
        let mut objects = snapshot(properties);
        assert!(!speaker_list(&objects)[0].connected);
        let endpoint =
            OwnedObjectPath::try_from("/org/bluez/hci0/dev_00_11_22_33_44_55/sep1/fd0").unwrap();
        let transport = OwnedInterfaceName::try_from("org.bluez.MediaTransport1").unwrap();
        let transport_properties = HashMap::from([(
            "Device".into(),
            OwnedValue::try_from(Value::from(path.as_ref().clone())).unwrap(),
        )]);
        objects.insert(endpoint, HashMap::from([(transport, transport_properties)]));
        assert!(speaker_list(&objects)[0].connected);
    }

    #[test]
    fn rejects_invalid_addresses_and_non_audio_classes() {
        assert!(valid_address("00:11:22:ab:44:55"));
        assert!(!valid_address("00:11:22:zz:44:55"));
        assert!(!valid_address("00/11:22:33:44:55"));
        assert!(!audio_class(0x0204));
        assert!(!audio_class(0x0430)); // video camera
    }
}
