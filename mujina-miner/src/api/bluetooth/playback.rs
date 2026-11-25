//! One uploaded MP3 and one playback session in the Nano3S audio runtime.

use std::{io, path::Path, process::Stdio, time::Duration};

use axum::{
    Json, Router,
    body::Body,
    extract::Query,
    http::{HeaderMap, StatusCode, header},
    routing,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    io::AsyncWriteExt,
    process::{Child, Command},
    sync::Mutex,
    time::timeout,
};

use super::{
    ApiError, ApiResult, SpeakerRequest, audio_bus, error, selected_speaker, valid_address,
};
use crate::api::server::SharedState;

const ROOT: &str = "/tmp/mujina-bluetooth/root";
const TRACK: &str = "/tmp/mujina-track.mp3";
const UPLOAD: &str = "/tmp/mujina-track.part";
const ALSA_CONFIG: &str = "/tmp/mujina-playback.conf";
const PCM_RATE: &str = "48000";
// Leave room in the board's tmpfs for the old track and a replacement.
const MAX_UPLOAD: usize = 12 * 1024 * 1024;
const PLAYER_FAILED: &str = "Playback ended unexpectedly. Check the speaker connection.";

static PLAYER: Mutex<Player> = Mutex::const_new(Player {
    name: None,
    playback: None,
    playback_error: None,
});

struct Playback {
    decoder: Child,
    output: Child,
}

struct Player {
    name: Option<String>,
    playback: Option<Playback>,
    playback_error: Option<&'static str>,
}

#[derive(Serialize)]
struct TrackStatus {
    name: Option<String>,
    playing: bool,
    error: Option<&'static str>,
}

#[derive(Deserialize)]
struct TrackUpload {
    name: String,
}

#[derive(Deserialize)]
struct StopRequest {}

pub(super) fn routes() -> Router<SharedState> {
    Router::new()
        .route("/bluetooth/track", routing::get(track_status).put(upload))
        .route("/bluetooth/play", routing::post(play))
        .route("/bluetooth/stop", routing::post(stop))
}

impl Player {
    fn snapshot(&mut self) -> TrackStatus {
        if let Some(playback) = self.playback.as_mut() {
            let decoder = playback.decoder.try_wait();
            let output = playback.output.try_wait();
            let failed = [&decoder, &output].into_iter().any(|result| match result {
                Ok(Some(code)) => !code.success(),
                Err(_) => true,
                Ok(None) => false,
            });
            if failed || matches!(output, Ok(Some(_))) {
                self.playback = None;
                self.playback_error = failed.then_some(PLAYER_FAILED);
            }
        }
        TrackStatus {
            name: self.name.clone(),
            playing: self.playback.is_some(),
            error: self.playback_error,
        }
    }
}

// Clean up interrupted uploads before allowing the next upload.
struct PartialTrack;

impl Drop for PartialTrack {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(host_path(UPLOAD));
    }
}

fn bundled_audio() -> bool {
    std::env::var("MUJINA_BUNDLED_BLUETOOTH").as_deref() == Ok("1")
}

fn runtime_path(path: &str) -> String {
    if bundled_audio() {
        path.to_owned()
    } else {
        path.replacen("/tmp/", "/run/mujina-mp3/", 1)
    }
}

fn host_path(path: &str) -> String {
    if bundled_audio() {
        format!("{ROOT}{path}")
    } else {
        runtime_path(path)
    }
}

fn audio_command(program: &str, bundled_program: &str) -> Command {
    let mut cmd = if bundled_audio() {
        let mut chroot = Command::new("/usr/sbin/chroot");
        chroot.args([ROOT, bundled_program]).env(
            "DBUS_SYSTEM_BUS_ADDRESS",
            "unix:path=/run/dbus/system_bus_socket",
        );
        chroot
    } else {
        Command::new(program)
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    cmd
}

fn player_busy() -> ApiError {
    error(StatusCode::CONFLICT, "Another MP3 action is in progress.")
}

fn upload_too_large() -> ApiError {
    error(StatusCode::PAYLOAD_TOO_LARGE, "Choose an MP3 up to 12 MiB.")
}

fn io_error(err: io::Error) -> ApiError {
    let status = if err.raw_os_error() == Some(libc::ENOSPC) {
        StatusCode::INSUFFICIENT_STORAGE
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    error(status, format!("MP3 storage error. {err}"))
}

fn valid_mp3_name(name: &str) -> bool {
    name.len() <= 200
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
        && Path::new(name)
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("mp3"))
}

async fn track_status() -> ApiResult<TrackStatus> {
    Ok(Json(
        PLAYER.try_lock().map_err(|_| player_busy())?.snapshot(),
    ))
}

async fn upload(
    Query(track): Query<TrackUpload>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<TrackStatus> {
    if !valid_mp3_name(&track.name)
        || headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            != Some("audio/mpeg")
    {
        return Err(error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Choose an MP3 file.",
        ));
    }
    if headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > MAX_UPLOAD as u64)
    {
        return Err(upload_too_large());
    }
    if (bundled_audio() && !Path::new(ROOT).join("usr/bin/mpg123.bin").is_file())
        || (!bundled_audio() && !Path::new("/run/mujina-mp3").is_dir())
    {
        return Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The Bluetooth audio runtime is still starting.",
        ));
    }
    let mut player = PLAYER.try_lock().map_err(|_| player_busy())?;
    if player.snapshot().playing {
        return Err(error(
            StatusCode::CONFLICT,
            "Stop playback before uploading another MP3.",
        ));
    }
    let _partial = PartialTrack;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(host_path(UPLOAD))
        .await
        .map_err(io_error)?;
    let mut stream = body.into_data_stream();
    let mut bytes = 0;
    let copy = async {
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|_| error(StatusCode::BAD_REQUEST, "The upload was interrupted."))?;
            if chunk.len() > MAX_UPLOAD - bytes {
                return Err(upload_too_large());
            }
            bytes += chunk.len();
            file.write_all(&chunk).await.map_err(io_error)?;
        }
        file.flush().await.map_err(io_error)
    };
    timeout(Duration::from_secs(90), copy)
        .await
        .map_err(|_| error(StatusCode::REQUEST_TIMEOUT, "The upload timed out."))??;
    drop(file);
    let mut decoder = audio_command("mpg123", "/usr/bin/mpg123.bin");
    decoder
        .args(["-q", "-n", "8", "-s"])
        .arg(runtime_path(UPLOAD))
        .stdout(Stdio::piped());
    // mpg123 can exit successfully when it finds no audio frames.
    if !matches!(timeout(Duration::from_secs(5), decoder.output()).await,
        Ok(Ok(probe)) if probe.status.success() && !probe.stdout.is_empty())
    {
        return Err(error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "The MP3 could not be decoded.",
        ));
    }
    fs::rename(host_path(UPLOAD), host_path(TRACK))
        .await
        .map_err(io_error)?;
    player.name = Some(track.name);
    player.playback_error = None;
    Ok(Json(player.snapshot()))
}

async fn play(Json(request): Json<SpeakerRequest>) -> ApiResult<TrackStatus> {
    if !valid_address(&request.address) {
        return Err(error(StatusCode::BAD_REQUEST, "Invalid Bluetooth address."));
    }
    let bus = audio_bus().await?;
    if !selected_speaker(&bus, &request.address).await?.connected {
        return Err(error(
            StatusCode::CONFLICT,
            "Connect the speaker before playing.",
        ));
    }
    let mut player = PLAYER.try_lock().map_err(|_| player_busy())?;
    if player.snapshot().playing {
        return Err(error(
            StatusCode::CONFLICT,
            "Stop the current track before starting playback.",
        ));
    }
    if player.name.is_none() {
        return Err(error(StatusCode::CONFLICT, "Upload an MP3 first."));
    }
    player.playback_error = None;
    player.playback = Some(start_playback(&request.address).await.map_err(|err| {
        error(
            StatusCode::BAD_GATEWAY,
            format!("MP3 playback could not start. {err}"),
        )
    })?);
    Ok(Json(player.snapshot()))
}

async fn start_playback(address: &str) -> io::Result<Playback> {
    let config = format!(
        "</usr/share/alsa/alsa.conf>\n\
         pcm.mujina_speaker {{\n\
             type plug\n\
             slave.pcm {{\n\
                 type bluealsa\n\
                 device \"{address}\"\n\
                 profile \"a2dp\"\n\
             }}\n\
         }}\n",
    );
    fs::write(host_path(ALSA_CONFIG), config).await?;
    // Play raw PCM so ALSA can stream to EOF without a seekable WAV header.
    let mut decoder = audio_command("mpg123", "/usr/bin/mpg123.bin")
        .args(["-q", "-r", PCM_RATE, "--stereo", "--encoding", "s16", "-s"])
        .arg(runtime_path(TRACK))
        .stdout(Stdio::piped())
        .spawn()?;
    let pipe: Stdio = decoder
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("No audio pipe"))?
        .try_into()?;
    let output = audio_command("aplay", "/usr/bin/aplay")
        .env("ALSA_CONFIG_PATH", runtime_path(ALSA_CONFIG))
        .args([
            "-q",
            "-D",
            "mujina_speaker",
            "-t",
            "raw",
            "-f",
            "S16_LE",
            "-r",
            PCM_RATE,
            "-c",
            "2",
            "-",
        ])
        .stdin(pipe)
        .spawn()?;
    Ok(Playback { decoder, output })
}

async fn stop(Json(_): Json<StopRequest>) -> ApiResult<TrackStatus> {
    let mut player = PLAYER.try_lock().map_err(|_| player_busy())?;
    if let Some(mut playback) = player.playback.take() {
        let _ = tokio::join!(playback.decoder.kill(), playback.output.kill());
    }
    player.playback_error = None;
    Ok(Json(player.snapshot()))
}
