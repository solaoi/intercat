"""Run the repair execution loop against the same eight approval scripts."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tomllib

root = Path(__file__).resolve().parents[1]
directory = Path(sys.argv[1]).resolve()
build_log = Path(sys.argv[2]).resolve()
directory.mkdir(parents=True, exist_ok=True)
records = [json.loads(line) for line in build_log.read_text().splitlines()]
binary = next(record['executable'] for record in records
              if record.get('reason') == 'compiler-artifact' and record.get('executable')
              and record['target']['name'] == 'intercat_daemon')
config = tomllib.loads((root / '.cargo/config.toml').read_text())
gitleaks = str(root / config['env']['INTERCAT_GITLEAKS']['value'])
original = root / 'research/verify/msp-approval-oracle-20261104'
cases = json.loads((original / 'comparison.json').read_text())
summary = []
for case in cases:
    name = case['case']
    path = directory / name
    path.mkdir(exist_ok=True)
    (path / 'fixture.json').write_bytes((original / name / 'fixture.json').read_bytes())
    with (path / 'typescript.log').open('w') as log:
        subprocess.run(['node', 'tools/msp_approval_oracle.mjs', str(path / 'fixture.json'),
                        str(path / 'typescript.json')], cwd=root, stdout=log,
                       stderr=subprocess.STDOUT, timeout=20, check=True)
    environment = {**os.environ, 'INTERCAT_REPAIR_ORACLE': str(path), 'INTERCAT_GITLEAKS': gitleaks}
    with (path / 'repair.log').open('w') as log:
        subprocess.run([binary, 'repair_execution::oracle_tests::approvals_match_the_pinned_sdk',
                        '--exact', '--ignored', '--nocapture'], cwd=root, env=environment,
                       stdout=log, stderr=subprocess.STDOUT, timeout=120, check=True)
    actual = json.loads((path / 'repair.json').read_text())
    expected = json.loads((path / 'typescript.json').read_text())
    def decisions(result):
        return [{key: value for key, value in frame['params'].items() if key != 'commandId'}
                for frame in result['sent']]
    matched = decisions(actual) == decisions(expected) and actual['receipts'] == expected['receipts']
    summary.append({'case': name, 'decisions': len(actual['sent']), 'matched': matched,
                    'complete': actual['complete'] and expected['complete'],
                    'sourceUnchanged': actual['sourceUnchanged']})
    print(json.dumps(summary[-1]), flush=True)
(directory / 'comparison.json').write_text(json.dumps(summary, indent=2) + '\n')
assert all(case['matched'] and case['complete'] and case['sourceUnchanged'] for case in summary)
