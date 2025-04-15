use std::{
    env::current_exe,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{self, Child, Command, Stdio, exit},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, Result};
use backend::Backend;
use clap::Parser;
use num_enum::IntoPrimitive;
use oma_pm::{
    apt::{AptConfig, OmaApt, OmaAptArgs, OmaAptError, SummarySort},
    matches::PackagesMatcher,
    pkginfo::OmaPackage,
};
use oma_utils::human_bytes::HumanBytes;
use slint::{ComponentHandle, ToSharedString};
use tracing::{debug, error, level_filters::LevelFilter};
use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use zbus::{Connection, connection, proxy};

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
    async fn finished_get_result(&self) -> zbus::Result<String>;
    async fn exit(&self) -> zbus::Result<bool>;
}

enum Progress {
    Percent(u32),
    Message(String),
    Done,
}

fn u8_oma_pm_errors(error: &OmaAptError) -> u8 {
    match error {
        OmaAptError::DependencyIssue(_) => 6,
        OmaAptError::DiskSpaceInsufficient(_, _) => 17,
        _ => 255,
    }
}

fn main() {
    #[cfg(feature = "debug")]
    slint::init_translations!(concat!(env!("CARGO_MANIFEST_DIR"), "/mo/"));

    #[cfg(not(feature = "debug"))]
    slint::init_translations!("/usr/share/locale");

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
                "Usage: {} /path/to/package.deb",
                current_exe().unwrap().display()
            );
            exit(1);
        }

        ui(package.canonicalize().unwrap());
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

fn ui(pkg: PathBuf) {
    let arg = pkg.display().to_string();

    let debconf_helper = start_kde_debconf();

    let mut debconf_child = None;

    match debconf_helper {
        Err(e) => error!("Failed to start debconf-kde-helper: {e}"),
        Ok(child) => debconf_child = Some(child),
    }

    let installer = DebInstaller::new().unwrap();

    set_info(&arg, &installer);

    let argc = arg.to_string();

    let ui_weak = installer.as_weak();
    let ui_weak_2 = ui_weak.clone();

    let (progress_tx, progress_rx) = flume::unbounded();

    installer.on_install(move || {
        let t = on_install(argc.clone(), progress_tx.clone());

        let ui_weak_2 = ui_weak_2.clone();

        thread::spawn(move || {
            let res = t.join().unwrap();
            if let Err(e) = res {
                let _ = ui_weak_2.upgrade_in_event_loop(move |ui| {
                    let old = ui.get_message();
                    let new_msg = slint::format!("{}{}\n", old, e);
                    ui.set_message(new_msg);
                });
            }
        });
    });

    thread::spawn(move || {
        loop {
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
                        let new_msg = slint::format!("{}{}\n", old, msg);
                        ui.set_message(new_msg);
                    });
                }
                Progress::Done => {
                    let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                        ui.set_finished(true);
                    });
                }
            }
        }
    });

    handle_exit(&installer, debconf_child);

    installer.run().unwrap();
}

#[derive(IntoPrimitive)]
#[repr(u8)]
enum InstallAction {
    Install = 0,
    ReInstall = 1,
    Upgrade = 2,
    Downgrade = 3,
}

fn set_info(arg: &str, installer: &DebInstaller) {
    let apt = OmaApt::new(
        vec![arg.to_string()],
        OmaAptArgs::builder().build(),
        false,
        AptConfig::new(),
    );

    match apt {
        Ok(mut apt) => {
            let info = get_package(&mut apt, arg);

            let (pkg, mut can_install) = match info {
                Ok(info) => {
                    installer.set_err_num(0);
                    (Some(info), true)
                }
                Err(e) => {
                    if let Some(e) = e.downcast_ref::<OmaAptError>() {
                        let err_num = u8_oma_pm_errors(e);
                        installer.set_err_num(err_num.into());
                        installer.set_err(e.to_string().into());
                    } else {
                        installer.set_err(e.to_string().into());
                    }
                    (None, false)
                }
            };

            if let Err(e) = apt.resolve(true, false) {
                let err_num = u8_oma_pm_errors(&e);
                installer.set_err_num(err_num.into());
                installer.set_err(e.to_string().into());
                can_install = false;
            }

            if let Some(oma_pkg) = pkg {
                let pkg = oma_pkg.package(&apt.cache);
                let version = oma_pkg.version(&apt.cache);
                let info = oma_pkg.pkg_info(&apt.cache);

                if let Err(e) = apt
                    .summary(SummarySort::NoSort, |_| false, |_| false)
                    .and_then(|summary| apt.check_disk_size(&summary))
                {
                    let err_num = u8_oma_pm_errors(&e);
                    installer.set_err_num(err_num.into());
                    installer.set_err(e.to_string().into());
                    can_install = false;
                }

                let info = match info {
                    Ok(info) => Some(info),
                    Err(e) => {
                        let err_num = u8_oma_pm_errors(&e);
                        installer.set_err_num(err_num.into());
                        installer.set_err(e.to_string().into());
                        can_install = false;
                        None
                    }
                };

                installer.set_can_install(can_install);

                if let Some(info) = info {
                    installer.set_package(info.package.to_string().into());
                    installer.set_description(info.short_description.into());
                    installer.set_version(info.version.to_string().into());
                    installer.set_installed_size(HumanBytes(info.install_size).to_shared_string());

                    let mut archs = apt.get_architectures();

                    // Should support noarch package
                    archs.push("all".to_string());

                    if !archs.contains(&version.arch().to_string()) {
                        installer.set_err_num(100);
                        installer.set_can_install(false);
                    }

                    let mut action = InstallAction::Install;

                    if let Some(installed) = pkg.installed() {
                        action = match version.cmp(&installed) {
                            std::cmp::Ordering::Less => InstallAction::Downgrade,
                            std::cmp::Ordering::Equal => InstallAction::ReInstall,
                            std::cmp::Ordering::Greater => InstallAction::Upgrade,
                        };
                    }

                    let action: u8 = action.into();
                    installer.set_action(action.into());
                }
            } else {
                installer.set_can_install(false);
            }
        }
        Err(e) => {
            installer.set_status(e.to_string().into());
        }
    };
}

fn handle_exit(installer: &DebInstaller, debconf_child: Option<Child>) {
    let kill_debconf = Arc::new(AtomicBool::new(false));
    let can_exit = Arc::new(AtomicBool::new(false));
    let cec = can_exit.clone();
    let cec2 = can_exit.clone();
    let kc = kill_debconf.clone();
    let kc2 = kill_debconf.clone();

    let weak = installer.as_weak();

    let has_debconf_child = debconf_child.is_some();

    // 关闭按钮
    installer.on_close(move || {
        if has_debconf_child {
            // 杀掉 debconf helper 进程
            kill_debconf.store(true, Ordering::SeqCst);
            // 等待是否可以退出
            while !cec.load(Ordering::SeqCst) {}
        }
        process::exit(0);
    });

    // 窗口关闭按钮
    installer.window().on_close_requested(move || {
        let ui = weak.unwrap();
        if !ui.get_finished() && ui.get_is_install() {
            return slint::CloseRequestResponse::KeepWindowShown;
        }

        if has_debconf_child {
            kc2.store(true, Ordering::SeqCst);
            while !cec2.load(Ordering::SeqCst) {}
        }

        slint::CloseRequestResponse::HideWindow
    });

    if let Some(mut child) = debconf_child {
        thread::spawn(move || {
            loop {
                // 接收杀死 debconf-helper 请求
                if kc.load(Ordering::SeqCst) {
                    let _ = child.kill();
                    can_exit.store(true, Ordering::SeqCst);
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        });
    }
}

fn get_package<'a>(apt: &'a mut OmaApt, arg: &'a str) -> Result<OmaPackage> {
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

fn on_install(argc: String, tx: flume::Sender<Progress>) -> JoinHandle<Result<()>> {
    let t = thread::spawn(move || -> Result<()> {
        let txc = tx.clone();
        let txc2 = tx.clone();
        let txc3 = tx.clone();

        let mut backend_child = start_backend()?;
        let stdout = backend_child.stdout.take();
        let stderr = backend_child.stderr.take();

        thread::spawn(move || {
            if let Some(out) = stdout {
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
            if let Some(out) = stderr {
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

        thread::spawn(move || {
            let _wait = backend_child.wait();
            if let Err(e) = txc3.send(Progress::Done) {
                error!("{e}");
            }
        });

        on_install_inner(argc, tx)
    });

    t
}

#[tokio::main]
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
            let res = client.finished_get_result().await?;
            tx.send_async(Progress::Message(res)).await?;
            tx.send_async(Progress::Done).await?;
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

#[tokio::main]
async fn run_backend() -> Result<()> {
    let backend = Backend::default();

    let exit = backend.exit.clone();

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
