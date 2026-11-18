#!/usr/bin/env python3
"""Read executable signing identities; never open a keychain or execute inputs."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys


def inspect(path):
    path = Path(path).resolve(strict=True)
    if not path.is_file():
        raise ValueError('not_regular_file')
    with path.open('rb') as source:
        digest = hashlib.file_digest(source, 'sha256').hexdigest()
    # codesign only inspects the file. Keep raw diagnostics (including paths)
    # out of reports and do not run the inspected executable.
    result = subprocess.run(
        ['/usr/bin/codesign', '--display', '--requirements', '-', str(path)],
        capture_output=True, timeout=10, check=False)
    text = (result.stdout + result.stderr).decode('utf-8', errors='replace')
    if result.returncode:
        return {'sha256': digest, 'inspection_ok': False,
                'error': 'codesign_inspection_failed'}
    requirement = re.search(r'^#?\s*designated => (.+)$', text, re.MULTILINE)
    if not requirement:
        return {'sha256': digest, 'inspection_ok': False,
                'error': 'designated_requirement_missing'}
    requirement = requirement.group(1).strip()
    return {'sha256': digest, 'inspection_ok': True,
            'requirement_sha256': hashlib.sha256(requirement.encode()).hexdigest(),
            'identity_bound_only_to_code_hash': bool(re.fullmatch(
                r'cdhash H"[0-9a-fA-F]+"', requirement))}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('executables', nargs='+', type=Path)
    args = parser.parse_args()
    if sys.platform != 'darwin':
        parser.error('macOS required')
    rows = []
    for path in args.executables:
        try:
            rows.append(inspect(path))
        except (OSError, ValueError, subprocess.SubprocessError):
            rows.append({'inspection_ok': False, 'error': 'inspection_unavailable'})
    print(json.dumps({'schema_version': 1, 'keychain_accessed': False,
                      'executables_run': False, 'identities': rows}, indent=2))
    return 0 if all(row['inspection_ok'] for row in rows) else 1


if __name__ == '__main__':
    raise SystemExit(main())
