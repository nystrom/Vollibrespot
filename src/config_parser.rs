use crate::{meta_pipe::MetaPipeConfig, version};
use hex;
use librespot::{
    core::{
        self,
        authentication::Credentials,
        cache::Cache,
        config::{ConnectConfig, DeviceType, SessionConfig},
    },
    playback::{
        audio_backend::{self, SinkBuilder, BACKENDS},
        config::{Bitrate, PlayerConfig, VolumeCtrl},
        mixer::{self, MixerConfig, MixerFn},
    },
};
use serde::Deserialize;
use sha1::{Digest, Sha1};
use std::{
    fs::File,
    io::{prelude::*, ErrorKind},
    path::PathBuf,
    process::exit,
    str::FromStr,
};
use toml;
use url::Url;

#[derive(Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
struct Authentication {
    shared: Option<bool>,
    username: Option<String>,
    password: Option<String>,
    device_name: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
struct Playback {
    bitrate: Option<i16>,
    enable_volume_normalisation: Option<bool>,
    normalisation_pregain: Option<f32>,
    volume_ctrl: Option<String>,
    autoplay: Option<bool>,
    gapless: Option<bool>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
struct Output {
    device: Option<String>,
    initial_volume: Option<u16>,
    mixer: Option<String>,
    mixer_name: Option<String>,
    mixer_card: Option<String>,
    mixer_index: Option<u32>,
    mixer_linear_volume: Option<bool>,
    backend: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
struct Misc {
    disable_audio_cache: Option<bool>,
    cache_location: Option<String>,
    metadata_port: Option<u16>,
    ap_port: Option<u16>,
    zeroconf_port: Option<u16>,
    proxy: Option<String>,
    device_type: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct Config {
    authentication: Authentication,
    playback: Playback,
    output: Output,
    misc: Misc,
}

impl Config {
    pub fn new(path: &str) -> Config {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(e) => match e.kind() {
                ErrorKind::NotFound => {
                    println!("Unable to read config from {:?}, Using default config", path);
                    return Config::default();
                }
                _ => {
                    println!("There was a problem opening the file: {:#?}", e);
                    exit(1)
                }
            },
        };
        println!("Reading Config from {:?}", path);
        let mut f_str = String::new();
        file.read_to_string(&mut f_str).unwrap();
        drop(file);

        let config: Config = match toml::from_str(&f_str) {
            Ok(config) => config,
            Err(e) => {
                println!("Malformed config key: {}", e.to_string());
                exit(1)
            }
        };
        config
    }
}

impl Default for Authentication {
    fn default() -> Authentication {
        Authentication {
            shared: Some(true),
            username: None,
            password: None,
            device_name: Some(String::from("Vollibrespot")),
        }
    }
}

impl Default for Playback {
    fn default() -> Playback {
        Playback {
            bitrate: Some(320),
            enable_volume_normalisation: Some(true),
            normalisation_pregain: None,
            volume_ctrl: Some(String::from("linear")),
            autoplay: Some(false),
            gapless: Some(true),
        }
    }
}

impl Default for Output {
    fn default() -> Output {
        Output {
            device: Some(String::from("default")),
            initial_volume: Some(50),
            mixer: Some(String::from("softvol")),
            mixer_name: None,
            mixer_card: None,
            mixer_index: None,
            mixer_linear_volume: Some(true),
            backend: Some(String::from("alsa")),
        }
    }
}

impl Default for Misc {
    fn default() -> Misc {
        Misc {
            disable_audio_cache: Some(false),
            cache_location: Some(String::from("/tmp")),
            metadata_port: Some(5030),
            ap_port: None,
            zeroconf_port: Some(0),
            proxy: None,
            device_type: Some(String::from("Speaker")),
        }
    }
}

impl Default for Config {
    fn default() -> Config {
        Config {
            authentication: Authentication::default(),
            playback: Playback::default(),
            output: Output::default(),
            misc: Misc::default(),
        }
    }
}

fn device_id(name: &str) -> String {
    hex::encode(Sha1::digest(name.as_bytes()))
}

#[derive(Clone)]
pub struct Setup {
    pub credentials: Option<Credentials>,
    pub session_config: SessionConfig,
    pub connect_config: ConnectConfig,
    pub backend: SinkBuilder,
    pub device: Option<String>,
    pub mixer: MixerFn,
    pub cache: Option<Cache>,
    pub player_config: PlayerConfig,
    pub mixer_config: MixerConfig,
    pub meta_config: MetaPipeConfig,
    pub enable_discovery: bool,
    pub zeroconf_port: u16,
}

impl Setup {
    pub fn from_config(config: Config) -> Setup {
        // Cache
        let use_audio_cache = !config.misc.disable_audio_cache.unwrap_or(true);
        let cache = config.misc.cache_location.as_ref().and_then(|loc| {
            let path = PathBuf::from(loc);
            let audio: Option<PathBuf> = if use_audio_cache { Some(path.clone()) } else { None };
            match Cache::new(Some(path.clone()), Some(path.clone()), audio, None) {
                Ok(c) => Some(c),
                Err(e) => {
                    error!("Failed to open cache: {}", e);
                    None
                }
            }
        });

        let device_name = config
            .authentication
            .device_name
            .unwrap_or_else(|| String::from("Vollibrespot"));

        let credentials = {
            let username = config.authentication.username;
            let password = config.authentication.password;
            let cached_credentials = cache.as_ref().and_then(Cache::credentials);

            match (username, password, cached_credentials) {
                (Some(username), Some(password), _) if !username.is_empty() && !password.is_empty() => {
                    Some(Credentials::with_password(username, password))
                }
                (Some(ref username), _, Some(ref creds)) if username == &creds.username => {
                    Some(creds.clone())
                }
                _ => None,
            }
        };

        let device = config.output.device.and_then(|d| {
            if d.is_empty() {
                error!("Invalid output device!");
                exit(1);
            } else {
                Some(d)
            }
        });

        match config.output.backend.as_ref().map(AsRef::as_ref) {
            Some("pipe") => {
                warn!("Using Pipe backend with device: {}", device.as_ref().unwrap());
            }
            Some("alsa") => {
                warn!("Using Alsa backend with device: {}", device.as_ref().unwrap());
            }
            _ => {
                error!("Unsupported backend");
                exit(1)
            }
        }
        let backend = audio_backend::find(config.output.backend).unwrap();
        let mixer = mixer::find(config.output.mixer.as_deref()).expect("Invalid mixer");

        let volume_ctrl = config
            .playback
            .volume_ctrl
            .as_deref()
            .and_then(|s| VolumeCtrl::from_str(s).ok())
            .unwrap_or_default();

        let mixer_config = MixerConfig {
            device: config
                .output
                .mixer_card
                .unwrap_or_else(|| String::from("default")),
            control: config.output.mixer_name.unwrap_or_else(|| String::from("PCM")),
            index: config.output.mixer_index.unwrap_or(0),
            volume_ctrl,
        };

        let initial_volume = config
            .output
            .initial_volume
            .map(|volume| {
                if volume > 100 {
                    error!("Initial volume must be in the range 0-100");
                }
                (i32::from(volume) * 0xFFFF / 100) as u16
            })
            .or_else(|| cache.as_ref().and_then(Cache::volume))
            .unwrap_or(0x8000);

        let zeroconf_port = config.misc.zeroconf_port.unwrap_or(0);

        let session_config = SessionConfig {
            user_agent: core::version::VERSION_STRING.to_string(),
            device_id: device_id(&device_name),
            proxy: config
                .misc
                .proxy
                .or_else(|| std::env::var("http_proxy").ok())
                .map(|s| match Url::parse(&s) {
                    Ok(url) => {
                        if url.host().is_none() || url.port().is_none() {
                            panic!("Invalid proxy url, only urls in the format \"http://host:port\" are allowed");
                        }
                        if url.scheme() != "http" {
                            panic!("Only unsecure http:// proxies are supported");
                        }
                        url
                    }
                    Err(err) => panic!(
                        "Invalid proxy url: {}, only urls in the format \"http://host:port\" are allowed",
                        err
                    ),
                }),
            ap_port: config.misc.ap_port.or(Some(443)),
        };

        let player_config = {
            let bitrate = config
                .playback
                .bitrate
                .and_then(|b| Bitrate::from_str(&b.to_string()).ok())
                .unwrap_or_default();

            PlayerConfig {
                bitrate,
                normalisation: config.playback.enable_volume_normalisation.unwrap_or(false),
                normalisation_pregain_db: f64::from(
                    config.playback.normalisation_pregain.unwrap_or(0.0),
                ),
                gapless: config.playback.gapless.unwrap_or(true),
                ..PlayerConfig::default()
            }
        };

        let connect_config = ConnectConfig {
            name: device_name,
            device_type: config
                .misc
                .device_type
                .map(|dt| DeviceType::from_str(&dt).expect("Invalid device type"))
                .unwrap_or_default(),
            initial_volume: Some(initial_volume),
            has_volume_ctrl: true,
            autoplay: config.playback.autoplay.unwrap_or(false),
        };

        let meta_config = MetaPipeConfig {
            port: config.misc.metadata_port.unwrap_or(5030),
            version: format!("vollibrespot v{}", version::semver()),
        };

        let enable_discovery = config.authentication.shared.unwrap_or(true);

        Setup {
            cache,
            credentials,
            backend,
            device,
            mixer,
            mixer_config,
            session_config,
            player_config,
            connect_config,
            meta_config,
            enable_discovery,
            zeroconf_port,
        }
    }
}
