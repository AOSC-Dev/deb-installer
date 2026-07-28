use std::{
    cell::OnceCell,
    env,
    io::{self, BufRead, Write},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Chain;
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

struct DebInstallerInstallProgressManager {
    progress: Arc<AtomicU32>,
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
        let old = self.progress.swap(percent, Ordering::SeqCst);

        if old != percent {
            let _ = writeln!(io::stdout(), "progress {percent}");
            let _ = io::stdout().flush();
        }
    }

    fn no_interactive(&self) -> bool {
        which::which("debconf-kde-helper").is_err()
    }

    fn use_pty(&self) -> bool {
        false
    }
}

/// Walk up the PPID chain from ourselves and verify the frontend PID
/// is one of our ancestors (frontend → pkexec → us).
/// Also verifies the frontend's executable matches our own binary.
fn verify_frontend_ancestry(frontend_pid: u32) -> bool {
    let mut system = sysinfo::System::new();
    let self_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to get self exe: {e}");
            return false;
        }
    };

    // Verify frontend's executable matches ours
    let frontend_pid = sysinfo::Pid::from_u32(frontend_pid);
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[frontend_pid]), false);
    match system.process(frontend_pid) {
        Some(proc) => {
            if proc.exe() != Some(&self_exe) {
                error!("Frontend exe {:?} != self {:?}", proc.exe(), self_exe);
                return false;
            }
        }
        None => {
            error!("Frontend process {frontend_pid:?} not found");
            return false;
        }
    }

    // Walk the PPID chain from ourselves upward: us → pkexec → frontend
    let self_pid = sysinfo::Pid::from_u32(std::process::id());
    let mut current = self_pid;
    loop {
        if current == frontend_pid {
            return true;
        }

        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[current]), false);
        let ppid = match system.process(current).and_then(|p| p.parent()) {
            Some(parent) => parent,
            None => return false,
        };

        if ppid == current || ppid.as_u32() == 0 {
            return false;
        }

        current = ppid;
    }
}

/// Run the backend: read commands from stdin, write results to stdout.
pub fn run_backend() -> anyhow::Result<()> {
    let lock_path = std::path::Path::new("/run/lock/deb-installer");
    std::fs::create_dir_all(lock_path.parent().unwrap())?;
    let _lock = oma_utils::get_file_lock(lock_path)?;

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let mut auth_token = String::new();
    let mut frontend_pid: Option<u32> = None;

    for line in io::stdin().lock().lines() {
        let line = line?;

        // Every line must start with the auth token
        let (token, rest) = match line.split_once(' ') {
            Some(pair) => pair,
            None => {
                error!("Malformed command (missing token): {line}");
                continue;
            }
        };

        // First valid line establishes the token
        if auth_token.is_empty() {
            auth_token = token.to_string();
        } else if token != auth_token {
            error!("Auth token mismatch: ignoring command");
            continue;
        }

        let mut parts = rest.splitn(2, ' ');
        match parts.next() {
            Some("init") => {
                if frontend_pid.is_some() {
                    error!("init already called rejecting duplicate");
                    continue;
                }
                if let Ok(pid) = parts.next().unwrap_or("").parse::<u32>() {
                    frontend_pid = Some(pid);
                    debug!("Initialized with frontend PID: {pid}");
                } else {
                    error!("Invalid frontend PID in init command");
                }
            }
            Some("install") => {
                let pid = match frontend_pid {
                    Some(p) => p,
                    None => {
                        error!("Not initialized yet");
                        continue;
                    }
                };

                if !verify_frontend_ancestry(pid) {
                    error!("Frontend PID {pid} is not in our ancestry rejecting install");
                    continue;
                }

                let path = parts.next().unwrap_or("").to_string();
                if let Err(e) = do_install(path) {
                    error!("Install failed: {e}");
                }
            }
            Some("exit") => break,
            Some(other) => {
                error!("Unknown command: {other}");
            }
            None => {}
        }
    }

    debug!("Bye.");
    Ok(())
}

fn do_install(path: String) -> anyhow::Result<()> {
    let install_pm = Arc::new(AtomicU32::new(0));
    let install_pm_clone = install_pm.clone();

    unsafe {
        env::set_var("DEBIAN_FRONTEND", "passthrough");
        env::set_var("DEBCONF_PIPE", "/tmp/debkonf-sock");
    }

    let mut apt = OmaApt::new(vec![path.to_string()], OmaAptArgs::builder().build(), false)?;

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
                debug!(
                    "Send progress channel got error: {}; maybe check archive work still in progress",
                    e
                );
            }
        },
    );

    // Send result to stdout BEFORE any non-critical operations
    install_pm_clone.store(100, Ordering::SeqCst);
    let _ = writeln!(io::stdout(), "progress 100");
    let _ = io::stdout().flush();

    match &result {
        Ok(()) => {
            let _ = writeln!(io::stdout(), "result ok");
        }
        Err(e) => {
            let _ = writeln!(io::stdout(), "result {e}");
        }
    }
    let _ = io::stdout().flush();

    // Non-critical: history logging – don't fail the install if this breaks
    #[cfg(feature = "aosc")]
    if let Err(e) = history.edit_status(id, result.is_ok()) {
        error!("Failed to update install history: {e}");
    }

    result.map_err(Into::into)
}
