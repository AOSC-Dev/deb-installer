use std::{
    env::{self, current_exe},
    io::{BufRead, BufReader, PipeReader},
    path::{Path, PathBuf},
    process::{Child, Command, exit},
    sync::OnceLock,
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result};
use backend::Backend;
use clap::Parser;
use futures::StreamExt;
use futures::pin_mut;
use gettextrs::{bind_textdomain_codeset, bindtextdomain, textdomain};
use oma_pm::{apt::OmaApt, matches::PackagesMatcher, pkginfo::OmaPackage};
use tokio::fs::create_dir_all;
use tracing::{debug, error, level_filters::LevelFilter};
use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use zbus::{Connection, connection, fdo, proxy};

use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QQuickStyle, QString, QUrl};

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

#[proxy(
    interface = "io.aosc.DebInstaller1",
    default_service = "io.aosc.DebInstaller",
    default_path = "/io/aosc/DebInstaller"
)]
trait OmaClient {
    async fn install(&self, path: String) -> zbus::Result<bool>;
    async fn exit(&self) -> zbus::Result<bool>;

    #[zbus(signal)]
    async fn progress(&self, percent: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn finished(&self, result: String) -> zbus::Result<()>;
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
        let txc = tx.clone();
        let txc2 = tx.clone();

        let (mut backend_child, reader) = start_backend()?;
        let reader = BufReader::new(reader);

        thread::spawn(move || {
            reader.lines().for_each(|line| match line {
                Ok(line) => {
                    let _ = txc.send(ProgressEvent::Message(
                        console::strip_ansi_codes(&line).to_string(),
                    ));
                }
                Err(e) => error!("{e}"),
            });
        });

        thread::spawn(move || {
            let wait = backend_child.wait();
            match wait {
                Ok(status) => {
                    if !status.success() {
                        let _ = txc2.send(ProgressEvent::Err(format!(
                            "Backend process exited with status: {status}"
                        )));
                    } else {
                        let _ = txc2.send(ProgressEvent::Done);
                    }
                }
                Err(e) => {
                    let _ = txc2.send(ProgressEvent::Err(format!(
                        "Failed to wait for backend: {e}"
                    )));
                }
            }
        });

        on_install_inner(argc, tx)
    })
}

#[tokio::main]
async fn on_install_inner(argc: String, tx: flume::Sender<ProgressEvent>) -> Result<()> {
    let conn = Connection::system().await?;
    let client = OmaClientProxy::new(&conn).await?;

    // Wait for the backend to start
    let dbus_proxy = fdo::DBusProxy::new(&conn).await?;
    if !dbus_proxy
        .name_has_owner(zbus::names::BusName::from_static_str(
            "io.aosc.DebInstaller",
        )?)
        .await?
    {
        let name_stream = dbus_proxy.receive_name_owner_changed().await?;
        pin_mut!(name_stream);

        while let Some(signal) = name_stream.next().await {
            let args = signal.args()?;
            if args.name() == "io.aosc.DebInstaller" && args.new_owner().as_ref().is_some() {
                break;
            }
        }
    }

    let progress_stream = client.receive_progress().await?;
    let finished_stream = client.receive_finished().await?;
    pin_mut!(progress_stream);
    pin_mut!(finished_stream);

    let _ = client.install(argc).await?;

    loop {
        tokio::select! {
            Some(signal) = progress_stream.next() => {
                let args = signal.args()?;
                let _ = tx.send_async(ProgressEvent::Percent(args.percent)).await;
            }
            Some(signal) = finished_stream.next() => {
                let args = signal.args()?;
                let _ = tx.send_async(ProgressEvent::Message(args.result.clone())).await;
                if args.result == "ok" {
                    let _ = tx.send_async(ProgressEvent::Done).await;
                } else {
                    let _ = tx.send_async(ProgressEvent::Err(args.result)).await;
                }
                let _ = client.exit().await;
                return Ok(());
            }
        }
    }
}

fn start_backend() -> Result<(Child, PipeReader)> {
    let (recv, send) = std::io::pipe()?;
    let child = Command::new("pkexec")
        .arg("--keep-cwd")
        .arg(std::env::current_exe()?)
        .arg("--backend")
        .stdout(send.try_clone()?)
        .stderr(send)
        .spawn()?;
    Ok((child, recv))
}

fn start_kde_debconf() -> Result<Child> {
    Ok(Command::new("debconf-kde-helper").spawn()?)
}

#[tokio::main]
async fn run_backend() -> Result<()> {
    let lock_path = Path::new("/run/lock/deb-installer");
    create_dir_all(lock_path.parent().unwrap()).await?;
    let _lock = oma_utils::get_file_lock(lock_path)?;

    let backend = Backend::default();
    let exit = backend.exit.clone();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let _conn = connection::Builder::system()?
        .name("io.aosc.DebInstaller")?
        .serve_at("/io/aosc/DebInstaller", backend)?
        .build()
        .await?;

    debug!("zbus session created");

    exit.notified().await;
    debug!("Bye.");

    Ok(())
}
