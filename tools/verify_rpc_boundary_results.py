"""Reconcile the native RPC, process-metadata, Keychain, and policy evidence."""
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import sys


def verify(directory):
    repository = Path(__file__).resolve().parents[1]
    evidence = directory.parent
    live = evidence / 'rpc-live-20261104-run4'
    result = json.loads((live / 'result.json').read_text())
    assert result['passed'] and result['unauthenticatedRejected']
    assert result['metadataPositiveControl'] and result['tokenNotInDatabase']
    assert not result.get('forcedDaemonStop', False)
    assert result['shell'] == {'connected': False, 'errno': 1}
    assert result['paidTurns'] == result['savedTurnStarts'] == 1
    for name in ('daemon', 'worker'):
        scan = result[name]
        assert scan['argvCount'] > 0 and scan['environmentCount'] > 0
        assert not scan['tokenInArguments'] and not scan['tokenInEnvironment']
    with sqlite3.connect(f'file:{live / "events.sqlite"}?mode=ro', uri=True) as database:
        assert database.execute('PRAGMA integrity_check').fetchone()[0] == 'ok'
        assert database.execute('PRAGMA user_version').fetchone()[0] == 50
        assert database.execute("SELECT count(*) FROM operations WHERE kind='turn.start' AND state='complete'").fetchone()[0] == 1
        assert database.execute("SELECT count(*) FROM operations WHERE kind='checkpoint.save' AND state='complete'").fetchone()[0] == 1
        frames = [json.loads(payload) for payload, in database.execute("SELECT payload FROM events WHERE kind='msp.received'")]
        tools = {}
        for frame in frames:
            item = frame.get('params', {}).get('item', {})
            if item.get('kind') == 'toolCall':
                tools[item['itemId']] = item
        calls = [item for item in tools.values() if item.get('tool') == 'bash']
        assert len(calls) == 1 and calls[0]['status'] == 'completed'
        args = json.loads(calls[0]['args'])
        assert args['command'].startswith('/usr/bin/python3 probe.py ')
        assert args.get('sandbox_permissions') != 'require_escalated'
        shell = json.loads(calls[0]['visibleOutput'])
        assert shell['exit_code'] == 0 and shell['terminal_status'] == 'completed'
        assert json.loads(shell['output']) == result['shell']
        sessions = [json.loads(payload) for payload, in database.execute("SELECT payload FROM events WHERE kind='worker.session'")]
        assert len(sessions) == 1
        assert sessions[0]['result']['session']['permissionState']['profileId'] == 'intercat-worker'

    policies = evidence / 'rpc-policies-20261104'
    for name in ('worktree', 'checkpoint', 'transfer', 'git', 'code'):
        probe = json.loads((policies / f'{name}.json').read_text())
        profile = (policies / f'{name}.sb').read_text()
        assert probe['positiveControl'] and not probe['serverAccepted']
        assert probe['probe'] == {'connected': False, 'errno': 1}
        assert '(deny default)' in profile and '(allow network' not in profile
    keychain = json.loads((directory / 'keychain.json').read_text())
    assert keychain == {'status': -25293, 'readSucceeded': False, 'interactionDisabled': True}

    secret = evidence / 'secret-policy-20261104'
    prototype = json.loads((secret / 'native-positive/result.json').read_text())
    assert prototype['passed'] is False and prototype['rows'] == 0
    assert prototype['attemptedPaidTurns'] == 1
    assert 'Seatbelt' in prototype['failure']
    history = [json.loads(line) for line in (secret / 'native-positive/received.jsonl').read_text().splitlines()]
    reads = {}
    for frame in history:
        item = frame.get('params', {}).get('item', {})
        if frame.get('method') == 'item/completed' and item.get('tool') == 'read_file':
            reads[json.loads(item['args'])['path']] = item['status']
    assert reads == {'.env': 'failed', 'packages/app/.env': 'failed', '.env.example': 'completed', 'packages/app/.env.example': 'completed'}

    rust = (directory / 'rust.log').read_text()
    counts = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', rust)
    assert counts and 'Doc-tests intercat_git' in rust and 'FAILED' not in rust
    for name in ('worktree', 'checkpoint', 'checkout_profiles'):
        assert re.search(rf'{name}.*cannot_connect_to_the_private_rpc_socket \.\.\. ok', rust)
    assert not (directory / 'format.log').read_text().strip()
    assert '3 passed; 0 failed' in (directory / 'profiles-final.log').read_text()
    assert 'Finished' in (directory / 'clippy.log').read_text()
    assert 'error:' not in (directory / 'clippy.log').read_text()
    sources = [
        'crates/intercat-daemon/src/main.rs',
        'crates/intercat-daemon/src/rpc.rs',
        'crates/intercat-daemon/src/host.rs',
        'crates/intercat-daemon/src/runtime.rs',
        'crates/intercat-daemon/src/worktree.rs',
        'crates/intercat-daemon/src/checkpoint.rs',
        'crates/intercat-daemon/src/checkout.rs',
        'crates/intercat-daemon/src/sandbox_probe.rs',
        'crates/intercat-daemon/examples/rpc_boundary_fixture.rs',
        'tools/verify_live_rpc_boundary.py',
        'tools/verify_rpc_boundary_results.py',
        'tools/verify_keychain_boundary.m',
    ]
    return {'milestoneRequirement': 'M1.48', 'schema': 50,
            'rustPassed': sum(int(a) for a, _ in counts),
            'rustIgnored': sum(int(b) for _, b in counts),
            'sandboxProfilesVerified': 5, 'liveRPCConnectionDenied': True,
            'tokenAbsentFromProcessArgumentsEnvironmentAndDatabase': True,
            'keychainReadWithoutInteractionDenied': True,
            'secretExampleExceptionComplete': False,
            'additionalPaidTurns': 2, 'cumulativePaidTurns': 150,
            'sourceSha256': {path: hashlib.sha256((repository / path).read_bytes()).hexdigest() for path in sources}}


if __name__ == '__main__':
    directory = Path(sys.argv[1])
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({key: value for key, value in result.items() if key != 'sourceSha256'}, ensure_ascii=False))
