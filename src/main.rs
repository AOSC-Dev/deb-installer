use std::{
    future::pending,
    path::PathBuf,
    process::{exit, Command},
    thread,
    time::Duration,
};

use backend::Backend;
use clap::Parser;
use oma_pm::apt::{AptConfig, OmaApt, OmaAptArgs};
use slint::ComponentHandle;
use tracing::debug;
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
}

fn main() {
    let Args { package, backend } = Args::parse();

    if let Some(package) = package {
        if !package.exists() {
            eprintln!("Package path does not exist");
            exit(1);
        }

        if package.extension().map(|x| x.to_string_lossy()) != Some("deb".into()) {
            eprintln!("Usage: deb-installer foo.deb");
            exit(1);
        }

        main_window(package);
    } else if backend {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(run_backend()).unwrap();
    }
}

fn main_window(arg: PathBuf) {
    let arg = arg.display().to_string();

    let mut apt = OmaApt::new(
        vec![arg.to_string()],
        OmaAptArgs::builder().build(),
        false,
        AptConfig::new(),
    )
    .unwrap();

    let (pkgs, _) = apt.select_pkg(&[arg.as_str()], false, true, false).unwrap();

    apt.install(&pkgs, false).unwrap();

    let res = apt.resolve(true, false, false);

    let pkg = pkgs.first().unwrap();

    let info = pkg.pkg_info(&apt.cache).unwrap();
    let info_str = info.to_string();

    let installer = DebInstaller::new().unwrap();

    installer.set_status(match res {
        Ok(_) => "is ok to install".into(),
        Err(e) => e.to_string().into(),
    });

    installer.set_package(pkg.raw_pkg.name().into());
    installer.set_metadata(info_str.into());

    let argc = arg.to_string();

    installer.on_install(move || {
        on_install(argc.clone());
    });

    installer.run().unwrap();
}

fn on_install(argc: String) {
    thread::spawn(|| {
        let _ = start_backend();
    });

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let conn = rt.block_on(Connection::system()).unwrap();
    let client = rt.block_on(OmaClientProxy::new(&conn)).unwrap();

    loop {
        let Ok(_msg) = rt.block_on(client.ping()) else {
            thread::sleep(Duration::from_millis(100));
            continue;
        };

        break;
    }

    let _res = rt
        .block_on(send_install_request(&client, argc.clone()))
        .unwrap();

    loop {
        let progress = rt.block_on(get_progress(&client)).unwrap();
        if progress == 101 {
            break;
        }
        println!("{}", progress);
        thread::sleep(Duration::from_millis(100));
    }
}

fn start_backend() -> anyhow::Result<()> {
    Command::new("pkexec")
        .arg(std::env::current_exe().unwrap())
        .arg("--backend")
        .output()?;

    Ok(())
}

async fn send_install_request(client: &OmaClientProxy<'_>, path: String) -> anyhow::Result<bool> {
    let b = client.install(path).await?;

    Ok(b)
}

async fn get_progress(client: &OmaClientProxy<'_>) -> anyhow::Result<u32> {
    let b = client.get_progress().await?;

    Ok(b)
}

async fn run_backend() -> anyhow::Result<()> {
    let backend = Backend::default();

    let _conn = ConnectionBuilder::system()?
        .name("io.aosc.DebInstaller")?
        .serve_at("/io/aosc/DebInstaller", backend)?
        .build()
        .await?;

    debug!("zbus session created");

    pending::<()>().await;

    Ok(())
}
