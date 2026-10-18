// SPDX-License-Identifier: AGPL-3.0-or-later
import QtQuick
import org.kde.plasma.plasmoid

PlasmoidItem {
    width: 96
    height: 96
    fullRepresentation: Rectangle {
        color: clicked ? "#25d6b8" : "#be29e9"
        property bool clicked: false
        MouseArea {
            anchors.fill: parent
            onClicked: parent.clicked = true
        }
    }
}
