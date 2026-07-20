use std::{
    cell::OnceCell,
    env,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Chain, Context};
use apt_auth_config::AuthConfig;
use flume::unbounded;
use oma_fetch::SingleDownloadError;
use oma_history::HistoryInfo;
use oma_pm::{
    CommitConfig, PackageDownloadEvent,
    apt::{InstallProgressOpt::TermLike, OmaApt, OmaAptArgs},
    matches::PackagesMatcher,
    progress::InstallProgressManager,
    sort::SummarySort,
};
use oma_utils::human_bytes::HumanBytes;
use tracing::{debug, error, info};
use zbus::object_server::SignalEmitter;
use zbus::interface;

pub struct Backend {
    pub exit: Arc<AtomicBool>,
}

impl Backend {
    pub fn new() -> Self {
        Self {
            exit: Arc::new(AtomicBool::new(false)),
        }
    }
}

struct DebInstallerInstallProgressManager {
    progress: Arc<AtomicU32>,
    rt: tokio::runtime::Handle,
    ctxt: SignalEmitter<'static>,
}

pub trait RenderPackagesDownloadProgress {
    fn render_progress(&mut self, rx: &flume::Receiver<PackageDownloadEvent>);
}

impl Default for NoProgressBar {
    fn default() -> Self {
        Self {
            timer: Instant::now(),
            total_size: OnceCell::new(),
            old_downloaded: 0,
            progress: 0,
        }
    }
}

pub struct NoProgressBar {
    timer: Instant,
    total_size: OnceCell<u64>,
    old_downloaded: u64,
    progress: u64,
}

impl RenderPackagesDownloadProgress for NoProgressBar {
    fn render_progress(&mut self, rx: &flume::Receiver<PackageDownloadEvent>) {
        while let Ok(event) = rx.recv() {
            if self.download_event(event) {
                break;
            }
        }
    }
}

impl NoProgressBar {
    fn download_event(&mut self, event: PackageDownloadEvent) -> bool {
        match event {
            PackageDownloadEvent::ChecksumMismatch {
                index: _,
                filename,
                times,
            } => {
                error!(
                    "Checksum verification failed for {}. Retrying {} times ...",
                    filename, times
                );
            }
            PackageDownloadEvent::GlobalProgressAdd(inc) => {
                self.progress += inc;
                self.print_progress();
            }
            PackageDownloadEvent::GlobalProgressSub(num) => {
                self.progress = self.progress.saturating_sub(num);
                self.print_progress();
            }
            PackageDownloadEvent::NextUrl {
                index: _,
                file_name,
                err,
            } => {
                handle_no_pb_download_error(file_name, err);
                info!("Retrying using the next available mirror ...");
            }
            PackageDownloadEvent::DownloadDone { index: _, msg } => {
                info!("Done: {msg}");
            }
            PackageDownloadEvent::AllDone => return true,
            PackageDownloadEvent::NewGlobalProgressBar(total_size) => {
                self.total_size.get_or_init(|| total_size);
            }
            PackageDownloadEvent::Failed { file_name, error } => {
                handle_no_pb_download_error(file_name, error);
            }
            _ => {}
        };

        false
    }

    fn print_progress(&mut self) {
        let elapsed = self.timer.elapsed();
        if elapsed >= Duration::from_secs(3) {
            if let Some(total_size) = self.total_size.get() {
                info!(
                    "{} / {} ({}/s)",
                    HumanBytes(self.progress),
                    HumanBytes(*total_size),
                    HumanBytes((self.progress - self.old_downloaded) / elapsed.as_secs())
                );
                self.old_downloaded = self.progress;
            } else {
                info!("Downloaded {}", HumanBytes(self.progress));
            }
            self.timer = Instant::now();
        }
    }
}

fn handle_no_pb_download_error(file_name: String, error: SingleDownloadError) {
    let errs = Chain::new(&error).collect::<Vec<_>>();
    let first_cause = errs.first().unwrap().to_string();
    let last = errs.iter().skip(1).last();

    if let Some(last_cause) = last {
        let reason = format!("{}: {}", first_cause, last_cause);
        error!(
            "Failed to download package {}, Reason: {}.",
            file_name, reason
        );
    } else {
        error!(
            "Failed to download package {}, Reason: {}.",
            file_name, first_cause
        );
    }
}

impl InstallProgressManager for DebInstallerInstallProgressManager {
    fn status_change(&self, _pkgname: &str, steps_done: u64, total_steps: u64) {
        let percent = steps_done as f32 / total_steps as f32;
        let percent = (percent * 100.0).round() as u32;
        self.progress.store(percent, Ordering::SeqCst);

        let _ = self.rt.block_on(Backend::progress(&self.ctxt, percent));
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
    #[zbus(signal)]
    async fn progress(ctxt: &SignalEmitter<'_>, percent: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn finished(ctxt: &SignalEmitter<'_>, result: String) -> zbus::Result<()>;

    fn install(
        &mut self,
        #[zbus(signal_emitter)] ctxt: SignalEmitter<'_>,
        path: String,
    ) -> bool {
        let install_pm = Arc::new(AtomicU32::new(0));
        let install_pm_clone = install_pm.clone();
        let ctxt = ctxt.into_owned();

        thread::spawn(move || -> anyhow::Result<()> {
            let rt = tokio::runtime::Runtime::new()
                .context("Failed to create signal runtime")?;
            let rt = rt.handle().clone();
            unsafe {
                env::set_var("DEBIAN_FRONTEND", "passthrough");
                env::set_var("DEBCONF_PIPE", "/tmp/debkonf-sock");
            }

            let mut apt =
                OmaApt::new(vec![path.to_string()], OmaAptArgs::builder().build(), false)?;

            let matcher = PackagesMatcher::builder()
                .filter_candidate(true)
                .filter_downloadable_candidate(false)
                .select_dbg(false)
                .cache(&apt.cache)
                .build();

            let pkgs = matcher.match_local_glob(&path)?;

            apt.install(&pkgs, true)?;
            apt.resolve(true, false)?;

            let auth = AuthConfig::system("/").ok();

            let client = oma_fetch::reqwest::Client::builder()
                .user_agent("oma/1.14.514")
                .build()
                .map(|client| {
                    if let Some(auth) = auth {
                        reqwest_middleware::ClientBuilder::new(client)
                            .with_init(apt_auth_config::reqwuest::AuthMiddleware::new(auth))
                            .build()
                    } else {
                        client.into()
                    }
                })?;

            let op = apt.build_transaction(SummarySort::default(), |_| false, |_| false)?;

            let (download_tx, download_rx) = unbounded();

            thread::spawn(move || {
                let mut pb = NoProgressBar::default();
                pb.render_progress(&download_rx);
            });

            #[cfg(feature = "aosc")]
            let mut history = oma_history::History::new("/var/lib/oma/history.db", true, false)?;

            #[cfg(feature = "aosc")]
            let id = history.write(HistoryInfo {
                summary: &op,
                start_time: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64,
                success: false,
                is_fix_broken: false,
                is_undo: false,
                topics_enabled: Vec::new(),
                topics_disabled: Vec::new(),
            })?;

            let result = apt.commit(
                TermLike(Box::new(DebInstallerInstallProgressManager {
                    progress: install_pm_clone.clone(),
                    rt: rt.clone(),
                    ctxt: ctxt.clone(),
                })),
                &op,
                &client,
                CommitConfig {
                    network_thread: None,
                    download_only: false,
                },
                None,
                move |event| {
                    if let Err(e) = download_tx.send(event) {
                        debug!("Send progress channel got error: {}; maybe check archive work still in progress", e);
                    }
                },
            );

            #[cfg(feature = "aosc")]
            history.edit_status(id, result.is_ok())?;

            install_pm_clone.store(100, Ordering::SeqCst);

            // Emit Finished signal
            let result_str = match &result {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("{e}"),
            };

            let _ = rt.block_on(Backend::finished(&ctxt, result_str));

            Ok(result?)
        });

        true
    }

    fn ping(&self) -> &'static str {
        "pong"
    }

    fn exit(&self) -> bool {
        self.exit.store(true, Ordering::Relaxed);
        true
    }
}
