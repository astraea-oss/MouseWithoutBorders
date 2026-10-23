import QtQuick
import Quickshell
import Quickshell.Io
import Quickshell.Services.Notifications

// The reviewed centre's lifecycle, without desktop windows or popups.
ShellRoot {
    id: root
    property bool dnd: false
    readonly property var notificationTimes: JSON.parse(notificationState.times)
    PersistentProperties {
        id: notificationState
        reloadableId: "notificationIntegrationState"
        property string times: "{}"
        property string epoch: ""
        property string offlineClosures: "[]"
    }
    function retainNotification(notification, time) { notification.tracked = true; }
    function releaseTransients() {}
    function removeNotificationPopup(id) {}
    NotificationServer {
        id: notificationServer
        persistenceSupported: true
        onNotification: notification => {
            if (notification.lastGeneration) { root.retainNotification(notification); return; }
            if (notification.transient) notification.tracked = true;
            else root.retainNotification(notification, Number(notification.hints["x-edge-kvm-posted-ms"]) || Date.now());
            if (root.dnd || notification.hints["x-edge-kvm-replay"]) return;
        }
    }
    EdgeNotificationBridge {
        id: edgeNotificationBridge
        server: notificationServer
        state: notificationState
        timestamps: root.notificationTimes
    }
    component EmberButton: Item {}
    IpcHandler {
        target: "test"
        function entries(): string {
            return JSON.stringify(notificationServer.trackedNotifications.values.map(n => ({ id: n.id, title: n.summary, label: typeof edgeNotificationBridge === "undefined" ? n.appName : edgeNotificationBridge.appLabel(n), key: n.hints["x-edge-kvm-key"] || "" })));
        }
        function dismiss(id: int): void {
            const entry = notificationServer.trackedNotifications.values.find(n => n.id === id);
            if (entry) entry.dismiss();
        }
    }
}
