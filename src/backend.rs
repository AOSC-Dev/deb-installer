use std::{
    env,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread::{self, JoinHandle},
};

use flume::unbounded;
use oma_pm::{
    CommitNetworkConfig,
    apt::{AptConfig, OmaApt, OmaAptArgs, SummarySort},
    matches::PackagesMatcher,
    progress::InstallProgressManager,
};
use reqwest::ClientBuilder;
use tracing::debug;
use zbus::interface;

pub struct Backend {
    install_thread: Option<JoinHandle<Result<(), anyhow::Error>>>,
    install_pm: Arc<AtomicU32>,
    pub exit: Arc<AtomicBool>,
}

impl Default for Backend {
    fn default() -> Self {
        Self {
            install_thread: None,
            install_pm: Arc::new(AtomicU32::new(0)),
            exit: Arc::new(AtomicBool::new(false)),
        }
    }
}

struct DebInstallerInstallProgressManager {
    progress: Arc<AtomicU32>,
}

impl InstallProgressManager for DebInstallerInstallProgressManager {
    fn status_change(
        &self,
        _pkgname: &str,
        steps_done: u64,
        total_steps: u64,
        _config: &AptConfig,
    ) {
        let percent = steps_done as f32 / total_steps as f32;
        let percent = (percent * 100.0).round() as u32;
        self.progress.store(percent, Ordering::SeqCst);
    }

    fn no_interactive(&self) -> bool {
        which::which("debconf-kde-helper").is_err()
    }

    fn use_pty(&self) -> bool {
        false
    }
}

#[interface(name = "io.aosc.DebInstaller1")]
impl Backend {
    fn install(&mut self, path: String) -> bool {
        let install_pm_clone = self.install_pm.clone();
        let thread = Some(thread::spawn(move || -> anyhow::Result<()> {
            unsafe {
                env::set_var("DEBIAN_FRONTEND", "passthrough");
                env::set_var("DEBCONF_PIPE", "/tmp/debkonf-sock");
            }

            let mut apt = OmaApt::new(
                vec![path.to_string()],
                OmaAptArgs::builder().build(),
                false,
                AptConfig::new(),
            )?;

            let matcher = PackagesMatcher::builder()
                .filter_candidate(true)
                .filter_downloadable_candidate(false)
                .select_dbg(false)
                .cache(&apt.cache)
                .build();

            let pkgs = matcher.match_pkgs_and_versions_from_glob(&path)?;

            apt.install(&pkgs, true)?;
            apt.resolve(true, false)?;

            let client = ClientBuilder::new().user_agent("oma/1.14.514").build()?;
            let op = apt.summary(SummarySort::NoSort, |_| false, |_| false)?;

            let (download_tx, download_rx) = unbounded();

            thread::spawn(move || {
                while let Ok(event) = download_rx.recv() {
                    println!("{:?}", event);
                }
            });

            apt.commit(
                Box::new(DebInstallerInstallProgressManager {
                    progress: install_pm_clone.clone(),
                }),
                &op,
                &client,
                CommitNetworkConfig {
                    auth_config: None,
                    network_thread: None,
                },
                |event| async {
                    if let Err(e) = download_tx.send_async(event).await {
                        debug!("Send progress channel got error: {}; maybe check archive work still in progress", e);
                    }
                },
            )?;

            install_pm_clone.store(100, Ordering::SeqCst);

            Ok(())
        }));

        self.install_thread = thread;

        true
    }

    fn get_progress(&self) -> u32 {
        self.install_pm.load(Ordering::SeqCst)
    }

    fn is_finished(&self) -> bool {
        self.install_thread
            .as_ref()
            .is_some_and(|x| x.is_finished())
    }

    fn finished_get_result(&mut self) -> String {
        let Some(t) = self.install_thread.take() else {
            return "BUG: Install thread does not exist".to_string();
        };

        let res = match t.join() {
            Ok(t) => t,
            Err(e) => return format!("BUG: Failed to wait install thread: {:?}", e),
        };

        if let Err(e) = res {
            e.to_string()
        } else {
            "ok".to_string()
        }
    }

    fn ping(&self) -> &'static str {
        "pong"
    }

    fn exit(&self) -> bool {
        self.exit.store(true, Ordering::Relaxed);
        true
    }
}
