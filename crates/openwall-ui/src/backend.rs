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
            outputs_json: QString::from("[\"All displays\"]"),
            busy: false,
            active: false,
            child: None,
            previews: Vec::new(),
            generation: 0,
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

impl ffi::Backend {
    fn message(mut self: Pin<&mut Self>, message: impl AsRef<str>) {
        self.as_mut().set_status(QString::from(message.as_ref()));
    }

    pub fn sample(mut self: Pin<&mut Self>) {
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
        self.as_mut().rust_mut().generation += 1;
        let generation = self.rust().generation;
        self.as_mut().set_busy(true);
        self.as_mut().message("Reading wallpaper…");
        let qt = self.qt_thread();
        std::thread::spawn(move || {
            let result = Command::new(player()).arg("inspect").arg(&path).output();
            let _ = qt.queue(move |mut object| {
                if object.rust().generation != generation {
                    return;
                }
                object.as_mut().set_busy(false);
                match result {
                    Ok(output) if output.status.success() => {
                        let report: serde_json::Value =
                            serde_json::from_slice(&output.stdout).unwrap_or_default();
                        object
                            .as_mut()
                            .set_selected(QString::from(path.to_string_lossy().as_ref()));
                        object.as_mut().set_title(QString::from(
                            report["title"].as_str().unwrap_or("Local wallpaper"),
                        ));
                        let details = if report["kind"] == "video" {
                            "Native video playback".to_owned()
                        } else {
                            format!(
                                "{} × {}  ·  {} layers",
                                report["width"], report["height"], report["layers"]
                            )
                        };
                        object.as_mut().set_details(QString::from(details.as_str()));
                        let preview = if path.extension().is_some_and(|x| {
                            matches!(x.to_str(), Some("png" | "jpg" | "jpeg" | "webp"))
                        }) {
                            Some(path.clone())
                        } else {
                            path.parent()
                                .map(|x| x.join("preview.png"))
                                .filter(|x| x.exists())
                        };
                        let preview = preview
                            .and_then(|x| url::Url::from_file_path(x).ok())
                            .map(|x| x.to_string())
                            .unwrap_or_default();
                        object
                            .as_mut()
                            .set_preview_url(QString::from(preview.as_str()));
                        let unsupported = report["unsupported"]
                            .as_array()
                            .is_some_and(|x| !x.is_empty());
                        object.message(if unsupported {
                            "This wallpaper uses features that are not supported yet."
                        } else {
                            "Ready"
                        });
                    }
                    Ok(output) => object.message(String::from_utf8_lossy(&output.stderr)),
                    Err(_) => object.message(
                        "The wallpaper player is missing. Install the complete OpenWall package.",
                    ),
                }
            });
        });
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
            .and_then(|child| child.try_wait().ok().flatten());
        if let Some(status) = ended {
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
        self.as_mut()
            .rust_mut()
            .previews
            .retain_mut(|child| matches!(child.try_wait(), Ok(None)));
    }

    pub fn discover_outputs(self: Pin<&mut Self>) {
        let qt = self.qt_thread();
        std::thread::spawn(move || {
            let mut outputs = vec!["All displays".to_owned()];
            if let Ok(result) = Command::new(player()).arg("outputs").output()
                && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&result.stdout)
                && let Some(displays) = value["outputs"].as_array()
            {
                outputs.extend(
                    displays
                        .iter()
                        .filter_map(|x| x["name"].as_str().map(str::to_owned)),
                );
            }
            let json = serde_json::to_string(&outputs).unwrap_or_else(|_| "[]".into());
            let _ = qt.queue(move |mut object| {
                object
                    .as_mut()
                    .set_outputs_json(QString::from(json.as_str()))
            });
        });
    }
}
