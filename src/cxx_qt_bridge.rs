#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    #[qenum]
    #[namespace = "InstallerActions"]
    enum InstallAction {
        Install,
        ReInstall,
        Upgrade,
        Downgrade,
    }

    #[qenum]
    #[namespace = "InstallStatus"]
    enum InstallStatus {
        OkToInstall,
        DependencyIssue,
        DiskSpaceInsufficient,
        Other,
    }

    #[qenum]
    #[namespace = "WorkStatus"]
    enum WorkStatus {
        Idle,
        Working,
        Done,
    }

    impl cxx_qt::Threading for DebInstaller {}

    extern "RustQt" {
        #[qobject]
        #[qml_element]
        #[qproperty(QString, package)]
        #[qproperty(QString, pkg_version)]
        #[qproperty(QString, pkg_description)]
        #[qproperty(QString, pkg_size)]
        #[qproperty(f32, progress)]
        #[qproperty(QString, message)]
        #[qproperty(QString, path)]
        #[qproperty(InstallAction, action)]
        #[qproperty(InstallStatus, status)]
        #[qproperty(WorkStatus, work_status)]
        #[qproperty(bool, has_error)]
        type DebInstaller = super::DebInstallerRust;

        #[qinvokable]
        fn install(self: Pin<&mut Self>);

        #[qinvokable]
        #[cxx_name = "loadPackageInfo"]
        fn load_package_info(self: Pin<&mut DebInstaller>);

        #[qinvokable]
        fn i18n(self: Pin<&mut Self>, text: QString) -> QString;

        #[qinvokable]
        fn i18n_with_arg(self: Pin<&mut Self>, text: QString, arg: QString) -> QString;
    }
}

use core::pin::Pin;
use cxx_qt::Threading;
use cxx_qt_lib::QString;
use gettextrs::gettext;
use oma_pm::apt::{OmaApt, OmaAptArgs, OmaAptError};

use crate::{
    ProgressEvent,
    cxx_qt_bridge::qobject::{InstallAction, InstallStatus, WorkStatus},
};

pub struct DebInstallerRust {
    pub package: QString,
    pub pkg_version: QString,
    pub pkg_description: QString,
    pub pkg_size: QString,
    pub path: QString,
    pub action: InstallAction,
    pub message: QString,
    pub progress: f32,
    pub status: InstallStatus,
    pub work_status: WorkStatus,
    pub has_error: bool,
}

impl Default for DebInstallerRust {
    fn default() -> Self {
        Self {
            package: QString::from(""),
            pkg_version: QString::from(""),
            pkg_description: QString::from(""),
            pkg_size: QString::from(""),
            message: QString::from(""),
            path: QString::from(""),
            progress: 0.0,
            action: InstallAction::Install,
            status: InstallStatus::OkToInstall,
            work_status: WorkStatus::Idle,
            has_error: false,
        }
    }
}

impl qobject::DebInstaller {
    pub fn load_package_info(self: Pin<&mut Self>) {
        let mut this = self;
        let path = crate::PKG_PATH.get().unwrap();
        let path_str = path.to_string_lossy().to_string();
        let apt = match OmaApt::new(vec![path_str.clone()], OmaAptArgs::builder().build(), false) {
            Ok(apt) => Some(apt),
            Err(e) => {
                this.as_mut().set_message(QString::from(e.to_string()));
                this.as_mut().set_status(InstallStatus::Other);
                None
            }
        };

        if let Some(mut apt) = apt
            && let Ok(pkg) = crate::get_package(&mut apt, &path_str)
        {
            this.as_mut().set_package(QString::from(pkg.raw_pkg.name()));
            this.as_mut()
                .set_pkg_version(QString::from(pkg.version(&apt.cache).version()));
            this.as_mut().set_pkg_description(QString::from(
                pkg.version(&apt.cache).summary().unwrap_or_default(),
            ));
            let size =
                oma_utils::human_bytes::HumanBytes(pkg.version_raw.installed_size()).to_string();
            this.as_mut().set_pkg_size(QString::from(&size));
            this.as_mut().set_path(QString::from(&path_str));

            let installed = pkg.package(&apt.cache).installed();
            let version = pkg.version(&apt.cache);

            let action = if let Some(installed) = installed {
                if version == installed {
                    InstallAction::ReInstall
                } else if version > installed {
                    InstallAction::Upgrade
                } else {
                    InstallAction::Downgrade
                }
            } else {
                InstallAction::Install
            };

            this.as_mut().set_action(action);
            let result = apt
                .install(&[pkg], true)
                .and_then(|_| apt.resolve(false, false));
            let mut status = InstallStatus::OkToInstall;

            if let Err(e) = result {
                match e {
                    OmaAptError::DependencyIssue { .. } => {
                        status = InstallStatus::DependencyIssue;
                    }
                    OmaAptError::DiskSpaceInsufficient(_, _) => {
                        status = InstallStatus::DiskSpaceInsufficient;
                    }
                    _ => {
                        status = InstallStatus::Other;
                    }
                }
            }

            this.as_mut().set_status(status);
        }
    }

    pub fn install(mut self: Pin<&mut Self>) {
        if *self.work_status() == WorkStatus::Working {
            return;
        }

        self.as_mut().set_work_status(WorkStatus::Working);
        self.as_mut().set_progress(0.0);
        self.as_mut()
            .set_message(QString::from("Connecting to backend..."));

        let qt_context = self.qt_thread();
        let pkg_path = self.path().to_string();

        std::thread::spawn(move || {
            let (tx, rx) = flume::unbounded();
            let handle = crate::on_install(pkg_path, tx);

            while let Ok(progress) = rx.recv() {
                let qt_context = qt_context.clone();

                let _ = qt_context.queue(move |mut ui| match progress {
                    ProgressEvent::Percent(p) => {
                        ui.as_mut().set_progress(p as f32 / 100.0);
                    }
                    ProgressEvent::Message(msg) => {
                        ui.as_mut().set_message(QString::from(msg));
                    }
                    ProgressEvent::Done => {
                        ui.as_mut().set_work_status(WorkStatus::Done);
                    }
                    ProgressEvent::Err(_) => {
                        ui.as_mut().set_has_error(true);
                        ui.as_mut().set_work_status(WorkStatus::Done);
                    }
                });
            }

            let _ = handle.join();
        });
    }

    pub fn i18n(self: Pin<&mut Self>, text: QString) -> QString {
        QString::from(gettext(text))
    }

    pub fn i18n_with_arg(self: Pin<&mut Self>, text: QString, arg: QString) -> QString {
        let fmt = gettext(text);
        let result = fmt.replace("{}", &arg.to_string());

        QString::from(result)
    }
}
