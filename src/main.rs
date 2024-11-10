use std::{
    env::current_exe,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{self, exit, Child, Command, Stdio},
    sync::atomic::Ordering,
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::Result;
use backend::Backend;
use clap::Parser;
use oma_pm::{
    apt::{AptConfig, OmaApt, OmaAptArgs},
    pkginfo::PackageInfo,
};
use slint::ComponentHandle;
use tracing::{debug, error, level_filters::LevelFilter};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};
use zbus::{proxy, Connection, ConnectionBuilder};

use crate::deb_installer::DebInstaller;

mod backend;
mod deb_installer;

#[derive(Parser, Debug)]
#[clap(about, version, author)]
struct Args {
    package: Option<PathBuf>,
    #[clap(long)]
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
    async fn exit(&self) -> zbus::Result<bool>;
}

enum Progress {
    Percent(u32),
    Message(String),
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
            eprintln!("Package path does not exist");
            exit(1);
        }

        if package.extension().map(|x| x.to_string_lossy()) != Some("deb".into()) {
            eprintln!(
                "Usage: {} /path/to/foo.deb",
                current_exe().unwrap().display()
            );
            exit(1);
        }

        ui(package.canonicalize().unwrap());
    } else if backend {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run_backend())
            .unwrap();
    } else {
        eprintln!(
            "Usage: {} /path/to/foo.deb",
            current_exe().unwrap().display()
        );
        exit(1);
    }
}

fn ui(pkg: PathBuf) {
    let arg = pkg.display().to_string();

    let debconf_helper = start_kde_debconf();

    if let Err(e) = debconf_helper {
        error!("Failed to start debconf-kde-helper: {e}");
    }

    let installer = DebInstaller::new().unwrap();

    let info = get_package_info(&arg);

    let (status, info) = match info {
        Ok(info) => ("is ok to install".to_string(), Some(info)),
        Err(e) => (e.to_string(), None),
    };

    installer.set_status(status.into());

    if let Some(info) = info {
        installer.set_package(info.package.to_string().into());
        installer.set_metadata(info.to_string().into());
        installer.set_description(info.description.into());
    }

    let argc = arg.to_string();

    let ui_weak = installer.as_weak();
    let ui_weak_2 = ui_weak.clone();

    let (progress_tx, progress_rx) = flume::unbounded();

    installer.on_install(move || {
        let t = on_install(argc.clone(), progress_tx.clone());

        let ui_weak_2 = ui_weak_2.clone();

        thread::spawn(move || {
            let res = t.join().unwrap();
            match res {
                Ok(_) => {
                    let _ = ui_weak_2.upgrade_in_event_loop(|ui| {
                        let old = ui.get_message();
                        let new_msg = format!("{}Install is finished\n", old);
                        ui.set_message(new_msg.into());
                    });
                }
                Err(e) => {
                    let _ = ui_weak_2.upgrade_in_event_loop(move |ui| {
                        let old = ui.get_message();
                        let new_msg = format!("{}{}\n", old, e);
                        ui.set_message(new_msg.into());
                    });
                }
            }
        });
    });

    installer.on_close(|| {
        process::exit(0);
    });

    thread::spawn(move || loop {
        let Ok(progress) = progress_rx.recv() else {
            break;
        };

        match progress {
            Progress::Percent(p) => {
                let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.set_progress(p as f32 / 100.0);
                });
            }
            Progress::Message(msg) => {
                let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                    let old = ui.get_message();
                    let new_msg = format!("{}{}\n", old, msg);
                    ui.set_message(new_msg.into());
                });
            }
        }
    });

    installer.run().unwrap();
}

fn get_package_info(arg: &str) -> Result<PackageInfo> {
    let mut apt = OmaApt::new(
        vec![arg.to_string()],
        OmaAptArgs::builder().build(),
        false,
        AptConfig::new(),
    )?;

    let (pkgs, _) = apt.select_pkg(&[arg], false, true, false)?;
    apt.install(&pkgs, true)?;
    let pkg = pkgs.first().unwrap();
    let info = pkg.pkg_info(&apt.cache)?;
    apt.resolve(true, false, false)?;

    Ok(info)
}

fn on_install(argc: String, tx: flume::Sender<Progress>) -> JoinHandle<Result<()>> {
    let t = thread::spawn(move || -> Result<()> {
        let mut backend_child = start_backend()?;

        let txc = tx.clone();
        let txc2 = tx.clone();

        thread::spawn(move || {
            if let Some(out) = backend_child.stdout.take() {
                let reader = BufReader::new(out);
                reader.lines().for_each(|line| match line {
                    Ok(line) => {
                        if let Err(e) = txc.send(Progress::Message(
                            console::strip_ansi_codes(&line).to_string(),
                        )) {
                            error!("{e}");
                        }
                    }
                    Err(e) => {
                        error!("{e}")
                    }
                });
            }
        });

        thread::spawn(move || {
            if let Some(out) = backend_child.stderr.take() {
                let reader = BufReader::new(out);
                reader.lines().for_each(|line| match line {
                    Ok(line) => {
                        if let Err(e) = txc2.send(Progress::Message(line)) {
                            error!("{e}");
                        }
                    }
                    Err(e) => {
                        error!("{e}")
                    }
                });
            }
        });

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        rt.block_on(on_install_inner(argc, tx))
    });

    t
}

async fn on_install_inner(argc: String, tx: flume::Sender<Progress>) -> Result<()> {
    let conn = Connection::system().await?;
    let client = OmaClientProxy::new(&conn).await?;

    loop {
        let Ok(_msg) = client.ping().await else {
            thread::sleep(Duration::from_millis(100));
            continue;
        };

        break;
    }

    client.install(argc).await?;

    loop {
        let is_finished = client.is_finished().await?;
        let progress = client.get_progress().await?;

        tx.send_async(Progress::Percent(progress)).await?;

        if progress == 100 || is_finished {
            client.exit().await?;
            return Ok(());
        }

        thread::sleep(Duration::from_millis(100));
    }
}

fn start_backend() -> Result<Child> {
    let child = Command::new("pkexec")
        .arg(std::env::current_exe()?)
        .arg("--backend")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    Ok(child)
}

fn start_kde_debconf() -> Result<Child> {
    Ok(Command::new("debconf-kde-helper").spawn()?)
}

async fn run_backend() -> Result<()> {
    let backend = Backend::default();

    let exit = backend.exit.clone();

    let _conn = ConnectionBuilder::system()?
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
