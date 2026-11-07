"""Inspect native per-tool file evidence using one paid, disposable Muse turn."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

from verify_host_hooks import Host, command_id


def request(host, method, params):
    host.counter += 1
    host.send({'jsonrpc': '2.0', 'id': host.counter, 'method': method, 'params': params})
    deadline = time.monotonic() + 120
    while True:
        frame = host.receive(deadline)
        if frame.get('id') == host.counter:
            if 'error' in frame:
                raise RuntimeError(json.dumps(frame['error']))
            return frame['result']
        host.events.append(frame)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--auth', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--previous-paid-turns', type=int, required=True)
    parser.add_argument('--edge-cases', action='store_true')
    args = parser.parse_args()
    binary, auth = args.binary.resolve(strict=True), args.auth.resolve(strict=True)
    assert hashlib.sha256(binary.read_bytes()).hexdigest() == '1c96114408acbeade9a8fb79e3b4425e5b64d50425938568aef01bdc04ce2e03'
    output = args.output.resolve()
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    temporary = Path(tempfile.mkdtemp(prefix='intercat-clip-probe-'))
    workspace, config = temporary / 'workspace', temporary / 'config'
    workspace.mkdir()
    (config / 'muse').mkdir(parents=True)
    (output / 'host').mkdir()
    link = config / 'muse/auth.json'
    link.symlink_to(auth)
    source = Path(__file__).resolve().parents[1]
    settings = {
        'schema_version': 1, 'provider': 'meta', 'model': 'muse-spark-1.3-contributor',
        'contributor_mode_enabled': False, 'agents': {'execution_capacity': 1},
        'run': {'toolset': ['write_file', 'edit_file', 'bash']},
        'permissions': {'schema_version': 1, 'default_profile': 'intercat-worker',
                        'profiles': {'intercat-worker': {'extends': ':ask-me',
                                     'filesystem': {str(source): 'deny', str(config): 'deny', str(auth): 'deny'}}}},
    }
    (config / 'muse/settings.json').write_text(json.dumps(settings))
    (workspace / 'file-existing.txt').write_text('before\n')
    git_env = {**os.environ, 'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_CONFIG_NOSYSTEM': '1'}
    subprocess.run(['/usr/bin/git', 'init', '-q', '--template=', '-b', 'main'], cwd=workspace, env=git_env, check=True)
    subprocess.run(['/usr/bin/git', 'add', 'file-existing.txt'], cwd=workspace, env=git_env, check=True)
    subprocess.run(['/usr/bin/git', '-c', 'user.name=Dummy', '-c', 'user.email=dummy@example.invalid', 'commit', '-qm', 'Dummy baseline'], cwd=workspace, env=git_env, check=True)
    shell = "printf 'shell\\n' > file-shell.txt"
    prompt = (
        'This is a disposable protocol test. Do exactly these three operations, one tool call each, in order: '
        '1. Use write_file to create file-created.txt with exactly one line: created. '
        '2. Use edit_file to change the existing single line in file-existing.txt from before to after. '
        f'3. Use bash to run exactly this command: {shell}\n'
        'Do not read or write anything else, do not use the network, do not create commits. '
        'Finish with a short report after these three calls.'
    )
    if args.edge_cases:
        prompt = ('This is a disposable protocol test. Use write_file to create file-empty.txt with empty content. '
                  'Then use write_file to write file-existing.txt with exactly its existing content: before followed by a newline. '
                  'Then use edit_file on file-existing.txt to replace before with before, an intentional no-op. '
                  'Attempt these three calls once each even if a no-op returns an error. '
                  'Do not read or write other files, do not use the network, do not commit. Report briefly.')
    result = {'attemptedPaidTurns': 0, 'cumulativePaidTurns': args.previous_paid_turns,
              'prompt': prompt, 'approvals': [], 'items': [], 'outputs': {}}
    save = lambda: (output / 'result.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    save()
    host = None
    try:
        host = Host(binary, workspace, config, output / 'host', False)
        result['initialize'] = request(host, 'initialize', {'clientInfo': {'name': 'intercat_clip_evidence', 'version': '1'}, 'capabilities': {'userInputDialogs': True}})
        host.send({'jsonrpc': '2.0', 'method': 'initialized'})
        session = request(host, 'session/start', {'commandId': command_id(), 'workspaceRoot': str(workspace), 'approvalMode': 'promptUnmatched'})
        sid = session['session']['sessionId']
        result.update(attemptedPaidTurns=1, cumulativePaidTurns=args.previous_paid_turns + 1)
        save()
        result['turn'] = request(host, 'turn/start', {'commandId': command_id(), 'sessionId': sid, 'input': [{'type': 'text', 'text': prompt}]})
        deadline, cursor, handled, items = time.monotonic() + 180, 0, set(), {}
        while time.monotonic() < deadline:
            if cursor == len(host.events):
                host.events.append(host.receive(deadline))
            frame = host.events[cursor]
            cursor += 1
            method, params = frame.get('method'), frame.get('params', {})
            item = params.get('item', {})
            if method in ['item/started', 'item/updated', 'item/completed'] and item.get('kind') == 'toolCall':
                items[item['itemId']] = item
            if method == 'approval/requested':
                key = (params['approvalId'], json.dumps(params['currentRequirementId'], sort_keys=True))
                if key in handled:
                    continue
                handled.add(key)
                subject = params.get('subject', {})
                allow = subject.get('kind') == 'shell' and subject.get('command') == shell
                if subject.get('kind') == 'fileAccess':
                    target = Path(subject.get('path', ''))
                    allow = target in [workspace / name for name in ['file-created.txt', 'file-existing.txt', 'file-shell.txt','file-empty.txt']]
                choice = next(c for c in params['availableChoices'] if c['scope'] == 'once' and c['decision'] == ('approved' if allow else 'abort'))
                response = request(host, 'approval/decide', {'commandId': command_id(), 'sessionId': sid, 'approvalId': params['approvalId'], 'requirementId': params['currentRequirementId'], 'choiceId': choice['choiceId']})
                result['approvals'].append({'subject': subject, 'allowed': allow, 'response': response})
            if method == 'turn/completed':
                result['terminal'] = params
                break
        else:
            raise TimeoutError('The dummy turn did not complete')
        result['items'] = list(items.values())
        for item in result['items']:
            for field in ['patchRef', 'outputRef']:
                reference = item.get(field)
                if reference and reference.get('availability') == 'available':
                    result['outputs'][reference['id']] = request(host, 'item/readOutput', {'sessionId': sid, 'itemId': item['itemId'], 'outputRef': reference['id'], 'offsetBytes': 0, 'lengthBytes': 1048576})
        result['files'] = {name: (workspace / name).read_text() if (workspace / name).is_file() else None for name in ['file-created.txt', 'file-existing.txt', 'file-shell.txt','file-empty.txt']}
        save()
        print(json.dumps({'attemptedPaidTurns': 1, 'items': [{'tool': i.get('tool'), 'status': i.get('status'), 'patch': bool(i.get('patchRef'))} for i in result['items']], 'files': result['files']}))
    finally:
        if host is not None:
            host.close()
        link.unlink(missing_ok=True)
        save()
        shutil.rmtree(temporary)


if __name__ == '__main__':
    main()
