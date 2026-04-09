#[macro_use]
extern crate log;

use env_logger::{fmt, Builder};
use futures::StreamExt;
use librespot::{
    connect::spirc::Spirc,
    core::{
        authentication::Credentials,
        cache::Cache,
        config::{ConnectConfig, SessionConfig},
        session::Session,
    },
    discovery::Discovery,
    playback::{audio_backend::BACKENDS, config::AudioFormat, mixer::MixerConfig, player::Player},
};
use std::{env, io::Write, sync::Arc};

mod config_parser;
mod meta_pipe;
mod reconnect;
mod version;
use crate::{
    config_parser::{Config, Setup},
    meta_pipe::MetaPipe,
    reconnect::ReconnectPolicy,
};

fn usage(program: &str, opts: &getopts::Options) -> String {
    let brief = format!("Usage: {} [options]", program);
    opts.usage(&brief)
}

fn setup_logging(verbose: bool) {
    let mut builder = Builder::new();
    builder.format(|buf, record| {
        let mut base_style = buf.style();
        let mut module_style = buf.style();
        let mut level_style = buf.style();
        let mut module_path = "";
        let level = record.level();

        match level {
            log::Level::Trace | log::Level::Debug => {
                module_path = record.module_path().unwrap_or("vollibrespot");
                module_style.set_color(fmt::Color::Yellow).set_bold(true);
                level_style.set_color(fmt::Color::Green)
            }
            log::Level::Info => level_style.set_color(fmt::Color::White),
            log::Level::Warn => level_style.set_color(fmt::Color::Yellow),
            log::Level::Error => level_style.set_color(fmt::Color::Red),
        };
        level_style.set_bold(true);
        base_style.set_color(fmt::Color::Cyan).set_bold(true);
        writeln!(
            buf,
            "{} {}: {}",
            base_style.value("[Vollibrespot]"),
            module_style.value(module_path),
            level_style.value(record.args())
        )
    });
    match env::var("RUST_LOG") {
        Ok(config) => {
            builder.parse_filters(&config);
            if verbose {
                warn!("`--verbose` flag overridden by `RUST_LOG` environment variable");
            }
            builder.init();
        }
        Err(_) => {
            if verbose {
                builder.parse_filters("libmdns=info,librespot=debug,vollibrespot=trace");
            } else {
                builder.parse_filters("libmdns=info,librespot=info,vollibrespot=info");
            }
            builder.init();
        }
    }
}

fn list_backends() {
    println!("Available Backends : ");
    for (&(name, _), idx) in BACKENDS.iter().zip(0..) {
        if idx == 0 {
            println!("- {} (default)", name);
        } else {
            println!("- {}", name);
        }
    }
}

fn parse_args(args: &[String]) -> Setup {
    let mut opts = getopts::Options::new();
    opts.optopt(
        "c",
        "config",
        "Path to config file to read. Defaults to 'config.toml'",
        "CONFIG",
    )
    .optopt(
        "",
        "backend",
        "Audio backend to use. Use '?' to list options",
        "BACKEND",
    )
    .optflag("", "verbose", "Enable verbose output");

    let matches = match opts.parse(&args[1..]) {
        Ok(m) => m,
        Err(f) => {
            match args.last().unwrap().as_str() {
                "-v" | "--version" => {
                    println!("{}", version::version());
                    std::process::exit(0)
                }
                _ => eprintln!("error: {:?}\n{}", f, usage(&args[0], &opts)),
            }
            std::process::exit(1);
        }
    };

    let verbose = matches.opt_present("verbose");
    setup_logging(verbose);

    let backend_name = matches.opt_str("backend");
    if backend_name == Some("?".into()) {
        list_backends();
        std::process::exit(0);
    }

    println!("{}", version::version());

    let config_file = matches
        .opt_str("config")
        .unwrap_or_else(|| String::from("config.toml"));
    Setup::from_config(Config::new(&config_file))
}

async fn optional_discovery_next(discovery: &mut Option<Discovery>) -> Option<Credentials> {
    match discovery {
        Some(ref mut d) => d.next().await,
        None => std::future::pending().await,
    }
}

async fn run(setup: Setup) {
    let mut discovery: Option<Discovery> = if setup.enable_discovery {
        Some(
            Discovery::builder(setup.session_config.device_id.clone())
                .name(setup.connect_config.name.clone())
                .device_type(setup.connect_config.device_type)
                .port(setup.zeroconf_port)
                .launch()
                .expect("Failed to start Spotify discovery"),
        )
    } else {
        None
    };

    let mut last_credentials = setup.credentials.clone();
    let mut reconnect_policy = ReconnectPolicy::default();

    'main: loop {
        // Get or wait for credentials
        let credentials = if let Some(creds) = last_credentials.clone() {
            creds
        } else {
            warn!("No credentials — waiting for Spotify app connection...");
            match optional_discovery_next(&mut discovery).await {
                Some(creds) => creds,
                None => {
                    error!("Discovery stream ended without delivering credentials");
                    break 'main;
                }
            }
        };

        // Back-off delay on reconnect
        if let Some(reconnect_wait) = reconnect_policy.reconnect_wait() {
            warn!("Reconnecting in {:?}", reconnect_wait);
            tokio::time::sleep(reconnect_wait).await;
        }

        // Connect to Spotify
        let (session, _stored_credentials) = match Session::connect(
            setup.session_config.clone(),
            credentials.clone(),
            setup.cache.clone(),
            true, // store credentials in cache
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                error!("Failed to connect to Spotify: {}", e);
                reconnect_policy.note_connect_error();
                continue;
            }
        };
        last_credentials = Some(credentials);
        reconnect_policy.note_connected();

        // Setup mixer
        let mixer_config = setup.mixer_config.clone();
        let mixer = (setup.mixer)(mixer_config);
        let volume_getter = mixer.get_soft_volume();

        // Setup player
        let backend = setup.backend;
        let device = setup.device.clone();
        let (player, player_event_channel) = Player::new(
            setup.player_config.clone(),
            session.clone(),
            volume_getter,
            move || (backend)(device, AudioFormat::default()),
        );

        // Setup spirc (Spotify Remote Playback Control)
        let (spirc, spirc_task) =
            Spirc::new(setup.connect_config.clone(), session.clone(), player, mixer);
        let spirc = Arc::new(spirc);

        // MetaPipe: sends metadata to Volumio over UDP
        let _meta_pipe = MetaPipe::new(
            setup.meta_config.clone(),
            session.clone(),
            player_event_channel,
            spirc.clone(),
        );

        // Run until shutdown, spirc ends, or new credentials arrive
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl-C received — shutting down");
                spirc.shutdown();
                break 'main;
            }
            _ = spirc_task => {
                let uptime = reconnect_policy.note_spirc_disconnect();
                if uptime.is_zero() {
                    warn!("Spirc shut down unexpectedly — will reconnect");
                } else {
                    warn!("Spirc shut down unexpectedly after {:?} — will reconnect", uptime);
                }
            }
            Some(new_creds) = optional_discovery_next(&mut discovery) => {
                warn!("New credentials from discovery — reconnecting");
                spirc.shutdown();
                last_credentials = Some(new_creds);
                reconnect_policy.reset();
            }
        }
        // _meta_pipe dropped here; its tokio task is aborted
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if env::var("RUST_BACKTRACE").is_err() {
        env::set_var("RUST_BACKTRACE", "full");
    }
    let args: Vec<String> = std::env::args().collect();
    run(parse_args(&args)).await;
}
