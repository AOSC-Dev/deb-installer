use std::{
    env::{self, current_exe},
    io::{BufRead, BufReader, PipeReader},
    path::{Path, PathBuf},
    process::{Child, Command, exit},
    sync::{OnceLock, atomic::Ordering},
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, Result};
use backend::Backend;
use clap::Parser;
use gettextrs::{bind_textdomain_codeset, bindtextdomain, textdomain};
use oma_pm::{apt::OmaApt, matches::PackagesMatcher, pkginfo::OmaPackage};
use tokio::fs::create_dir_all;
use tracing::{debug, error, level_filters::LevelFilter};
use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use zbus::{Connection, connection, proxy};

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
    async fn get_progress(&self) -> zbus::Result<u32>;
    async fn ping(&self) -> zbus::Result<String>;
    async fn is_finished(&self) -> zbus::Result<bool>;
    async fn finished_get_result(&self) -> zbus::Result<String>;
    async fn exit(&self) -> zbus::Result<bool>;
}

#[derive(Debug)]
pub enum Progress {
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

pub fn on_install(argc: String, tx: flume::Sender<Progress>) -> JoinHandle<Result<()>> {
    thread::spawn(move || -> Result<()> {
        let txc = tx.clone();
        let txc2 = tx.clone();

        let (mut backend_child, reader) = start_backend()?;
        let reader = BufReader::new(reader);

        thread::spawn(move || {
            reader.lines().for_each(|line| match line {
                Ok(line) => {
                    let _ = txc.send(Progress::Message(
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
                        let _ = txc2.send(Progress::Err(format!(
                            "Backend process exited with status: {status}"
                        )));
                    } else {
                        let _ = txc2.send(Progress::Done);
                    }
                }
                Err(e) => {
                    let _ = txc2.send(Progress::Err(format!("Failed to wait for backend: {e}")));
                }
            }
        });

        on_install_inner(argc, tx)
    })
}

#[tokio::main]
async fn on_install_inner(argc: String, tx: flume::Sender<Progress>) -> Result<()> {
    let conn = Connection::system().await?;
    let client = OmaClientProxy::new(&conn).await?;

    loop {
        if client.ping().await.is_ok() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    let _ = client.install(argc).await?;

    loop {
        let is_finished = client.is_finished().await?;
        let progress = client.get_progress().await?;

        let _ = tx.send_async(Progress::Percent(progress)).await;

        if progress == 100 || is_finished {
            let res = client.finished_get_result().await?;
            let _ = tx.send_async(Progress::Message(res.clone())).await;
            if res == "ok" {
                let _ = tx.send_async(Progress::Done).await;
            } else {
                let _ = tx.send_async(Progress::Err(res)).await;
            }
            let _ = client.exit().await;
            return Ok(());
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
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

    loop {
        if exit.load(Ordering::Relaxed) {
            debug!("Bye.");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
}
