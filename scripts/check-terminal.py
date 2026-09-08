#!/usr/bin/env python3
"""Compare terminal defaults with rg using real PTYs. No third-party packages."""
import errno
import os
from pathlib import Path
import pty
import shutil
import subprocess
import sys
import tempfile

rrg = str(Path(sys.argv[1] if len(sys.argv) > 1 else 'target/debug/rrg').resolve())
rg = shutil.which('rg')
if not rg:
    sys.exit('rg must be installed')

def run(exe, root, args, input_bytes):
    master, slave = pty.openpty()
    env = dict(os.environ, TERM='xterm-256color')
    env.pop('NO_COLOR', None)
    env.pop('RIPGREP_CONFIG_PATH', None)
    try:
        proc = subprocess.Popen([exe, '--no-config', '--sort=path', *args], cwd=root,
                                env=env, stdin=subprocess.PIPE if input_bytes is not None else subprocess.DEVNULL,
                                stdout=slave, stderr=subprocess.PIPE)
        os.close(slave)
        slave = None
        if input_bytes is not None:
            proc.stdin.write(input_bytes)
            proc.stdin.close()
        output = bytearray()
        while True:
            try:
                chunk = os.read(master, 65536)
                if not chunk:
                    break
                output.extend(chunk)
            except OSError as err:
                if err.errno != errno.EIO:
                    raise
                break
        error = proc.stderr.read()
        return proc.wait(), bytes(output), error
    finally:
        os.close(master)
        if slave is not None:
            os.close(slave)

with tempfile.TemporaryDirectory(prefix='rrg-terminal-') as root:
    Path(root, 'a.txt').write_text('needle needle\nother\n')
    Path(root, 'b.txt').write_text('needle second\n')
    for args, data in [
        (['needle', '.'], None),
        (['needle', 'a.txt'], None),
        (['-N', '--no-heading', 'needle', '.'], None),
        (['--color=never', 'needle', '.'], None),
        (['-c', 'needle', '.'], None),
        (['needle'], b'needle\nother\n'),
        (['needle', '-'], b'needle\nother\n'),
    ]:
        expected = run(rg, root, args, data)
        actual = run(rrg, root, args, data)
        assert actual[:2] == expected[:2], (args, expected, actual)
print('7 terminal compatibility scenarios passed')
