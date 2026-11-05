"""Exercise the real worker and official TS facade with identical fake-host scripts."""
import copy
import json
from pathlib import Path
import subprocess
import sys

root = Path(__file__).resolve().parents[1]
directory = Path(sys.argv[1]).resolve()
base = json.loads((directory / 'fixture.json').read_text())
cases = [
    ('update_after_ack', 2, False, False, None),
    ('update_before_open_ack', 2, True, False, None),
    ('update_before_terminal_ack', 1, True, False, None),
    ('duplicate_requests_after_terminal', 1, False, True, None),
    ('abort_initial', 1, True, False, 'request'),
    ('abort_updated_stage', 2, False, False, 'update'),
    ('abort_update_before_ack', 2, True, False, 'update'),
    ('server_request_receipt', 2, False, True, None),
]
summary = []
for name, terminal, before, duplicate, abort in cases:
    path = directory / name
    path.mkdir(exist_ok=True)
    fixture = copy.deepcopy(base)
    fixture.update(terminalAt=terminal, updateBeforeAck=before, duplicateRequest=duplicate)
    fixture['serverRequest'] = name == 'server_request_receipt'
    if abort:
        fixture[abort]['availableChoices'] = [choice for choice in fixture[abort]['availableChoices']
                                              if choice['decision'] == 'abort']
    fixture_path = path / 'fixture.json'
    fixture_path.write_text(json.dumps(fixture, ensure_ascii=False, indent=2) + '\n')
    for language, command in [
        ('typescript', ['node', 'tools/msp_approval_oracle.mjs']),
        ('rust', ['target/debug/examples/msp_approval']),
    ]:
        with (path / f'{language}.log').open('w') as stream:
            result = subprocess.run(command + [str(fixture_path), str(path / f'{language}.json')],
                                    cwd=root, stdout=stream, stderr=subprocess.STDOUT, timeout=25)
        assert result.returncode == 0, (name, language, result.returncode)
    actual = json.loads((path / 'rust.json').read_text())
    expected = json.loads((path / 'typescript.json').read_text())
    # Command/request IDs are minted by different runtimes. Their uniqueness,
    # durable correlation, and one-send property are checked independently.
    def decisions(result):
        return [{key: frame['params'][key] for key in ['approvalId', 'sessionId', 'requirementId', 'choiceId']}
                for frame in result['sent']]
    matched = decisions(actual) == decisions(expected) and actual['receipts'] == expected['receipts']
    summary.append({'case': name, 'decisions': len(actual['sent']), 'matched': matched,
                    'complete': actual['complete'] and expected['complete']})
    print(json.dumps(summary[-1]), flush=True)
(directory / 'comparison.json').write_text(json.dumps(summary, indent=2) + '\n')
assert all(case['matched'] and case['complete'] for case in summary)
