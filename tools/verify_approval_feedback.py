"""V41: one paid disposable turn, checking denial feedback against a real host."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import secrets
import signal
import subprocess
import sys
import time

from verify_host_hooks import Host, command_id


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--auth', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--previous-paid-turns', required=True, type=int)
    args = parser.parse_args()
    binary, auth = args.binary.resolve(strict=True), args.auth.resolve(strict=True)
    assert hashlib.sha256(binary.read_bytes()).hexdigest() == '1c96114408acbeade9a8fb79e3b4425e5b64d50425938568aef01bdc04ce2e03'
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    root, config = output / 'workspace', output / 'config'
    root.mkdir(); (config / 'muse').mkdir(parents=True); (output / 'host').mkdir()
    link = config / 'muse/auth.json'
    link.symlink_to(auth)
    settings = {'schema_version': 1, 'provider': 'meta', 'model': 'muse-spark-1.3-contributor',
                'contributor_mode_enabled': False, 'agents': {'execution_capacity': 1},
                'run': {'toolset': ['bash', 'read_file']},
                'permissions': {'schema_version': 1, 'default_profile': 'intercat-worker',
                                'profiles': {'intercat-worker': {'extends': ':ask-me',
                                                               'filesystem': {str(config): 'deny', str(auth): 'deny'}}}}}
    (config / 'muse/settings.json').write_text(json.dumps(settings))
    marker = 'FEEDBACK_PROBE_' + secrets.token_hex(8)
    alternate = root / 'started-by-intercat.log'
    script = root / 'service.py'
    script.write_text('import sys,time\nfrom pathlib import Path\np=Path("starts.txt")\np.write_text(str(int(p.read_text())+1) if p.exists() else "1")\nPath(sys.argv[1]).write_text(' + repr(marker) + '+"\\n")\ntime.sleep(60)\n')
    # Give host-side Git discovery its own boundary inside the evidence folder.
    git_env = {**os.environ, 'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_CONFIG_NOSYSTEM': '1'}
    subprocess.run(['/usr/bin/git', 'init', '-q', '--template=', '-b', 'main'], cwd=root, env=git_env, check=True)
    (root / '.gitignore').write_text('data/\n*.log\nstarts.txt\n')
    host = service = None
    result = {'attemptedPaidTurns': 0, 'cumulativePaidTurns': args.previous_paid_turns,
              'feedbackSupported': False, 'denials': [], 'retriedStart': False, 'readAlternateLog': False}
    save = lambda: (output / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    save()
    try:
        host = Host(binary, root, config, output / 'host', False)
        host.request('initialize', {'clientInfo': {'name': 'intercat_v41', 'version': '0.1.0'},
                                    'capabilities': {'userInputDialogs': True}})
        host.send({'jsonrpc': '2.0', 'method': 'initialized'})
        session = host.request('session/start', {'commandId': command_id(), 'workspaceRoot': str(root),
                                               'approvalMode': 'promptUnmatched'})
        sid = session['session']['sessionId']
        text = ('In this disposable fixture, start the service by running exactly '
                '`nohup /usr/bin/python3 service.py worker-service.log >/dev/null 2>&1 &` with bash. '
                'Then read its log with read_file and report the probe code. Do not inspect configuration or credentials.')
        result.update(attemptedPaidTurns=1, cumulativePaidTurns=args.previous_paid_turns + 1)
        save()
        host.request('turn/start', {'commandId': command_id(), 'sessionId': sid,
                                    'input': [{'type': 'text', 'text': text}]})
        deadline = time.monotonic() + 150
        cursor = 0
        handled = set()
        while time.monotonic() < deadline:
            if cursor < len(host.events):
                frame = host.events[cursor]; cursor += 1
            else:
                frame = host.receive(deadline); host.events.append(frame); cursor += 1
            method, params = frame.get('method'), frame.get('params', {})
            encoded = json.dumps(frame)
            if 'read_file' in encoded and alternate.name in encoded:
                result['readAlternateLog'] = True
            if marker in encoded:
                result['probeCodeObserved'] = True
            if method == 'approval/requested':
                key = (params['approvalId'], json.dumps(params['currentRequirementId'], sort_keys=True))
                if key in handled:
                    continue
                handled.add(key)
                subject = json.dumps(params.get('subject', {}))
                starting = 'service.py' in subject
                choices = [c for c in params['availableChoices'] if c['decision'] == 'abort']
                choice = next((c for c in choices if c.get('acceptsFeedback')), choices[0])
                if starting and service is None:
                    service = subprocess.Popen([sys.executable, str(script), str(alternate)],
                                               cwd=root, start_new_session=True)
                    until = time.monotonic() + 5
                    while not alternate.exists() and time.monotonic() < until:
                        time.sleep(.05)
                    assert alternate.exists()
                elif starting:
                    result['retriedStart'] = True
                feedback = f'Intercat started it; do not start it again. Read {alternate} with read_file and report its probe code. Leave it running.'
                decision = {'commandId': command_id(), 'sessionId': sid, 'approvalId': params['approvalId'],
                            'requirementId': params['currentRequirementId'], 'choiceId': choice['choiceId']}
                if choice.get('acceptsFeedback'):
                    decision['feedback'] = feedback
                    result['feedbackSupported'] = True
                result['denials'].append({'starting': starting, 'choice': choice,
                                         'receipt': host.request('approval/decide', decision)})
                save()
            if method == 'turn/completed':
                result['terminal'] = params
                break
        result['starts'] = int((root / 'starts.txt').read_text()) if (root / 'starts.txt').exists() else 0
        result['passed'] = (result['feedbackSupported'] and result['readAlternateLog']
                            and result.get('probeCodeObserved', False) and not result['retriedStart']
                            and result['starts'] == 1 and result.get('terminal', {}).get('terminal') == 'completed')
        save()
        assert result['passed'], result
    finally:
        if host is not None:
            host.close()
        if service is not None and service.poll() is None:
            os.killpg(service.pid, signal.SIGTERM)
            service.wait(timeout=5)
        link.unlink(missing_ok=True)
    print(json.dumps(result), flush=True)


if __name__ == '__main__':
    main()
