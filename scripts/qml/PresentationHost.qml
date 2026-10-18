// SPDX-License-Identifier: AGPL-3.0-or-later
import QtQuick
import QtQuick.Window
import "../../integrations/plasma/org.openwall.wallpaper/contents/ui" as OpenWall

Window {
    visible: true
    width: 640
    height: 360
    // Request an alpha channel rather than a low-precision RGB565 EGL format.
    color: "transparent"
    title: "OpenWall presentation bridge test"

    Rectangle {
        anchors.fill: parent
        color: "#17233d"
    }

    OpenWall.ExternalSurface {
        anchors.fill: parent
        outputName: "test-output"
    }
}
