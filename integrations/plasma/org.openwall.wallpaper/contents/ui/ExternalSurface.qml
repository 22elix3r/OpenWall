// SPDX-License-Identifier: AGPL-3.0-or-later
import QtQuick
import QtQuick.Window
import QtWayland.Compositor
import QtWayland.Compositor.XdgShell

Item {
    id: root
    required property string outputName
    readonly property string instanceToken: Date.now().toString(36) + Math.random().toString(36).slice(2, 10)
    // A new socket per instance also permits overlapping wallpaper transitions.
    // The hash bounds connector names; it is an identifier, not authentication.
    readonly property string socketName: "openwall-plasma-" + Qt.md5(outputName) + "-" + instanceToken
    readonly property bool hasFrame: view.surface !== null && view.surface.hasContent
    property XdgToplevel currentToplevel: null
    clip: true

    function configureClient() {
        if (currentToplevel && width > 0 && height > 0) {
            currentToplevel.sendFullscreen(Qt.size(Math.round(width), Math.round(height)))
        }
    }

    onWidthChanged: Qt.callLater(configureClient)
    onHeightChanged: Qt.callLater(configureClient)

    WaylandCompositor {
        id: server
        socketName: root.socketName
        retainedSelection: false

        WaylandOutput {
            id: output
            window: root.Window.window
            sizeFollowsWindow: true
        }

        XdgDecorationManagerV1 {
            preferredMode: XdgToplevel.ServerSideDecoration
        }

        XdgShell {
            onToplevelCreated: (toplevel, xdgSurface) => {
                if (view.shellSurface !== null) {
                    // Qt continues initializing this role after emitting the signal.
                    // Disconnecting synchronously would free objects still on its stack.
                    Qt.callLater(function() {
                        if (xdgSurface && xdgSurface.surface) {
                            server.destroyClientForSurface(xdgSurface.surface)
                        }
                    })
                    return
                }
                view.shellSurface = xdgSurface
                root.currentToplevel = toplevel
                root.configureClient()
            }
            onPopupCreated: (popup, xdgSurface) => popup.sendPopupDone()
        }
    }

    ShellSurfaceItem {
        id: view
        anchors.fill: parent
        output: output
        enabled: false
        focusOnClick: false
        inputEventsEnabled: false
        touchEventsEnabled: false
        autoCreatePopupItems: false
        onSurfaceDestroyed: {
            shellSurface = null
            root.currentToplevel = null
        }
    }
}
