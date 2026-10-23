// SPDX-License-Identifier: AGPL-3.0-or-later
import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import QtQuick.Dialogs
import org.openwall

ApplicationWindow {
    id: window
    width: 1220; height: 780
    minimumWidth: 940; minimumHeight: 640
    visible: true
    title: "OpenWall"
    color: "#0e111a"
    palette.window: "#0e111a"
    palette.windowText: "#e9ecf5"
    palette.button: "#242c40"
    palette.buttonText: "#e9ecf5"
    palette.highlight: "#9bded1"
    palette.highlightedText: "#102822"
    font.family: Qt.platform.os === "osx" ? ".AppleSystemUIFont" : "Sans Serif"
    font.pixelSize: 14

    Backend { id: backend }
    Component.onCompleted: backend.discover_outputs()
    Timer { interval: 1000; running: true; repeat: true; onTriggered: backend.refresh() }
    Timer { interval: 1500; running: Qt.application.arguments.indexOf("--check") !== -1; onTriggered: Qt.quit() }
    FileDialog {
        id: picker
        title: "Choose a wallpaper"
        nameFilters: ["Wallpapers (*.json *.png *.jpg *.jpeg *.webp *.tex *.mp4 *.webm *.mkv *.mov)"]
        onAccepted: backend.choose(selectedFile.toString())
    }
    Shortcut { sequence: StandardKey.Open; onActivated: picker.open() }
    Shortcut { sequence: StandardKey.Quit; onActivated: window.close() }

    RowLayout {
        anchors.fill: parent; spacing: 0
        Rectangle {
            Layout.preferredWidth: 208; Layout.fillHeight: true; color: "#141824"
            ColumnLayout {
                anchors.fill: parent; anchors.margins: 24; spacing: 28
                RowLayout {
                    spacing: 11
                    Rectangle {
                        width: 31; height: 31; radius: 9; color: "#9bded1"
                        Label { anchors.centerIn: parent; text: "◒"; font.pixelSize: 23; color: "#123930" }
                    }
                    Label { text: "OpenWall"; font.pixelSize: 21; font.weight: Font.DemiBold; color: "#f2f4fb" }
                }
                Label { text: "YOUR SPACE"; color: "#7b859e"; font.pixelSize: 10; font.letterSpacing: 2; Layout.topMargin: 26 }
                Rectangle {
                    Layout.fillWidth: true; implicitHeight: 43; radius: 9; color: "#263440"
                    Label { anchors.verticalCenter: parent.verticalCenter; anchors.left: parent.left; anchors.leftMargin: 14; text: "▧   Library"; color: "#b9e7dd"; font.weight: Font.DemiBold }
                }
                Item { Layout.fillHeight: true }
                Rectangle { Layout.fillWidth: true; height: 1; color: "#293044" }
                Label { text: "Your desktop.\nYour imagination."; color: "#8f9bb5"; lineHeight: 1.6; font.pixelSize: 13 }
                Label { text: "Free. Open. Yours."; color: "#65718b"; font.pixelSize: 11 }
            }
        }
        ColumnLayout {
            Layout.fillWidth: true; Layout.fillHeight: true; Layout.margins: 36; spacing: 24
            RowLayout {
                ColumnLayout {
                    spacing: 6
                    Label { text: "Wallpaper library"; color: "#f0f2fa"; font.pixelSize: 29; font.weight: Font.DemiBold }
                    Label { text: "Make room for something beautiful."; color: "#8f9bb5"; font.pixelSize: 14 }
                }
                Item { Layout.fillWidth: true }
                Button { text: "+  Open wallpaper"; enabled: !backend.busy; onClicked: picker.open(); Accessible.name: "Open a local wallpaper" }
            }
            Rectangle {
                Layout.fillWidth: true; Layout.fillHeight: true; radius: 16; color: "#171d2b"; border.color: "#293248"
                ColumnLayout {
                    anchors.fill: parent; anchors.margins: 1; spacing: 0
                    Item {
                        Layout.fillWidth: true; Layout.fillHeight: true
                        Image { anchors.fill: parent; source: backend.previewUrl; fillMode: Image.PreserveAspectCrop; asynchronous: true; clip: true }
                        Rectangle {
                            anchors.fill: parent
                            gradient: Gradient { GradientStop { position: 0.3; color: "transparent" } GradientStop { position: 1; color: "#e6171d2b" } }
                        }
                        Rectangle {
                            anchors.top: parent.top; anchors.left: parent.left; anchors.margins: 20
                            width: label.implicitWidth + 22; height: 28; radius: 14; color: "#b314242b"
                            Label { id: label; anchors.centerIn: parent; text: backend.active ? "●  ON YOUR DESKTOP" : "LOCAL WALLPAPER"; color: "#bceadd"; font.pixelSize: 10; font.letterSpacing: 1.2 }
                        }
                        ColumnLayout {
                            anchors.left: parent.left; anchors.right: parent.right; anchors.bottom: parent.bottom; anchors.margins: 28; spacing: 8
                            Label { text: backend.title; color: "#ffffff"; font.pixelSize: 32; font.weight: Font.DemiBold; Layout.fillWidth: true; elide: Text.ElideRight }
                            Label { text: backend.details; color: "#acb8cb"; font.pixelSize: 12 }
                        }
                    }
                    RowLayout {
                        Layout.fillWidth: true; Layout.margins: 23; spacing: 14
                        ColumnLayout {
                            spacing: 6
                            Label { text: "DISPLAY"; color: "#8b98b2"; font.pixelSize: 10; font.letterSpacing: 1.4 }
                            ComboBox {
                                id: displays
                                model: JSON.parse(backend.outputsJson)
                                textRole: "label"; valueRole: "selector"
                                Layout.preferredWidth: 260
                                hoverEnabled: true
                                ToolTip.visible: hovered
                                ToolTip.text: currentText
                                Accessible.name: "Display to use for wallpaper"
                            }
                        }
                        Item { Layout.fillWidth: true }
                        Button { text: "Preview"; enabled: !backend.busy; onClicked: backend.start(false, ""); Accessible.name: "Open live wallpaper preview" }
                        Button { text: backend.active ? "Stop wallpaper" : "Set wallpaper"; highlighted: !backend.active; enabled: !backend.busy; onClicked: backend.active ? backend.stop() : backend.start(true, displays.currentValue || "") }
                    }
                }
            }
            RowLayout {
                Layout.fillWidth: true; spacing: 12
                Rectangle { width: 7; height: 7; radius: 4; color: backend.active ? "#9bded1" : "#64718c" }
                Label { text: backend.status; color: "#9ca9c1"; font.pixelSize: 12; wrapMode: Text.Wrap; Layout.fillWidth: true; maximumLineCount: 3; elide: Text.ElideRight; Accessible.role: Accessible.StaticText }
                BusyIndicator { running: backend.busy; visible: running; Layout.preferredWidth: 24; Layout.preferredHeight: 24 }
                Button { text: "Reset to sample"; flat: true; enabled: !backend.busy; onClicked: backend.sample() }
            }
        }
    }
}
