import QtQuick
import QtQml.Models
import Quickshell
import Quickshell.Io

// Attach to the existing NotificationServer; this component has no UI.
Item {
    id: bridge
    visible: false
    required property var server
    property var timestamps: ({})
    property string socketPath: Quickshell.env("EDGE_KVM_NOTIFICATION_SOCKET") || ""
    property bool sendingInventory: false
    property bool initialized: false
    property string localIdentity: ""
    property var sourceNames: ({})
    signal entryClosed(int id)

    // Keep this PersistentProperties object at the ShellRoot level so its
    // reload identity is stable even when this component is recreated.
    required property var state
    // NotificationServer publishes its live model before synchronously restoring
    // entries. Deferring this signal waits until restoration has completed.
    Connections {
        target: bridge.server
        function onTrackedNotificationsChanged() { Qt.callLater(bridge.initialize); }
    }
    function initialize() {
        if (!state.epoch) state.epoch = Date.now().toString(36) + "-" + Math.random().toString(36).slice(2);
        initialized = true;
        if (socketPath.length > 0) socket.connected = true;
    }
    function send(message) {
        if (!initialized || !socket.connected) return;
        socket.write(JSON.stringify(message) + "\n");
        socket.flush();
    }
    function text(value, limit) { return Array.from(value || "").slice(0, limit).join(""); }
    function content(n) {
        const hinted = n.hints["x-edge-kvm-posted-ms"];
        let icon = n.appIcon || n.desktopEntry || "";
        if (!/^[A-Za-z0-9_.-]{0,128}$/.test(icon)) icon = "";
        return { app_name: text(n.appName, 64), title: text(n.summary, 256),
            body: text(n.body, 1024), icon: icon,
            posted_ms: Number(hinted || timestamps[n.id] || Date.now()),
            urgency: Math.max(0, Math.min(2, Number(n.urgency))) };
    }
    function copyKey(n) {
        try { return JSON.parse(n.hints["x-edge-kvm-key"] || "null"); }
        catch (_) { return null; }
    }
    function appLabel(n) {
        const app = n.appName || "Notification";
        const key = copyKey(n);
        if (n.hints["x-edge-kvm-phone"] || (key && typeof key.instance === "string" && key.instance.startsWith("phone/"))) return app;
        const origin = key ? key.origin : localIdentity;
        const name = sourceNames[origin];
        return name ? app + " (" + name + ")" : app;
    }
    function post(n) {
        if (!n || n.transient || n.hints["x-edge-kvm-phone"]) return;
        const key = copyKey(n);
        if (key) send({ type: "copy", id: n.id, key: key, content: content(n) });
        else send({ type: "posted", id: n.id, instance: state.epoch + "/" + n.id, content: content(n) });
    }
    function closed(n, reason) {
        if (!n) return;
        entryClosed(n.id);
        if (!socketPath.length || n.transient || n.hints["x-edge-kvm-phone"]) return;
        const key = copyKey(n);
        // Copies only forward explicit user dismissals, never programmatic closes.
        if (key && Number(reason) !== 2) return;
        const message = { type: "closed", id: n.id, reason: Number(reason),
            instance: state.epoch + "/" + n.id, key: key };
        if (!socket.connected) {
            let queue = JSON.parse(state.offlineClosures);
            if (queue.length >= 1024) { console.warn("edge-kvm offline closure queue full"); return; }
            queue.push(message);
            state.offlineClosures = JSON.stringify(queue);
        } else send(message);
    }
    function inventory() {
        sendingInventory = true;
        send({ type: "start" });
        for (const n of server.trackedNotifications.values) post(n);
        for (const message of JSON.parse(state.offlineClosures)) send(message);
        send({ type: "end" });
    }
    Socket {
        id: socket
        path: bridge.socketPath
        connected: false
        onConnectedChanged: {
            if (connected) { retry.interval = 1000; bridge.inventory(); }
            else if (bridge.socketPath.length > 0) retry.restart();
        }
        onError: if (bridge.socketPath.length > 0) retry.restart()
        parser: SplitParser {
            onRead: data => {
                let message;
                try { message = JSON.parse(data); } catch (_) { return; }
                if (message.type === "sources") {
                    bridge.localIdentity = message.identity;
                    bridge.sourceNames = message.names;
                }
                if (message.type === "retire_phone" && typeof message.name === "string") {
                    // Dedicated legacy marker plus the paired phone's app suffix,
                    // never title/body matching. Shared projections lack this marker.
                    const suffix = " (" + message.name + ")";
                    for (const n of [...bridge.server.trackedNotifications.values]) {
                        if (n.hints["x-edge-kvm-phone"] && n.appName.endsWith(suffix)) n.expire();
                    }
                }
                if (bridge.sendingInventory && data === '{"type":"ready"}') {
                    state.offlineClosures = "[]";
                    bridge.sendingInventory = false;
                }
            }
        }
    }
    Timer {
        id: retry
        interval: 1000
        onTriggered: {
            socket.connected = true;
            interval = Math.min(interval * 2, 30000);
        }
    }
    Instantiator {
        model: bridge.server.trackedNotifications
        delegate: QtObject {
            required property var modelData
            Component.onCompleted: bridge.post(modelData)
            property Connections observer: Connections {
                target: modelData
                function onClosed(reason) { bridge.closed(modelData, reason); }
                function onSummaryChanged() { bridge.post(modelData); }
                function onBodyChanged() { bridge.post(modelData); }
                function onAppNameChanged() { bridge.post(modelData); }
                function onAppIconChanged() { bridge.post(modelData); }
                function onHintsChanged() { bridge.post(modelData); }
            }
        }
    }
}
