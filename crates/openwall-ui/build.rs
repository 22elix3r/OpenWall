// SPDX-License-Identifier: AGPL-3.0-or-later
use cxx_qt_build::{CxxQtBuilder, QmlModule};

fn main() {
    CxxQtBuilder::new_qml_module(QmlModule::new("org.openwall").qml_file("qml/Main.qml"))
        .qt_module("Network")
        .qt_module("Quick")
        .qt_module("QuickControls2")
        .qrc("resources.qrc")
        .file("src/backend.rs")
        .build();
}
