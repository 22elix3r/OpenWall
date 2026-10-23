// SPDX-License-Identifier: AGPL-3.0-or-later
use crate::worker::Worker;
use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::QString;
use std::{path::PathBuf, pin::Pin, process::Command};

#[cxx_qt::bridge]
mod ffi {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    extern "RustQt" {
        #[qobject]
        #[qml_element]
        #[qproperty(QString, title)]
        #[qproperty(QString, selected)]
        #[qproperty(QString, status)]
        #[qproperty(QString, details)]
        #[qproperty(QString, preview_url, cxx_name = "previewUrl")]
        #[qproperty(QString, outputs_json, cxx_name = "outputsJson")]
        #[qproperty(bool, busy)]
        #[qproperty(bool, active)]
        type Backend = super::BackendRust;

        #[qinvokable]
        fn choose(self: Pin<&mut Backend>, uri: &QString);
        #[qinvokable]
        fn sample(self: Pin<&mut Backend>);
        #[qinvokable]
        fn start(self: Pin<&mut Backend>, desktop: bool, output: &QString);
        #[qinvokable]
        fn stop(self: Pin<&mut Backend>);
        #[qinvokable]
        fn refresh(self: Pin<&mut Backend>);
        #[qinvokable]
        fn discover_outputs(self: Pin<&mut Backend>);
    }
    impl cxx_qt::Threading for Backend {}
}

pub struct BackendRust {
    title: QString,
    selected: QString,
    status: QString,
    details: QString,
    preview_url: QString,
    outputs_json: QString,
    busy: bool,
    active: bool,
    child: Option<Worker>,
    previews: Vec<Worker>,
    generation: u64,
    inspection: Option<crate::query::Cancellation>,
    thumbnail: Option<crate::thumbnail::Thumbnail>,
}

impl Default for BackendRust {
    fn default() -> Self {
        Self {
            title: QString::from("Aurora Harbor"),
            selected: QString::from(
                openwall_core::scene::sample_path()
                    .to_string_lossy()
                    .as_ref(),
            ),
            status: QString::from("Ready"),
            details: QString::from("1280 × 720  ·  Original scene  ·  CC0"),
            preview_url: QString::from("qrc:/openwall/aurora.png"),
            outputs_json: QString::from(crate::outputs::choices(None)),
            busy: false,
            active: false,
            child: None,
            previews: Vec::new(),
            generation: 0,
            inspection: None,
            thumbnail: None,
        }
    }
}

fn player() -> PathBuf {
    std::env::var_os("OPENWALL_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap_or_default()
                .with_file_name("openwall")
        })
}

impl BackendRust {
    fn cancel_inspection(&mut self) {
        if let Some(cancellation) = self.inspection.take() {
            cancellation.cancel();
        }
    }
}

impl Drop for BackendRust {
    fn drop(&mut self) {
        self.cancel_inspection();
    }
}

impl ffi::Backend {
    fn message(mut self: Pin<&mut Self>, message: impl AsRef<str>) {
        self.as_mut().set_status(QString::from(message.as_ref()));
    }

    pub fn sample(mut self: Pin<&mut Self>) {
        self.as_mut().rust_mut().cancel_inspection();
        self.as_mut().rust_mut().generation += 1;
        self.as_mut().set_busy(false);
        self.as_mut().set_title(QString::from("Aurora Harbor"));
        self.as_mut().set_selected(QString::from(
            openwall_core::scene::sample_path()
                .to_string_lossy()
                .as_ref(),
        ));
        self.as_mut()
            .set_details(QString::from("1280 × 720  ·  Original scene  ·  CC0"));
        self.as_mut()
            .set_preview_url(QString::from("qrc:/openwall/aurora.png"));
        self.as_mut().rust_mut().thumbnail = None;
        self.message("Ready");
    }

    pub fn choose(mut self: Pin<&mut Self>, uri: &QString) {
        let raw = uri.to_string();
        let path = match url::Url::parse(&raw)
            .ok()
            .and_then(|url| url.to_file_path().ok())
        {
            Some(path) => path,
            None => {
                self.message("Choose a local file.");
                return;
            }
        };
        self.as_mut().rust_mut().cancel_inspection();
        self.as_mut().rust_mut().generation += 1;
        let generation = self.rust().generation;
        self.as_mut().set_busy(true);
        self.as_mut().message("Reading wallpaper…");
        let cancellation = crate::query::Cancellation::default();
        self.as_mut().rust_mut().inspection = Some(cancellation.clone());
        let qt = self.qt_thread();
        let launched = crate::query::spawn(move || {
            let result = crate::query::run_cancellable(
                Command::new(player()).arg("inspect").arg(&path),
                std::time::Duration::from_secs(10),
                &cancellation,
            )
            .map_err(|error| format!("Could not read wallpaper: {error}"))
            .and_then(|output| {
                if !output.status.success() {
                    let error = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                    return Err(if error.is_empty() {
                        "The player could not inspect this wallpaper.".into()
                    } else {
                        error
                    });
                }
                openwall_core::inspection::Inspection::from_json(&output.stdout)
                    .map(|inspection| inspection.report)
                    .map_err(|error| format!("Could not read wallpaper: {error}"))
            });
            let thumbnail = result
                .as_ref()
                .ok()
                .and_then(|_| crate::thumbnail::load(&player(), &path, &cancellation));
            if crate::query::is_closing() || cancellation.is_cancelled() {
                return;
            }
            let _ = qt.queue(move |mut object| {
                if object.rust().generation != generation {
                    return;
                }
                object.as_mut().rust_mut().inspection = None;
                object.as_mut().set_busy(false);
                match result {
                    Ok(report) => {
                        object
                            .as_mut()
                            .set_selected(QString::from(path.to_string_lossy().as_ref()));
                        object
                            .as_mut()
                            .set_title(QString::from(if report.title.is_empty() {
                                "Local wallpaper"
                            } else {
                                report.title.as_str()
                            }));
                        let details = if report.kind == "video" {
                            "Native video playback".to_owned()
                        } else {
                            format!(
                                "{} × {}  ·  {} layers",
                                report.width, report.height, report.layers
                            )
                        };
                        object.as_mut().set_details(QString::from(details.as_str()));
                        let preview = thumbnail
                            .as_ref()
                            .map(|thumbnail| thumbnail.url.as_str())
                            .unwrap_or_default();
                        object.as_mut().set_preview_url(QString::from(preview));
                        object.as_mut().rust_mut().thumbnail = thumbnail;
                        let unsupported = !report.unsupported.is_empty();
                        object.message(if unsupported {
                            "This wallpaper uses features that are not supported yet."
                        } else {
                            "Ready"
                        });
                    }
                    Err(error) => object.message(error),
                }
            });
        });
        if let Err(error) = launched {
            self.as_mut().rust_mut().cancel_inspection();
            self.as_mut().set_busy(false);
            self.message(format!("Could not read wallpaper: {error}"));
        }
    }

    pub fn start(mut self: Pin<&mut Self>, desktop: bool, output: &QString) {
        if *self.busy() {
            return;
        }
        let mut command = Command::new(player());
        command
            .arg(if desktop { "apply" } else { "preview" })
            .arg(self.selected().to_string());
        if desktop && !output.to_string().is_empty() {
            command.arg("--output").arg(output.to_string());
        }
        match Worker::spawn(&mut command) {
            Ok(child) => {
                if desktop {
                    self.as_mut().rust_mut().child.take();
                    self.as_mut().rust_mut().child = Some(child);
                    self.as_mut().set_active(true);
                    self.message("Wallpaper running");
                } else {
                    self.as_mut().rust_mut().previews.push(child);
                    self.message("Preview opened");
                }
            }
            Err(error) => self.message(format!("Could not open the wallpaper player: {error}")),
        }
    }

    pub fn stop(mut self: Pin<&mut Self>) {
        self.as_mut().rust_mut().child.take();
        self.as_mut().set_active(false);
        self.message("Wallpaper stopped");
    }

    pub fn refresh(mut self: Pin<&mut Self>) {
        let ended = self
            .as_mut()
            .rust_mut()
            .child
            .as_mut()
            .map(Worker::try_wait);
        if let Some(Err(error)) = &ended {
            self.as_mut().rust_mut().child.take();
            self.as_mut().set_active(false);
            self.as_mut()
                .message(format!("Could not observe wallpaper player: {error}"));
        }
        if let Some(Ok(Some(status))) = ended {
            let child = self.as_mut().rust_mut().child.take();
            self.as_mut().set_active(false);
            if !status.success() {
                let message = child
                    .map(|child| child.diagnostic())
                    .filter(|x| !x.trim().is_empty())
                    .unwrap_or_else(|| "The wallpaper player stopped unexpectedly.".into());
                self.as_mut().message(message);
            } else {
                self.as_mut().message("Wallpaper stopped");
            }
        }
        let mut preview_error = None;
        self.as_mut()
            .rust_mut()
            .previews
            .retain_mut(|child| match child.try_wait() {
                Ok(None) => true,
                Ok(Some(status)) => {
                    if !status.success() {
                        preview_error = Some(child.diagnostic());
                    }
                    false
                }
                Err(error) => {
                    preview_error = Some(error.to_string());
                    false
                }
            });
        if let Some(error) = preview_error {
            self.message(if error.is_empty() {
                "Preview could not be opened.".to_owned()
            } else {
                error
            });
        }
    }

    pub fn discover_outputs(self: Pin<&mut Self>) {
        let qt = self.qt_thread();
        let launched = crate::query::spawn(move || {
            let discovery = crate::query::run(
                Command::new(player()).arg("outputs"),
                std::time::Duration::from_secs(5),
            )
            .ok()
            .filter(|result| result.status.success())
            .and_then(|result| serde_json::from_slice::<serde_json::Value>(&result.stdout).ok());
            let json = crate::outputs::choices(discovery.as_ref());
            if crate::query::is_closing() {
                return;
            }
            let _ = qt.queue(move |mut object| {
                object
                    .as_mut()
                    .set_outputs_json(QString::from(json.as_str()))
            });
        });
        if let Err(error) = launched {
            self.message(format!("Could not list displays: {error}"));
        }
    }
}
