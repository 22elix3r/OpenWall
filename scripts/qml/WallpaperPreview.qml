// SPDX-License-Identifier: AGPL-3.0-or-later
import QtQuick
import QtQuick.Window

Window {
    visible: true
    width: 320
    height: 180
    title: "OpenWall wallpaper preview guard test"

    Loader {
        anchors.fill: parent
        source: "../../integrations/plasma/org.openwall.wallpaper/contents/ui/main.qml"
        onLoaded: {
            if (item.desktopHost) {
                console.error("Preview incorrectly enabled desktop hosting")
                Qt.exit(1)
            }
            console.info("OpenWall preview guard passed")
        }
        onStatusChanged: {
            if (status === Loader.Error) Qt.exit(1)
        }
    }
}
