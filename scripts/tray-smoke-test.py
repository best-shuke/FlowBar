#!/usr/bin/env python3
"""Drive the Linux tray icon over D-Bus as a desktop panel would, without TUN.

Acts as the StatusNotifierWatcher and host, reads the item's icon and tooltip, then clicks
menu items through com.canonical.dbusmenu: connect, switch mode, disconnect and quit.
Needs a display and a fresh D-Bus session, for example:
    xvfb-run -a dbus-run-session -- python3 scripts/tray-smoke-test.py
"""
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import time

import gi

gi.require_version('Gio', '2.0')
from gi.repository import Gio, GLib  # noqa: E402

WATCHER = '''<node><interface name="org.kde.StatusNotifierWatcher">
  <method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
  <method name="RegisterStatusNotifierHost"><arg type="s" direction="in"/></method>
  <property name="RegisteredStatusNotifierItems" type="as" access="read"/>
  <property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
  <property name="ProtocolVersion" type="i" access="read"/>
  <signal name="StatusNotifierItemRegistered"><arg type="s"/></signal>
</interface></node>'''
ROOT = Path(__file__).resolve().parent.parent
bus = Gio.bus_get_sync(Gio.BusType.SESSION)
items = []


def watcher_call(connection, sender, path, interface, method, parameters, invocation):
    if method == 'RegisterStatusNotifierItem':
        service = parameters.unpack()[0]
        # An item may register by object path; it is then served from the sender's name.
        items.append((sender, service) if service.startswith('/') else (service, '/StatusNotifierItem'))
    invocation.return_value(None)


def watcher_property(connection, sender, path, interface, name):
    return {
        'RegisteredStatusNotifierItems': GLib.Variant('as', [f'{s}{p}' for s, p in items]),
        'IsStatusNotifierHostRegistered': GLib.Variant('b', True),
        'ProtocolVersion': GLib.Variant('i', 0),
    }[name]


bus.register_object('/StatusNotifierWatcher', Gio.DBusNodeInfo.new_for_xml(WATCHER).interfaces[0],
                    watcher_call, watcher_property, None)
reply = bus.call_sync('org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus',
                      'RequestName', GLib.Variant('(su)', ('org.kde.StatusNotifierWatcher', 4)),
                      None, Gio.DBusCallFlags.NONE, -1, None)
assert reply.unpack()[0] == 1, 'Another StatusNotifierWatcher is running'


def pump(seconds, until=lambda: False):
    context, deadline = GLib.MainContext.default(), time.monotonic() + seconds
    while time.monotonic() < deadline and not until():
        context.iteration(False)
        time.sleep(.01)
    return until()


def call(service, path, interface, method, parameters=None):
    return bus.call_sync(service, path, interface, method, parameters, None,
                         Gio.DBusCallFlags.NONE, 5000, None).unpack()


def item_property(service, path, name):
    return call(service, path, 'org.freedesktop.DBus.Properties', 'Get',
                GLib.Variant('(ss)', ('org.kde.StatusNotifierItem', name)))[0]


def layout(service, menu):
    entries = {}

    def walk(node, depth):
        identifier, properties, children = node
        label = properties.get('label', '').replace('__', '_')
        if label:
            entries[label] = (identifier, properties)
        for child in children:
            walk(child, depth + 1)
    walk(call(service, menu, 'com.canonical.dbusmenu', 'GetLayout',
              GLib.Variant('(iias)', (0, -1, [])))[1], 0)
    return entries


def click(service, menu, label):
    entries = layout(service, menu)
    assert label in entries, f'{label} not in menu: {sorted(entries)}'
    identifier, properties = entries[label]
    assert properties.get('enabled', True), f'{label} is disabled'
    call(service, menu, 'com.canonical.dbusmenu', 'Event',
         GLib.Variant('(isvu)', (identifier, 'clicked', GLib.Variant('i', 0), 0)))


with tempfile.TemporaryDirectory(prefix='flowbar-tray-') as temp:
    home = Path(temp)
    shutil.copytree(ROOT / 'dist/FlowBar', home / 'FlowBar')
    data = home / '.local/share/flowbar'
    (data / 'Data').mkdir(parents=True, mode=0o700)
    sockets = [socket.socket() for _ in range(3)]
    for sock in sockets:
        sock.bind(('127.0.0.1', 0))
    ports = [sock.getsockname()[1] for sock in sockets]
    for sock in sockets:
        sock.close()
    (data / 'settings.json').write_text(json.dumps({
        'profileName': '托盘测试', 'tun': False, 'autoConnect': False, 'autoUpdate': False,
        'proxyPort': ports[0], 'controllerPort': ports[1], 'dnsPort': ports[2]}))
    (data / 'profile.yaml').write_text('proxies: []\nproxy-groups:\n  - name: 策略\n    type: select\n'
                                       '    proxies: [DIRECT, REJECT]\nrules: ["MATCH,DIRECT"]\n')
    (data / 'Profiles').mkdir(mode=0o700)
    (data / 'Profiles/备用.yaml').write_text('proxies: []\nproxy-groups:\n  - name: 备用组\n'
                                         '    type: select\n    proxies: [DIRECT]\nrules: ["MATCH,DIRECT"]\n')
    env = dict(os.environ, HOME=str(home), XDG_CONFIG_HOME=str(home / '.config'))
    app = subprocess.Popen([str(home / 'FlowBar/flowbar')], env=env,
                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    try:
        assert pump(15, lambda: items), 'The tray item did not register'
        service, path = items[0]
        assert item_property(service, path, 'Title') == 'FlowBar'
        pixmaps = item_property(service, path, 'IconPixmap')
        assert pixmaps and all(len(data) == w * h * 4 for w, h, data in pixmaps), 'Icon pixmaps are invalid'
        tooltip = lambda: item_property(service, path, 'ToolTip')[3]
        # The item registers before the app shows its first state.
        assert pump(5, lambda: tooltip() == 'FlowBar · 已停止'), tooltip()
        menu = item_property(service, path, 'Menu')
        assert {'启动连接', '配置：托盘测试', '托盘测试', '分流模式', '使用 TUN', '退出 FlowBar'} <= set(layout(service, menu))
        print('PASS tray registers with icon, tooltip, profiles and menu')

        click(service, menu, '启动连接')
        assert pump(30, lambda: tooltip() == 'FlowBar · 本地代理已启动'), tooltip()
        call(service, menu, 'com.canonical.dbusmenu', 'AboutToShow', GLib.Variant('(i)', (0,)))
        assert pump(10, lambda: '策略组与节点' in layout(service, menu)), 'Groups did not appear'
        # Latencies appear only after a test, which the core runs for a whole group.
        delays = lambda: [label for label in layout(service, menu) if '    ' in label]
        assert not delays() and '测试延迟' in layout(service, menu)
        click(service, menu, '测试延迟')
        assert pump(15, lambda: any(label.startswith('REJECT    ') for label in delays())), delays()
        assert 'REJECT    超时' in delays(), delays()
        print('PASS tray menu tests node latency on request')
        click(service, menu, '备用')
        assert pump(30, lambda: '配置：备用' in layout(service, menu)
                    and any(label.startswith('备用组') for label in layout(service, menu)))
        assert not any(label.startswith('策略 →') for label in layout(service, menu))
        click(service, menu, '托盘测试')
        assert pump(30, lambda: '配置：托盘测试' in layout(service, menu)
                    and any(label.startswith('策略 →') for label in layout(service, menu)))
        print('PASS tray menu refreshes groups after switching profiles without reopening')
        click(service, menu, '全局代理')
        settings = data / 'settings.json'
        assert pump(10, lambda: json.loads(settings.read_text())['mode'] == 'global')
        assert layout(service, menu)['全局代理'][1].get('toggle-state') == 1
        click(service, menu, '规则分流')
        assert pump(10, lambda: json.loads(settings.read_text())['mode'] == 'rule')
        print('PASS tray menu connects, lists groups and switches modes')

        click(service, menu, '停止连接')
        assert pump(15, lambda: tooltip() == 'FlowBar · 已停止'), tooltip()
        click(service, menu, '退出 FlowBar')
        app.wait(timeout=10)
        assert app.returncode == 0, app.stdout.read()
        statuses = list((data / 'Sessions').glob('*/status.json'))
        assert statuses and all(json.loads(p.read_text())['state'] == 'stopped' for p in statuses)
        print('PASS tray stops the core and quits')
    finally:
        if app.poll() is None:
            app.kill()
            print(app.communicate()[0])
