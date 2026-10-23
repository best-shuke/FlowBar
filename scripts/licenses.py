#!/usr/bin/env python3
"""Bundle attribution directly from the locally resolved, locked runtime dependency sources."""
import json
from pathlib import Path
import subprocess
import sys

# The target triple, by default the one cargo builds for on this machine.
target = sys.argv[2] if len(sys.argv) > 2 else next(
    line.split(': ', 1)[1] for line in subprocess.check_output(['rustc', '-vV'], text=True).splitlines()
    if line.startswith('host: '))
metadata = json.loads(subprocess.check_output([
    'cargo', 'metadata', '--format-version', '1', '--locked', '--offline',
    '--filter-platform', target
]))
nodes = {node['id']: node for node in metadata['resolve']['nodes']}
root = metadata['resolve']['root']
visited = set()
pending = [root]
while pending:
    package = pending.pop()
    if package in visited:
        continue
    visited.add(package)
    for dep in nodes[package]['deps']:
        if any(kind['kind'] != 'dev' for kind in dep['dep_kinds']):
            pending.append(dep['pkg'])

parts = ['FlowBar Rust runtime dependency notices\n']
for package in sorted(metadata['packages'], key=lambda p: p['name']):
    if package['id'] not in visited or package['id'] == root:
        continue
    directory = Path(package['manifest_path']).parent
    parts += [f"\n{'=' * 72}\n{package['name']} {package['version']}\n",
              f"Repository: {package.get('repository') or ''}\nLicense: {package.get('license') or ''}\n"]
    files = sorted(path for path in directory.iterdir()
                   if path.is_file() and path.name.upper().startswith(('LICENSE', 'LICENCE', 'COPYING', 'NOTICE')))
    for path in files:
        parts += [f'\n--- {path.name} ---\n', path.read_text(encoding='utf-8', errors='replace'), '\n']
Path(sys.argv[1]).write_text(''.join(parts), encoding='utf-8')
