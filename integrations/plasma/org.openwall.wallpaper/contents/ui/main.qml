// SPDX-License-Identifier: AGPL-3.0-or-later
import QtQuick
import QtQuick.Window
import org.kde.plasma.plasmoid

WallpaperItem {
    id: root

    // DesktopView's context properties are hidden from wallpaper packages in
    // Plasma 6. Its window properties distinguish it from configuration previews.
    // Fail closed if a future shell no longer exposes them.
    readonly property bool desktopHost: root.Window.window !== null
        && typeof root.Window.window.screenToFollow !== "undefined"
        && typeof root.Window.window.candidateContainments !== "undefined"
    readonly property string outputName: desktopHost && root.Window.window.screenToFollow
        ? root.Window.window.screenToFollow.name : ""

    function reloadBridge() {
        bridge.active = false
        if (desktopHost && outputName.length > 0) {
            bridge.setSource(Qt.resolvedUrl("ExternalSurface.qml"), { outputName: outputName })
            bridge.active = true
        }
    }

    onDesktopHostChanged: Qt.callLater(reloadBridge)
    onOutputNameChanged: Qt.callLater(reloadBridge)
    Component.onCompleted: Qt.callLater(reloadBridge)

    Rectangle {
        anchors.fill: parent
        gradient: Gradient {
            GradientStop { position: 0; color: "#17233d" }
            GradientStop { position: 1; color: "#0a1724" }
        }
    }

    Loader {
        id: bridge
        anchors.fill: parent
        active: false
    }
}
