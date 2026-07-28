use std::{
    env::{self, current_exe},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, Command, Stdio, exit},
    sync::OnceLock,
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result};
use clap::Parser;
use gettextrs::{bind_textdomain_codeset, bindtextdomain, textdomain};
use oma_pm::{apt::OmaApt, matches::PackagesMatcher, pkginfo::OmaPackage};
use tracing::{error, level_filters::LevelFilter};
use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QQuickStyle, QString, QUrl};

use crate::backend::run_backend;

mod backend;
mod cxx_qt_bridge;

static PKG_PATH: OnceLock<PathBuf> = OnceLock::new();

#[derive(Parser, Debug)]
#[clap(about, version, author)]
struct Args {
    package: Option<PathBuf>,
    #[clap(long, hide = true)]
    backend: bool,
    #[clap(long, short)]
    debug: bool,
}

#[derive(Debug)]
pub enum ProgressEvent {
    Percent(u32),
    Message(String),
    Done,
    Err(String),
}

fn main() {
    let Args {
        package,
        backend,
        debug,
    } = Args::parse();

    if !debug {
        let no_i18n_embd_info: EnvFilter = "i18n_embed=off,info".parse().unwrap();
        tracing_subscriber::registry()
            .with(
                fmt::layer()
                    .with_filter(no_i18n_embd_info)
                    .and_then(LevelFilter::INFO),
            )
            .init();
    } else {
        let env_log = EnvFilter::try_from_default_env();
        if let Ok(filter) = env_log {
            tracing_subscriber::registry()
                .with(
                    fmt::layer()
                        .event_format(
                            tracing_subscriber::fmt::format()
                                .with_file(true)
                                .with_line_number(true),
                        )
                        .with_filter(filter),
                )
                .init();
        } else {
            let debug_filter: EnvFilter = "hyper=off,rustls=off,debug".parse().unwrap();
            tracing_subscriber::registry()
                .with(
                    fmt::layer()
                        .event_format(
                            tracing_subscriber::fmt::format()
                                .with_file(true)
                                .with_line_number(true),
                        )
                        .with_filter(debug_filter),
                )
                .init();
        }
    }

    if let Some(package) = package {
        if !package.exists() {
            eprintln!("Package does not exist at the specified path.");
            exit(1);
        }

        if package.extension().map(|x| x.to_string_lossy()) != Some("deb".into()) {
            eprintln!(
                "Usage: {} /path/to/package.deb",
                current_exe().unwrap().display()
            );
            exit(1);
        }

        PKG_PATH.set(package).unwrap();

        ui();
    } else if backend {
        if let Err(e) = run_backend() {
            error!("{e}");
            exit(1);
        }
    } else {
        eprintln!(
            "Usage: {} /path/to/package.deb",
            current_exe().unwrap().display()
        );
        exit(1);
    }
}

fn ui() {
    #[cfg(debug_assertions)]
    bindtextdomain("deb-installer", "./mo").unwrap();

    #[cfg(not(debug_assertions))]
    bindtextdomain("deb-installer", "/usr/share/locale").unwrap();

    textdomain("deb-installer").unwrap();
    bind_textdomain_codeset("deb-installer", "UTF-8").unwrap();

    let debconf_helper = start_kde_debconf();
    let mut _debconf_child = None;
    match debconf_helper {
        Err(e) => error!("Failed to start debconf-kde-helper: {e}"),
        Ok(child) => _debconf_child = Some(child),
    }

    let style = env::var("QT_QUICK_CONTROLS_STYLE");
    if style.is_err() {
        QQuickStyle::set_style(&QString::from("org.kde.desktop"));
    }

    QGuiApplication::set_desktop_file_name(&QString::from("io.aosc.deb_installer"));

    let mut app = QGuiApplication::new();
    let mut engine = QQmlApplicationEngine::new();

    if let Some(engine) = engine.as_mut() {
        engine.load(&QUrl::from("qrc:/qt/qml/io/aosc/DebInstaller/src/main.qml"));
    }

    if let Some(app) = app.as_mut() {
        app.exec();
    }
}

pub fn get_package<'a>(apt: &'a mut OmaApt, arg: &'a str) -> Result<OmaPackage> {
    let matcher = PackagesMatcher::builder()
        .filter_candidate(true)
        .filter_downloadable_candidate(false)
        .select_dbg(false)
        .cache(&apt.cache)
        .build();

    let pkgs = matcher.match_local_glob(arg)?;
    apt.install(&pkgs, true)?;

    Ok(pkgs
        .first()
        .context("Failed to get package from path")?
        .try_clone()?)
}

pub fn on_install(argc: String, tx: flume::Sender<ProgressEvent>) -> JoinHandle<Result<()>> {
    thread::spawn(move || -> Result<()> {
        // Generate a cryptographically-secure auth token (32 bytes, hex-encoded)
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|e| anyhow::anyhow!("Failed to generate auth token: {e}"))?;
        let token: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();

        let (out_recv, out_send) = std::io::pipe()?;

        let mut child = Command::new("pkexec")
            .arg("--keep-cwd")
            .arg(std::env::current_exe()?)
            .arg("--backend")
            .stdin(Stdio::piped())
            .stdout(out_send)
            .stderr(Stdio::inherit())
            .spawn()?;

        // Send init command with auth token + our PID
        let mut child_stdin = child.stdin.take().context("Failed to get child stdin")?;
        writeln!(child_stdin, "{token} init {}", std::process::id())?;
        // Send install command
        writeln!(child_stdin, "{token} install {argc}")?;

        // Read progress and result from child's stdout
        let mut lines = BufReader::new(out_recv).lines();
        let crash_msg: Option<String> = loop {
            let line = match lines.next() {
                Some(Ok(l)) => l,
                Some(Err(e)) => break Some(format!("Lost connection to backend: {e}")),
                None => break Some("Backend exited unexpectedly".to_string()),
            };
            let stripped = console::strip_ansi_codes(&line).to_string();

            if let Some(rest) = stripped.strip_prefix("progress ")
                && let Ok(pct) = rest.trim().parse::<u32>()
            {
                let _ = tx.send(ProgressEvent::Percent(pct));
            } else if stripped == "result ok" {
                let _ = tx.send(ProgressEvent::Done);
                break None;
            } else if let Some(rest) = stripped.strip_prefix("result ") {
                let _ = tx.send(ProgressEvent::Message(rest.to_string()));
                let _ = tx.send(ProgressEvent::Err(rest.to_string()));
                break None;
            } else {
                let _ = tx.send(ProgressEvent::Message(stripped));
            }
        };

        if let Some(msg) = crash_msg {
            let _ = tx.send(ProgressEvent::Message(msg.clone()));
            let _ = tx.send(ProgressEvent::Err(msg));
        }

        // Signal exit, close stdin, then wait for the backend
        let _ = writeln!(child_stdin, "{token} exit");
        drop(child_stdin);
        let _ = child.wait();

        Ok(())
    })
}

fn start_kde_debconf() -> Result<Child> {
    Ok(Command::new("debconf-kde-helper").spawn()?)
}
