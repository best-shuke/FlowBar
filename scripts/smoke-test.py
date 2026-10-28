#!/usr/bin/env python3
"""Exercises the real bundled core on loopback. Never enables TUN or changes system proxy/DNS."""
import http.client
import http.server
import json
import os
from pathlib import Path
import platform
import socket
import subprocess
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parent.parent
SYSTEM = platform.system()
if SYSTEM == 'Darwin':
    GUI = ROOT / 'dist/FlowBar.app/Contents/MacOS/FlowBar'
    CORE = ROOT / 'dist/FlowBar.app/Contents/Resources/rill'
    HOME = Path.home() / 'Library/Application Support/FlowBar'
elif SYSTEM == 'Windows':
    GUI = ROOT / 'dist/FlowBar/FlowBar.exe'
    CORE = ROOT / 'dist/FlowBar/rill.exe'
    HOME = Path(os.environ['LOCALAPPDATA']) / 'FlowBar'
else:
    GUI = ROOT / 'dist/FlowBar/flowbar'
    CORE = ROOT / 'dist/FlowBar/rill'
    HOME = Path.home() / '.local/share/flowbar'
DATA = HOME / 'Data'
TOKEN = 'flowbar-local-smoke-test-token'
sockets = [socket.socket() for _ in range(3)]
for sock in sockets:
    sock.bind(('127.0.0.1', 0))
PROXY_PORT, CONTROLLER_PORT, DNS_PORT = [sock.getsockname()[1] for sock in sockets]
for sock in sockets:
    sock.close()

def api(path, method='GET', body=None, authorized=True):
    conn = http.client.HTTPConnection('127.0.0.1', CONTROLLER_PORT, timeout=2)
    headers = {'Authorization': 'Bearer ' + TOKEN} if authorized else {}
    if body is not None:
        body = json.dumps(body).encode()
        headers['Content-Type'] = 'application/json'
    conn.request(method, path, body=body, headers=headers)
    response = conn.getresponse()
    payload = response.read()
    conn.close()
    return response.status, json.loads(payload) if payload else {}

def render(source, target, tun=False):
    subprocess.run([str(GUI), '--render', str(source), str(target)] + (['--tun'] if tun else []),
                   check=True, capture_output=True)
    # --render uses the application's default ports; isolate each test instance.
    text = target.read_text(encoding='utf-8')
    text = text.replace('mixed-port: 17890', f'mixed-port: {PROXY_PORT}')
    text = text.replace('127.0.0.1:19090', f'127.0.0.1:{CONTROLLER_PORT}')
    text = text.replace('127.0.0.1:15353', f'127.0.0.1:{DNS_PORT}')
    target.write_text(text, encoding='utf-8')

def validate(path, directory):
    result = subprocess.run([str(CORE), '-t', '-d', str(directory), '-f', str(path)],
                            capture_output=True, timeout=60)
    if result.returncode:
        raise RuntimeError('rill rejected the generated config')

def launch(config, directory, log):
    env = {'HOME': str(Path.home()), 'PATH': '/usr/bin:/bin:/usr/sbin:/sbin'}
    if SYSTEM == 'Windows':
        env = {name: os.environ[name] for name in ('SystemRoot', 'TEMP', 'USERPROFILE') if name in os.environ}
    process = subprocess.Popen([str(CORE), '-d', str(directory), '-f', str(config)],
                               stdout=log, stderr=log, env=env)
    for _ in range(100):
        try:
            if api('/version')[0] == 200:
                return process
        except (OSError, http.client.HTTPException):
            pass
        if process.poll() is not None:
            raise RuntimeError('rill exited during startup')
        time.sleep(.1)
    process.terminate()
    process.wait(timeout=8)
    raise RuntimeError('rill did not become ready')

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b'flowbar-direct-ok')
    def log_message(self, *_):
        pass

with tempfile.TemporaryDirectory(prefix='flowbar-smoke-') as temp:
    temp = Path(temp)
    os.chmod(temp, 0o700)
    # The active imported profile; FlowBar 0.5 and earlier kept a single profile.yaml.
    try:
        name = json.loads((HOME / 'settings.json').read_text(encoding='utf-8'))['profileName']
        original = HOME / 'Profiles' / f'{name}.yaml'
    except (OSError, ValueError, KeyError):
        original = HOME / 'profile.yaml'
    if not original.exists():
        original = HOME / 'profile.yaml'
    if original.exists():
        for tun in [False, True]:
            config = temp / ('tun-validation.yaml' if tun else 'runtime.yaml')
            render(original, config, tun)
            validate(config, DATA)
        print('PASS imported profile: both runtime configurations accepted by rill -t (no TUN activation)')
        with (temp / 'real-core.log').open('wb') as log:
            process = launch(temp / 'runtime.yaml', DATA, log)
            try:
                assert api('/version', authorized=False)[0] == 401
                current = api('/configs')[1]
                assert current['mode'] == 'rule' and current['mixed-port'] == PROXY_PORT
                assert current['tun']['enable'] is False
                assert api('/configs', 'PATCH', {'mode': 'global'})[0] == 204
                assert api('/configs')[1]['mode'] == 'global'
                assert api('/configs', 'PATCH', {'mode': 'rule'})[0] == 204
                proxies = api('/proxies')[1]['proxies']
                name, group = next((name, value) for name, value in proxies.items()
                                   if value.get('type') == 'Selector' and len(value.get('all', [])) > 1)
                import urllib.parse
                path = '/proxies/' + urllib.parse.quote(name, safe='')
                choice = next(value for value in group['all'] if value != group.get('now'))
                assert api(path, 'PUT', {'name': choice})[0] == 204
                assert api(path)[1]['now'] == choice
                if group.get('now'):
                    api(path, 'PUT', {'name': group['now']})
                print('PASS authenticated control API, mode changes, node selection')
                if SYSTEM != 'Windows':
                    rss = int(subprocess.check_output(['ps', '-p', str(process.pid), '-o', 'rss=']).strip())
                    print(f'INFO imported profile core, TUN disabled, startup RSS: {rss / 1024:.1f} MiB')
            finally:
                process.terminate()
                process.wait(timeout=8)

    fixture = temp / 'fixture.yaml'
    fixture.write_text('''proxies: []
proxy-groups: []
rules:
  - DOMAIN,blocked.flowbar.invalid,REJECT
  - IP-CIDR,127.0.0.0/8,DIRECT,no-resolve
  - MATCH,DIRECT
''')
    runtime = temp / 'fixture-runtime.yaml'
    render(fixture, runtime)
    validate(runtime, temp)
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    with (temp / 'loopback-core.log').open('wb') as log:
        process = launch(runtime, temp, log)
        try:
            conn = http.client.HTTPConnection('127.0.0.1', PROXY_PORT, timeout=3)
            conn.request('GET', f'http://127.0.0.1:{server.server_port}/probe')
            response = conn.getresponse()
            assert response.status == 200 and response.read() == b'flowbar-direct-ok'
            conn.close()
            conn = http.client.HTTPConnection('127.0.0.1', PROXY_PORT, timeout=3)
            conn.request('GET', 'http://blocked.flowbar.invalid/')
            try:
                response = conn.getresponse()
                assert response.status >= 400
                response.read()
            except (ConnectionResetError, http.client.RemoteDisconnected):
                pass
            conn.close()
            print('PASS actual HTTP proxy: DIRECT succeeds, REJECT blocks')
        finally:
            process.terminate()
            process.wait(timeout=8)
            server.shutdown()

print('All integration checks passed; system proxy, DNS and TUN were not changed.')
