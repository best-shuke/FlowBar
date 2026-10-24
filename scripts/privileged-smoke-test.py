#!/usr/bin/env python3
"""Exercise real administrator authorization and detached root supervision without TUN.

Uses a local DIRECT-only fixture and temporary ports. The system displays its
normal administrator authorization dialog: osascript on macOS, pkexec on Linux,
or sudo with FLOWBAR_AUTHORIZE=sudo. Existing preferences stay untouched.
"""
import http.client
import json
import os
from pathlib import Path
import platform
import shlex
import shutil
import socket
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parent.parent
MACOS = platform.system() == 'Darwin'
if MACOS:
    BINARY = ROOT / 'dist/FlowBar.app/Contents/MacOS/FlowBar'
    USER_DATA = Path.home() / 'Library/Application Support/FlowBar'
    ROOT_BASE = Path('/Library/Application Support/FlowBar')
else:
    BINARY = ROOT / 'dist/FlowBar/flowbar'
    USER_DATA = Path.home() / '.local/share/flowbar'
    ROOT_BASE = Path('/var/lib/flowbar')
assert os.geteuid() != 0, 'Run as your normal user'
assert BINARY.is_file(), 'Build FlowBar first'
(USER_DATA / 'Data').mkdir(parents=True, mode=0o700, exist_ok=True)


def wait_for_status(session, expected, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            status = json.loads((session / 'status.json').read_text())
            if status['state'] == expected:
                return status
        except FileNotFoundError:
            pass
        time.sleep(.1)
    raise RuntimeError(f'Supervisor did not reach {expected}')


def authorize(session_id):
    arguments = [str(BINARY), '--tun-launch', str(os.getpid()), str(os.getuid()), session_id]
    if not MACOS:
        tool = ['sudo', '-n'] if os.environ.get('FLOWBAR_AUTHORIZE') == 'sudo' else ['pkexec']
        return subprocess.run(tool + arguments, capture_output=True, text=True, timeout=180)
    command = shlex.join(arguments)
    # AppleScript does not understand JSON's \uXXXX escapes, so keep non-ASCII paths literal.
    script = ('do shell script ' + json.dumps(command, ensure_ascii=False)
              + ' with administrator privileges')
    return subprocess.run(['/usr/bin/osascript', '-e', script],
                          capture_output=True, text=True, timeout=180)


for should_fail in [True, False]:
    session_id = str(uuid.uuid4())
    stage = USER_DATA / 'Sessions' / session_id
    stage.mkdir(parents=True, mode=0o700)
    session = ROOT_BASE / str(os.getuid()) / 'Sessions' / session_id
    try:
        if not should_fail:
            sockets = [socket.socket() for _ in range(2)]
            for sock in sockets:
                sock.bind(('127.0.0.1', 0))
            proxy_port, controller_port = [sock.getsockname()[1] for sock in sockets]
            token = uuid.uuid4().hex
            fixture = f'''mixed-port: {proxy_port}
bind-address: 127.0.0.1
allow-lan: false
external-controller: 127.0.0.1:{controller_port}
secret: {token}
mode: rule
log-level: warning
tun: {{enable: false}}
dns: {{enable: false}}
proxies: []
proxy-groups: []
rules: ["MATCH,DIRECT"]
'''
            path = stage / 'config.yaml'
            descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(descriptor, 'w') as file:
                file.write(fixture)
            for sock in sockets:
                sock.close()
        result = authorize(session_id)
        if should_fail:
            assert result.returncode != 0
            assert '管理员内核启动失败' in result.stderr, result.stderr
            wait_for_status(session, 'failed')
            assert (session / 'core.log').stat().st_size > 0
            print('PASS privileged startup failure reaches the caller with a concrete error')
        else:
            assert result.returncode == 0, result.stderr
            status = wait_for_status(session, 'running')
            # The authorization command has exited; the supervisor must survive.
            time.sleep(.3)
            connection = http.client.HTTPConnection('127.0.0.1', controller_port, timeout=2)
            connection.request('GET', '/configs', headers={'Authorization': 'Bearer ' + token})
            response = connection.getresponse()
            assert response.status == 200
            assert json.loads(response.read())['tun']['enable'] is False
            connection.close()
            (stage / 'stop').touch(mode=0o600)
            wait_for_status(session, 'stopped')
            assert subprocess.run(['ps', '-p', str(status['core_pid']), '-o', 'pid='],
                                  capture_output=True).returncode != 0
            print('PASS detached root supervisor survives authorization exit and reaps its core on stop')
    finally:
        (stage / 'stop').touch(mode=0o600)
        if session.exists():
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                if not (session / 'rill').exists() and not (session / 'config.yaml').exists():
                    break
                time.sleep(.1)
            assert not (session / 'rill').exists(), 'Root supervisor has not cleaned up'
            assert not (session / 'config.yaml').exists(), 'Root config has not been removed'
        shutil.rmtree(stage)

print('Privileged regression passed; TUN and DNS interception stayed disabled.')
