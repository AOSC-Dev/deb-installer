use std::{
    env,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
};

use apt_auth_config::AuthConfig;
use oma_fetch::DownloadProgressControl;
use oma_pm::{
    apt::{AptConfig, CommitDownloadConfig, OmaApt, OmaAptArgs, SummarySort},
    progress::InstallProgressManager,
};
use reqwest::ClientBuilder;
use zbus::interface;

pub struct Backend {
    install_thread: Option<JoinHandle<Result<(), anyhow::Error>>>,
    pm: Arc<DownloadProgressManager>,
    install_pm: Arc<AtomicU32>,
    pub exit: Arc<AtomicBool>,
}

impl Default for Backend {
    fn default() -> Self {
        Self {
            install_thread: None,
            pm: Arc::new(DownloadProgressManager::default()),
            install_pm: Arc::new(AtomicU32::new(0)),
            exit: Arc::new(AtomicBool::new(false)),
        }
    }
}

pub struct DownloadProgressManager {
    progress: AtomicU64,
}

impl Default for DownloadProgressManager {
    fn default() -> Self {
        Self {
            progress: AtomicU64::new(0),
        }
    }
}

impl DownloadProgressControl for DownloadProgressManager {
    fn checksum_mismatch_retry(&self, _index: usize, _filename: &str, _times: usize) {}

    fn global_progress_set(&self, _num: &std::sync::atomic::AtomicU64) {
        self.progress
            .store(_num.load(Ordering::SeqCst), Ordering::SeqCst);
    }

    fn progress_done(&self, _index: usize) {}

    fn new_progress_spinner(&self, _index: usize, _msg: &str) {}

    fn new_progress_bar(&self, _index: usize, _msg: &str, _size: u64) {}

    fn progress_inc(&self, _index: usize, _num: u64) {}

    fn progress_set(&self, _index: usize, _num: u64) {}

    fn failed_to_get_source_next_url(&self, _index: usize, _err: &str) {}

    fn download_done(&self, _index: usize, _msg: &str) {}

    fn all_done(&self) {}

    fn new_global_progress_bar(&self, _total_size: u64) {}
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
        let pmc = self.pm.clone();
        let install_pm_clone = self.install_pm.clone();
        let thread = Some(thread::spawn(move || -> anyhow::Result<()> {
            env::set_var("DEBIAN_FRONTEND", "passthrough");
            env::set_var("DEBCONF_PIPE", "/tmp/debkonf-sock");

            let mut apt = OmaApt::new(
                vec![path.to_string()],
                OmaAptArgs::builder().build(),
                false,
                AptConfig::new(),
            )?;

            let (pkgs, _) = apt.select_pkg(&[path.as_str()], false, true, false)?;

            apt.install(&pkgs, true)?;
            apt.resolve(true, false)?;

            let client = ClientBuilder::new().user_agent("deb_installer").build()?;
            let op = apt.summary(SummarySort::NoSort, |_| false, |_| false)?;

            apt.commit(
                &client,
                CommitDownloadConfig {
                    auth: &AuthConfig::system("/")?,
                    network_thread: None,
                },
                &*pmc,
                Box::new(DebInstallerInstallProgressManager {
                    progress: install_pm_clone.clone(),
                }),
                op,
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

    fn ping(&self) -> &'static str {
        "pong"
    }

    fn exit(&mut self) -> bool {
        self.exit.store(true, Ordering::Relaxed);
        true
    }
}
