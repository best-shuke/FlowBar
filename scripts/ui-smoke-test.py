#!/usr/bin/env python3
"""Run real UI actions and asynchronous core management in an isolated, non-TUN test home.

macOS uses the built app bundle in dist/FlowBar.app; Linux and Windows use dist/FlowBar.
Linux needs a display and a D-Bus session, for example:
    xvfb-run -a dbus-run-session -- python3 scripts/ui-smoke-test.py
"""
import json
import os
from pathlib import Path
import platform
import plistlib
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import uuid

root = Path(__file__).resolve().parent.parent
system = platform.system()
with tempfile.TemporaryDirectory(prefix='flowbar-ui-') as temp:
    home = Path(temp)
    env = dict(os.environ, FLOWBAR_TEST_LOGIN='1')
    if system == 'Darwin':
        bundle = home / 'FlowBar Validation.app'
        shutil.copytree(root / 'dist/FlowBar.app', bundle)
        info_path = bundle / 'Contents/Info.plist'
        with info_path.open('rb') as file:
            info = plistlib.load(file)
        info['CFBundleIdentifier'] = 'dev.local.FlowBar.Validation.' + uuid.uuid4().hex
        info['CFBundleName'] = 'FlowBar Validation'
        with info_path.open('wb') as file:
            plistlib.dump(info, file)
        subprocess.run(['/usr/bin/codesign', '--force', '--sign', '-', str(bundle)], check=True, capture_output=True)
        binary = bundle / 'Contents/MacOS/FlowBar'
        data = home / 'Library/Application Support/FlowBar'
        env['HOME'] = str(home)
    elif system == 'Windows':
        shutil.copytree(root / 'dist/FlowBar', home / 'FlowBar')
        binary = home / 'FlowBar/FlowBar.exe'
        data = home / 'Local/FlowBar'
        env.update(LOCALAPPDATA=str(home / 'Local'), APPDATA=str(home / 'Roaming'))
        # The login item lives in the user's registry, so only CI exercises it.
        if not os.environ.get('CI'):
            del env['FLOWBAR_TEST_LOGIN']
    else:
        shutil.copytree(root / 'dist/FlowBar', home / 'FlowBar')
        binary = home / 'FlowBar/flowbar'
        data = home / '.local/share/flowbar'
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / '.config'))
    data.mkdir(parents=True, mode=0o700)
    (data / 'Data').mkdir(mode=0o700)
    sockets = [socket.socket() for _ in range(3)]
    for sock in sockets:
        sock.bind(('127.0.0.1', 0))
    ports = [sock.getsockname()[1] for sock in sockets]
    for sock in sockets:
        sock.close()
    settings = {'profileName': '本地界面测试', 'tun': False, 'autoConnect': False,
                'proxyPort': ports[0], 'controllerPort': ports[1], 'dnsPort': ports[2]}
    (data / 'settings.json').write_text(json.dumps(settings), encoding='utf-8')
    # The single profile of FlowBar 0.5, which becomes the first of two profiles.
    (data / 'profile.yaml').write_text('''proxies: []
proxy-groups:
  - name: 策略 / 测试
    type: select
    proxies: [DIRECT, REJECT]
rules: ["MATCH,DIRECT"]
''', encoding='utf-8')
    (data / 'Profiles').mkdir(mode=0o700)
    (data / 'Profiles/备用.yaml').write_text('''proxies: []
proxy-groups:
  - name: 备用组
    type: select
    proxies: [DIRECT]
rules: ["MATCH,备用组"]
''', encoding='utf-8')
    result = subprocess.run([str(binary), '--ui-smoke'], env=env, capture_output=True,
                            text=True, encoding='utf-8', errors='replace', timeout=60)
    if result.returncode:
        print(result.stdout, result.stderr)
        raise SystemExit(result.returncode)
    assert 'PASS native Rust UI' in result.stdout, f'UI test did not execute: {result.stdout} {result.stderr}'
    print(result.stdout.strip())
    saved = json.loads((data / 'settings.json').read_text(encoding='utf-8'))
    assert saved['autoConnect'] is False and saved['tun'] is False
    assert saved['mode'] == 'rule' and saved['selections']['策略 / 测试'] == 'DIRECT'
    assert not (data / 'profile.yaml').exists() and (data / 'Profiles/本地界面测试.yaml').exists()
    assert saved['profileName'] == '本地界面测试', saved['profileName']
    statuses = list((data / 'Sessions').glob('*/status.json'))
    assert statuses and all(json.loads(path.read_text())['state'] == 'stopped' for path in statuses)
    if system == 'Linux':
        assert not (home / '.config/autostart').exists() or not any((home / '.config/autostart').iterdir())
    time.sleep(.2)
    print('PASS isolated preferences persisted; legacy profile migrated; node choices kept per profile; '
          'temporary test login item removed; DNS, proxy and TUN unchanged')
sys.exit(0)
