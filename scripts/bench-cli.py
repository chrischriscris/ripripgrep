#!/usr/bin/env python3
"""Small warm-cache CLI benchmark with output equivalence checks."""
import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import statistics
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--rrg', default='target/release/rrg')
parser.add_argument('--baseline', help='Optional pre-compatibility rrg executable')
args = parser.parse_args()
rg = shutil.which('rg')
if not rg:
    parser.error('rg must be installed')
env = dict(os.environ)
env.pop('RIPGREP_CONFIG_PATH', None)
with tempfile.TemporaryDirectory(prefix='rrg-cli-bench-') as directory:
    corpus = Path(directory)
    for i in range(1000):
        (corpus / f'{i:04}.txt').write_text(
            'ordinary line for searching\n' * 127 + 'needle benchmark match\n')
    commands = {
        'rrg': [str(Path(args.rrg).resolve()), '-nH', '-F', 'needle', directory],
        'rg': [rg, '--no-config', '-nH', '-F', 'needle', directory],
    }
    if args.baseline:
        commands['baseline'] = [str(Path(args.baseline).resolve()), '-F', 'needle', directory]
    expected = None
    results = {}
    for name, command in commands.items():
        output = sorted(subprocess.check_output(command, env=env, stdin=subprocess.DEVNULL).splitlines())
        if expected is None:
            expected = output
        assert output == expected and len(output) == 1000, f'{name}: output mismatch'
        durations = []
        for run in range(12):
            start = time.perf_counter()
            subprocess.run(command, env=env, stdin=subprocess.DEVNULL,
                           stdout=subprocess.DEVNULL, check=True)
            if run >= 2:
                durations.append((time.perf_counter() - start) * 1000)
        results[name] = {
            'median_ms': statistics.median(durations),
            'min_ms': min(durations), 'max_ms': max(durations),
        }
    print(json.dumps({
        'platform': platform.platform(),
        'rust': subprocess.check_output(['rustc', '--version'], text=True).strip(),
        'rg': subprocess.check_output([rg, '--version'], text=True).splitlines()[0],
        'files': 1000, 'bytes_per_file': (corpus / '0000.txt').stat().st_size,
        'warmups': 2, 'runs': 10, 'results': results,
    }, indent=2))
