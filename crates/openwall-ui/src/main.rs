// SPDX-License-Identifier: AGPL-3.0-or-later
mod backend;
mod worker;
use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QUrl};

fn main() {
    let mut app = QGuiApplication::new();
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        engine
            .as_mut()
            .on_object_creation_failed(|_, url| {
                eprintln!("OpenWall could not load its interface: {url:?}");
                std::process::exit(1);
            })
            .release();
        engine.load(&QUrl::from("qrc:/qt/qml/org/openwall/qml/Main.qml"));
    }
    if let Some(app) = app.as_mut() {
        app.exec();
    }
}
