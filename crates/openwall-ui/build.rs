// SPDX-License-Identifier: AGPL-3.0-or-later
use cxx_qt_build::{CxxQtBuilder, QmlModule};

fn main() {
    // qt-build-utils 0.8.1 runs qmlcachegen before regenerating plugin.qmltypes.
    // Old method indices can crash Qt after a Rust QObject interface changes.
    // Remove only this module's previous type information in our own OUT_DIR;
    // the builder regenerates/exports it afterward. This preserves fresh-build
    // behavior instead of allowing AOT calls compiled against stale metadata.
    let types = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo OUT_DIR"))
        .join("qt-build-utils/qml_modules/org/openwall/plugin.qmltypes");
    if let Err(error) = std::fs::remove_file(&types) {
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::NotFound,
            "cannot remove stale OpenWall QML type information: {error}"
        );
    }
    CxxQtBuilder::new_qml_module(QmlModule::new("org.openwall").qml_file("qml/Main.qml"))
        .qt_module("Network")
        .qt_module("Quick")
        .qt_module("QuickControls2")
        .qrc("resources.qrc")
        .file("src/backend.rs")
        .build();
}
