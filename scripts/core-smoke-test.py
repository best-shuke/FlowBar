#!/usr/bin/env python3
"""Verify DNS selector changes and XPro in an actual core binary, using loopback only."""
import http.client
import json
from pathlib import Path
import socket
import socketserver
import struct
import subprocess
import sys
import tempfile
import threading
import time


def exact(connection, size):
    data = bytearray()
    while len(data) < size:
        chunk = connection.recv(size - len(data))
        if not chunk:
            raise EOFError
        data.extend(chunk)
    return data


class DnsHandler(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(5)
        try:
            while True:
                size, = struct.unpack('!H', exact(self.request, 2))
                query = exact(self.request, size)
                end = 12
                while query[end]:
                    end += query[end] + 1
                end += 5
                answer = query[:end]
                answer[2:4] = bytes.fromhex('8180')
                answer[6:12] = bytes.fromhex('000100000000')
                answer.extend(bytes.fromhex('c00c000100010000003c0004cb007109'))
                self.request.sendall(struct.pack('!H', len(answer)) + answer)
        except (EOFError, OSError):
            pass


class DnsServer(socketserver.ThreadingTCPServer):
    daemon_threads = True


def main():
    core = Path(sys.argv[1]).resolve()
    with tempfile.TemporaryDirectory(prefix='flowbar-core-') as temp, DnsServer(('127.0.0.1', 0), DnsHandler) as dns:
        directory = Path(temp)
        threading.Thread(target=dns.serve_forever, daemon=True).start()
        with socket.socket() as reservation:
            reservation.bind(('127.0.0.1', 0))
            port = reservation.getsockname()[1]
        token = 'isolated-core-smoke-token'
        key = directory / 'device.key'
        key.write_bytes(bytes(range(32)))
        config = directory / 'config.yaml'
        config.write_text(f'''ipv6: false
external-controller: 127.0.0.1:{port}
secret: {token}
log-level: warning
proxies:
  - name: XPro fixture
    type: xpro
    server: localhost
    port: 443
    device-id: '01010101010101010101010101010101'
    key-file: {json.dumps(str(key))}
proxy-groups:
  - name: inner
    type: select
    proxies: [REJECT, DIRECT]
  - name: outer
    type: select
    proxies: [inner]
rules: [MATCH,DIRECT]
dns:
  enable: true
  use-system-hosts: false
  nameserver: ['tcp://127.0.0.1:{dns.server_address[1]}#outer']
''', encoding='utf-8')

        def api(path, method='GET', body=None):
            connection = http.client.HTTPConnection('127.0.0.1', port, timeout=8)
            headers = {'Authorization': 'Bearer ' + token}
            if body is not None:
                body = json.dumps(body)
                headers['Content-Type'] = 'application/json'
            try:
                connection.request(method, path, body, headers)
                response = connection.getresponse()
                data = response.read()
                assert response.status < 300, (path, response.status, data)
                return json.loads(data) if data else None
            finally:
                connection.close()

        with (directory / 'core.log').open('wb') as log:
            process = subprocess.Popen([str(core), '-d', str(directory), '-f', str(config)],
                                       stdout=log, stderr=log)
            try:
                for _ in range(100):
                    assert process.poll() is None, 'core exited during startup'
                    try:
                        api('/version')
                        break
                    except (OSError, http.client.HTTPException):
                        time.sleep(.1)
                else:
                    raise AssertionError('core did not become ready')
                assert not api('/dns/query?name=before.test&type=A').get('Answer')
                api('/proxies/inner', 'PUT', {'name': 'DIRECT'})
                answers = api('/dns/query?name=after.test&type=A').get('Answer', [])
                assert any(a.get('data') == '203.0.113.9' for a in answers), 'DNS did not follow the live selector'
                api('/proxies/inner', 'PUT', {'name': 'REJECT'})
                assert not api('/dns/query?name=blocked-again.test&type=A').get('Answer'), 'DNS kept the old DIRECT selection'
                api('/api/proxy-groups/inner/select', 'PUT', {'name': 'DIRECT'})
                answers = api('/dns/query?name=alternate-api.test&type=A').get('Answer', [])
                assert any(a.get('data') == '203.0.113.9' for a in answers), 'group API did not update DNS'
                proxies = api('/proxies')['proxies']
                fixture = proxies.get('XPro fixture', {})
                assert fixture.get('type', '').lower() == 'xpro' and fixture.get('alive'), 'XPro is disabled or failed to load'
            finally:
                process.terminate()
                try:
                    process.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=3)
                dns.shutdown()
    print('PASS core binary: DNS follows nested selector changes in both directions; XPro loads')


if __name__ == '__main__':
    main()
