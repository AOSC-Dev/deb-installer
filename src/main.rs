use std::{
    env::current_exe,
    path::PathBuf,
    process::{exit, Command},
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
use tracing::{debug, info};
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

fn main() {
    let Args { package, backend } = Args::parse();

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

        ui(package);
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

    let (tx, rx) = flume::unbounded();

    installer.on_install(move || {
        let t = on_install(argc.clone(), tx.clone());
    });

    thread::spawn(move || loop {
        let Ok(progress) = rx.recv() else {
            break;
        };

        let _ = ui_weak.upgrade_in_event_loop(move |ui| ui.set_progress(progress as f32 / 100.0));
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

fn on_install(argc: String, tx: flume::Sender<u32>) -> JoinHandle<Result<()>> {
    let t = thread::spawn(move || {
        let _ = start_backend();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(on_install_inner(argc, tx))
    });

    t
}

async fn on_install_inner(argc: String, tx: flume::Sender<u32>) -> Result<()> {
    let conn = Connection::system().await?;
    let client = OmaClientProxy::new(&conn).await?;

    loop {
        let Ok(_msg) = client.ping().await else {
            thread::sleep(Duration::from_millis(100));
            continue;
        };

        break;
    }

    let _res = send_install_request(&client, argc.clone()).await?;

    loop {
        let is_finished = client.is_finished().await?;
        let progress = get_progress(&client).await?;

        tx.send_async(progress).await?;

        if progress == 100 || is_finished {
            client.exit().await?;
            return Ok(());
        }

        thread::sleep(Duration::from_millis(100));
    }
}

fn start_backend() -> Result<()> {
    Command::new("pkexec")
        .arg(std::env::current_exe()?)
        .arg("--backend")
        .spawn()?;

    Ok(())
}

async fn send_install_request(client: &OmaClientProxy<'_>, path: String) -> Result<bool> {
    let b = client.install(path).await?;

    Ok(b)
}

async fn get_progress(client: &OmaClientProxy<'_>) -> Result<u32> {
    let b = client.get_progress().await?;

    Ok(b)
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
            info!("Bye.");
            return Ok(());
        }

        thread::sleep(Duration::from_millis(100));
    }
}
