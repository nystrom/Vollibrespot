use librespot::{
    connect::spirc::Spirc,
    core::{
        keymaster,
        session::Session,
        spotify_id::{SpotifyAudioType, SpotifyId},
    },
    metadata::{Album, Artist, Episode, Metadata, Show, Track},
    playback::player::{PlayerEvent, PlayerEventChannel},
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::net::UdpSocket;

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub enum PipeMsgs {
    Hello = 0x1,
    HeartBeat = 0x2,
    ReqToken = 0x3,
    Pause = 0x4,
    Play = 0x5,
    PlayPause = 0x6,
    Next = 0x7,
    Prev = 0x8,
    Volume = 0x9,
}

#[derive(Debug, Serialize)]
#[allow(non_camel_case_types)]
pub enum MetaMsgs<'a> {
    kSpPlaybackLoading,
    kSpPlaybackActive,
    kSpPlaybackInactive,
    kSpDeviceActive,
    kSpDeviceInactive,
    kSpSinkActive,
    kSpSinkInactive,
    position_ms(u32),
    volume(f64),
    state { status: &'a str },
    pong(PipeMsgs),
}

impl<'a> std::fmt::Display for MetaMsgs<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Clone, Debug)]
pub struct MetaPipeConfig {
    pub port: u16,
    pub version: String,
}

pub struct MetaPipe {
    handle: tokio::task::JoinHandle<()>,
}

const SCOPES: &str = "streaming,user-read-playback-state,user-modify-playback-state,user-read-currently-playing,user-read-private,user-library-modify,user-top-read,user-read-recently-played,user-library-read,playlist-read-private,playlist-read-collaborative";
const CLIENT_ID: Option<&'static str> = option_env!("CLIENT_ID");

impl MetaPipe {
    pub fn new(
        config: MetaPipeConfig,
        session: Session,
        event_rx: PlayerEventChannel,
        spirc: Arc<Spirc>,
    ) -> MetaPipe {
        let handle = tokio::spawn(run_meta_pipe(config, session, event_rx, spirc));
        MetaPipe { handle }
    }
}

impl Drop for MetaPipe {
    fn drop(&mut self) {
        debug!("drop MetaPipe");
        self.handle.abort();
    }
}

async fn run_meta_pipe(
    config: MetaPipeConfig,
    session: Session,
    mut event_rx: PlayerEventChannel,
    spirc: Arc<Spirc>,
) {
    let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), config.port + 1);
    let socket = match UdpSocket::bind(bind_addr).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to bind metadata socket on {}: {}", bind_addr, e);
            return;
        }
    };
    let remote_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), config.port);

    info!("Metadata pipe established on port {}", config.port + 1);
    send_str(&socket, &config.version, remote_addr).await;

    let mut token_info: Option<(Instant, Duration)> = None;
    let mut device_active = false;
    let mut buf = [0u8; 2];

    loop {
        if session.is_invalid() {
            error!("Session no longer valid");
            break;
        }

        // Refresh access token if expired
        if let Some((started, duration)) = token_info {
            if started.elapsed() > duration {
                info!("API token expired, refreshing...");
                token_info = request_and_send_token(&session, &socket, remote_addr).await;
            }
        }

        tokio::select! {
            event = event_rx.recv() => {
                match event {
                    Some(e) => {
                        handle_event(
                            &session, &socket, &spirc, e, remote_addr,
                            &mut token_info, &mut device_active,
                        )
                        .await;
                    }
                    None => {
                        warn!("Player event channel closed");
                        break;
                    }
                }
            }
            result = socket.recv(&mut buf) => {
                if result.is_ok() {
                    handle_volumio_msg(&spirc, &socket, buf, remote_addr).await;
                }
            }
        }
    }

    send_str(&socket, &MetaMsgs::kSpPlaybackInactive.to_string(), remote_addr).await;
    if device_active {
        send_str(&socket, &MetaMsgs::kSpSinkInactive.to_string(), remote_addr).await;
        send_str(&socket, &MetaMsgs::kSpDeviceInactive.to_string(), remote_addr).await;
    }
}

async fn handle_event(
    session: &Session,
    socket: &UdpSocket,
    spirc: &Arc<Spirc>,
    event: PlayerEvent,
    remote_addr: SocketAddr,
    token_info: &mut Option<(Instant, Duration)>,
    device_active: &mut bool,
) {
    info!("PlayerEvent: {:?}", event);
    match event {
        PlayerEvent::Loading {
            track_id,
            position_ms,
            ..
        } => {
            send_str(socket, &MetaMsgs::kSpPlaybackLoading.to_string(), remote_addr).await;
            handle_track_id(session, socket, track_id, Some(position_ms), remote_addr).await;
        }
        PlayerEvent::Playing {
            track_id,
            position_ms,
            ..
        } => {
            if !*device_active {
                *device_active = true;
                send_str(socket, &MetaMsgs::kSpDeviceActive.to_string(), remote_addr).await;
                send_str(socket, &MetaMsgs::kSpSinkActive.to_string(), remote_addr).await;
            }
            send_str(
                socket,
                &serde_json::to_string(&MetaMsgs::state { status: "play" }).unwrap(),
                remote_addr,
            )
            .await;
            send_str(socket, &MetaMsgs::kSpPlaybackActive.to_string(), remote_addr).await;
            handle_track_id(session, socket, track_id, Some(position_ms), remote_addr).await;
        }
        PlayerEvent::Paused {
            track_id,
            position_ms,
            ..
        } => {
            send_str(
                socket,
                &serde_json::to_string(&MetaMsgs::state { status: "pause" }).unwrap(),
                remote_addr,
            )
            .await;
            handle_track_id(session, socket, track_id, Some(position_ms), remote_addr).await;
        }
        PlayerEvent::Stopped { .. } | PlayerEvent::EndOfTrack { .. } => {
            send_str(socket, &MetaMsgs::kSpPlaybackInactive.to_string(), remote_addr).await;
        }
        PlayerEvent::Changed { new_track_id, .. } => {
            handle_track_id(session, socket, new_track_id, None, remote_addr).await;
        }
        PlayerEvent::VolumeSet { volume } => {
            let pvol = f64::from(volume) / f64::from(u16::MAX) * 100.0;
            debug!("VolumeSet: {:.1}%", pvol);
            send_str(
                socket,
                &serde_json::to_string(&MetaMsgs::volume(pvol)).unwrap(),
                remote_addr,
            )
            .await;
        }
        _ => debug!("Unhandled PlayerEvent: {:?}", event),
    }

    let _ = (spirc, token_info); // suppress unused warnings
}

async fn handle_volumio_msg(
    spirc: &Arc<Spirc>,
    socket: &UdpSocket,
    buf: [u8; 2],
    remote_addr: SocketAddr,
) {
    use self::PipeMsgs::*;
    match buf[0] {
        0x1 => info!("{:?}", Hello),
        0x2 => info!("{:?}", HeartBeat),
        0x3 => {
            info!("{:?}", ReqToken);
            // Token will be refreshed on next loop iteration via token_info check.
            // For immediate response, caller should trigger via event channel.
        }
        0x4 => {
            info!("{:?}", Pause);
            spirc.pause();
            send_str(
                socket,
                &serde_json::to_string(&MetaMsgs::pong(Pause)).unwrap(),
                remote_addr,
            )
            .await;
        }
        0x5 => {
            info!("{:?}", Play);
            spirc.play();
        }
        0x6 => {
            info!("{:?}", PlayPause);
            spirc.play_pause();
        }
        0x7 => {
            info!("{:?}", Next);
            spirc.next();
        }
        0x8 => {
            info!("{:?}", Prev);
            spirc.prev();
        }
        0x9 => {
            let volume = buf[1];
            // librespot 0.4 Spirc no longer exposes set_volume(u16); use volume_up/down
            // as a temporary stub. A future version can route this through the mixer.
            debug!(
                "{:?}: {:?}[u8] — absolute volume set not yet supported",
                Volume, volume
            );
        }
        _ => debug!("Unknown PipeMsg: {:02x} {:02x}", buf[0], buf[1]),
    }
}

async fn handle_track_id(
    session: &Session,
    socket: &UdpSocket,
    track_id: SpotifyId,
    position_ms: Option<u32>,
    remote_addr: SocketAddr,
) {
    if let Some(json) = get_metadata(session, track_id, position_ms).await {
        send_str(socket, &json.to_string(), remote_addr).await;
        send_str(socket, "\r\n", remote_addr).await;
    }
}

async fn get_metadata(
    session: &Session,
    spotify_id: SpotifyId,
    position_ms: Option<u32>,
) -> Option<Value> {
    if spotify_id.audio_type == SpotifyAudioType::Track {
        let track = match Track::get(session, spotify_id).await {
            Ok(t) => t,
            Err(e) => {
                error!("Error fetching track: {:?}", e);
                return None;
            }
        };
        let album = match Album::get(session, track.album).await {
            Ok(a) => a,
            Err(e) => {
                error!("Error fetching album: {:?}", e);
                return None;
            }
        };
        let mut artists = Vec::new();
        for artist_id in &track.artists {
            match Artist::get(session, *artist_id).await {
                Ok(a) => artists.push(a),
                Err(e) => {
                    error!("Error fetching artist: {:?}", e);
                    return None;
                }
            }
        }
        let covers = album
            .covers
            .iter()
            .map(|cover| cover.to_base16().unwrap_or_default())
            .collect::<Vec<_>>();
        let artist_ids = artists
            .iter()
            .map(|artist| artist.id.to_base62().unwrap_or_default())
            .collect::<Vec<_>>();
        let artist_names = artists
            .iter()
            .map(|artist| artist.name.clone())
            .collect::<Vec<String>>();
        Some(json!({
            "metadata": {
                "track_id": spotify_id.to_base62().unwrap_or_default(),
                "track_name": track.name,
                "artist_id": artist_ids,
                "artist_name": artist_names,
                "album_id": album.id.to_base62().unwrap_or_default(),
                "album_name": album.name,
                "duration_ms": track.duration,
                "albumartId": covers,
                "position_ms": position_ms.unwrap_or(0),
            }
        }))
    } else {
        let episode = match Episode::get(session, spotify_id).await {
            Ok(e) => e,
            Err(e) => {
                error!("Error fetching episode: {:?}", e);
                return None;
            }
        };
        let show = match Show::get(session, episode.show).await {
            Ok(s) => s,
            Err(e) => {
                error!("Error fetching show: {:?}", e);
                return None;
            }
        };
        let covers = episode
            .covers
            .iter()
            .map(|cover| cover.to_base16().unwrap_or_default())
            .collect::<Vec<_>>();
        let json = json!({
            "metadata": {
                "track_id": spotify_id.to_base62().unwrap_or_default(),
                "track_name": episode.name,
                "artist_name": vec![show.publisher],
                "album_id": show.id.to_base62().unwrap_or_default(),
                "album_name": show.name,
                "duration_ms": episode.duration,
                "albumartId": covers,
                "position_ms": position_ms.unwrap_or(0),
            }
        });
        info!("Episode metadata: {:?}", json);
        Some(json)
    }
}

async fn request_and_send_token(
    session: &Session,
    socket: &UdpSocket,
    remote_addr: SocketAddr,
) -> Option<(Instant, Duration)> {
    match CLIENT_ID {
        Some(client_id) => match keymaster::get_token(session, client_id, SCOPES).await {
            Ok(token) => {
                debug!("Got API token, expires_in={}", token.expires_in);
                let expiry = Duration::from_secs(u64::from(token.expires_in).saturating_sub(120));
                let msg = json!({
                    "token": {
                        "access_token": token.access_token,
                        "expires_in": token.expires_in,
                        "token_type": token.token_type,
                        "scope": token.scope,
                    }
                });
                send_str(socket, &msg.to_string(), remote_addr).await;
                Some((Instant::now(), expiry))
            }
            Err(e) => {
                error!("Failed to request access token: {:?}", e);
                None
            }
        },
        None => {
            warn!("No CLIENT_ID compiled in — cannot fetch Spotify API token");
            None
        }
    }
}

async fn send_str(socket: &UdpSocket, msg: &str, remote_addr: SocketAddr) {
    if let Err(e) = socket.send_to(msg.as_bytes(), remote_addr).await {
        error!("Failed to send metadata: {}", e);
    }
}
