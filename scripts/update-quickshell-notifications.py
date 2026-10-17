#!/usr/bin/env python3
"""Port Tera's notification centre without replacing laptop-specific controls.

Usage: update-quickshell-notifications.py --reference /path/to/astraea.files/Tera/home/.config/quickshell/shell.qml --target /path/to/laptop/shell.qml [--socket /path/to/bridge.sock]
"""
import argparse
from datetime import datetime
from pathlib import Path
import json
import shutil


def section(text, begin, end):
    start = text.index(begin)
    return text[start:text.index(end, start)]


def update(reference, current, socket):
    if 'id: notificationState' in current:
        raise ValueError('Target already has the new notification centre; refusing to apply twice')
    result = current.replace('import QtQuick.Layouts', 'import QtQuick.Effects\nimport Quickshell.Wayland\nimport QtQuick.Layouts', 1)
    result = result.replace('    property bool dnd: false', '''    property bool notificationsPinned: false
    readonly property int notificationPanelWidth: 360
    readonly property int hyprGap: 4
    readonly property int barWidth: 0
    readonly property int panelGap: 16
    property alias dnd: notificationState.dnd''', 1)
    properties = section(reference, '    // Arrival times in ms', '    property string focusedTitle:')
    result = result.replace('    property string notifications: ""', '    property string notifications: ""\n' + properties.rstrip(), 1)
    functions = section(reference, '    function notificationTimestamp', '    function closePanels')
    result = result.replace('    function closePanels()', functions + '    function closePanels()', 1)
    result = result.replace('notificationsOpen = false; mediaOpen', 'notificationsOpen = notificationsPinned; mediaOpen', 1)
    result = result.replace('|| notificationsOpen ||', '|| (notificationsOpen && !notificationsPinned) ||', 1)
    old_server = section(result, '    NotificationServer {', '    component EmberButton:')
    new_server = section(reference, '    ListModel { id: notificationPopups;', '    component EmberButton:')
    new_server = new_server.replace('property string times: "{}"', 'property string times: "{}"\n        property string epoch: ""\n        property string offlineClosures: "[]"')
    # Silent shared projections use the canonical timestamp and never replay a popup.
    new_server = new_server.replace('root.retainNotification(notification, Date.now());', 'root.retainNotification(notification, Number(notification.hints["x-edge-kvm-posted-ms"]) || Date.now());')
    new_server = new_server.replace('if (root.dnd) return;', 'if (root.dnd || notification.hints["x-edge-kvm-replay"]) return;')
    bridge = '''    EdgeNotificationBridge {
        server: notificationServer
        state: notificationState
        timestamps: root.notificationTimes
        onEntryClosed: id => {
            root.removeNotificationPopup(id);
            if (root.notificationTimes[id] === undefined) return;
            let times = Object.assign({}, root.notificationTimes);
            delete times[id];
            notificationState.times = JSON.stringify(times);
        }
'''
    if socket:
        bridge += '        socketPath: ' + json.dumps(str(socket)) + '\n'
    bridge += '    }\n\n'
    result = result.replace(old_server, new_server + bridge, 1)
    result = result.replace('target: "ember"', 'target: "ember"\n        function notificationStatus(): string { return JSON.stringify({ dnd: root.dnd, history: root.notificationHistory.length, pinned: root.notificationsPinned }); }', 1)
    result = result.replace('glyph: "󰂚"; label: notificationServer.trackedNotifications.values.length > 0 ? notificationServer.trackedNotifications.values.length : "";', 'glyph: root.dnd ? "󰂛" : "󰂚"; label: root.notificationHistory.length > 0 ? root.notificationHistory.length : "";', 1)
    result = result.replace('glyph: "󰒓"; label: "Do not disturb / notifications"; Layout.fillWidth: true; onPressed: root.run("swaync-client -d")', 'glyph: root.dnd ? "󰂛" : "󰂚"; label: root.dnd ? "Do not disturb: on" : "Do not disturb: off"; active: root.dnd; Layout.fillWidth: true; onPressed: root.dnd = !root.dnd', 1)
    start = reference.index('    PanelWindow {\n        // Unpinned,')
    panel = reference[start:reference.index('    PanelWindow {', start + 20)]
    panel = panel.replace('    PanelWindow {', '    PanelWindow {\n        id: notificationsPanel', 1).replace('screen: bar.screen', 'screen: root.panelScreen')
    panel = panel.replace('text: modelData.summary; color:', 'text: modelData.summary; textFormat: Text.PlainText; color:')
    old_panel_start = result.index('    PanelWindow {\n        id: notificationsPanel')
    old_panel_end = result.index('    PanelWindow {', old_panel_start + 20)
    result = result[:old_panel_start] + panel + result[old_panel_end:]
    overlay_start = reference.index('    PanelWindow {\n        id: notificationOverlay')
    overlay_end = reference.index('    PanelWindow {', overlay_start + 20)
    overlay = reference[overlay_start:overlay_end].replace('screen: bar.screen', 'screen: root.panelScreen')
    result = result.replace(panel, overlay + panel, 1)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--reference', required=True, type=Path)
    parser.add_argument('--target', required=True, type=Path)
    parser.add_argument('--socket', type=Path)
    args = parser.parse_args()
    original = args.target.read_text()
    updated = update(args.reference.read_text(), original, args.socket)
    bridge = Path(__file__).resolve().parents[1] / 'integrations/quickshell/EdgeNotificationBridge.qml'
    backup = args.target.with_name(args.target.name + '.before-notification-sync-' + datetime.now().strftime('%Y%m%d-%H%M%S'))
    shutil.copy2(args.target, backup)
    shutil.copy2(bridge, args.target.parent / bridge.name)
    # One replacement so file watchers see a complete configuration.
    temporary = args.target.with_suffix('.notification-update')
    temporary.write_text(updated)
    shutil.copymode(args.target, temporary)
    temporary.replace(args.target)
    print(f'Updated {args.target}; backup: {backup}')


if __name__ == '__main__':
    main()
