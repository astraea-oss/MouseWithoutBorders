import QtQuick
import Quickshell
import Quickshell.Io
import Quickshell.Services.Notifications

// Headless fixture: real Quickshell notification server, no desktop windows.
ShellRoot {
    id: root
    PersistentProperties {
        id: bridgeState
        reloadableId: "notificationIntegrationState"
        property string epoch: ""
        property string offlineClosures: "[]"
    }
    NotificationServer {
        id: notifications
        persistenceSupported: true
        onNotification: n => { n.tracked = true; }
    }
    EdgeNotificationBridge { server: notifications; state: bridgeState }
    IpcHandler {
        target: "test"
        function entries(): string {
            return JSON.stringify(notifications.trackedNotifications.values.map(n => ({ id: n.id, title: n.summary, key: n.hints["x-edge-kvm-key"] || "" })));
        }
        function dismiss(id: int): void {
            const entry = notifications.trackedNotifications.values.find(n => n.id === id);
            if (entry) entry.dismiss();
        }
    }
}
