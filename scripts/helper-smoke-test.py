#!/usr/bin/env python3
"""macOS CI: install once, reuse across app processes, reject an unrelated client.

Uses sudo (CI has passwordless sudo). Local runs can prompt for an administrator
password. Fixtures never enable TUN or DNS. Run only after packaging the app.
"""
import json
import os
from pathlib import Path
import socket
import subprocess

assert os.uname().sysname == 'Darwin'
root = Path(__file__).resolve().parent.parent
binary = root / 'dist/FlowBar.app/Contents/MacOS/FlowBar'
uid = os.getuid()
label = f'dev.local.FlowBar.tun.{uid}'
base = Path(f'/Library/Application Support/FlowBar/{uid}')
plist = Path(f'/Library/LaunchDaemons/{label}.plist')
assert not plist.exists(), 'Test must not replace an existing helper installation'
try:
    subprocess.run(['sudo', str(binary), '--install-tun-helper', str(uid)], check=True, timeout=30)
    for _ in range(2):
        subprocess.run([str(binary), '--tun-helper-smoke'], check=True, timeout=60)
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(10)
        client.connect(str(base / 'helper.sock'))
        response = json.loads(client.recv(65536))
        assert response['error'], 'Helper must reject a Python client with the same UID'
    print('PASS same-user unsigned/unrelated client rejected; helper reused across application launches')
finally:
    subprocess.run(['sudo', 'launchctl', 'bootout', f'system/{label}'], check=False, capture_output=True)
    subprocess.run(['sudo', 'rm', '-f', str(plist), str(base / 'helper.sock')], check=True)
    subprocess.run(['sudo', 'rm', '-rf', str(base / 'Helper')], check=True)
