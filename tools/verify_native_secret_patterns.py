"""Probe native glob handling with offline userShell; no provider/model calls."""
import argparse
import json
from pathlib import Path
import time

from verify_host_hooks import Host, command_id


def partition_rules():
    rules = {'**/.env': 'deny', '**/.envrc': 'deny', '**/.netrc': 'deny', '**/*.key': 'deny', '**/*.pem': 'deny'}
    words = ['dist', 'example', 'sample', 'template']
    def visit(prefix):
        if prefix not in words:
            rules['**/.env.' + prefix] = 'deny'
        letters = sorted({word[len(prefix)] for word in words if word.startswith(prefix) and len(word) > len(prefix)})
        if not letters:
            rules['**/.env.' + prefix + '?*'] = 'deny'
            return
        low, high = letters[0], letters[-1]
        rules['**/.env.' + prefix + '[!' + (low if low == high else low + '-' + high) + ']*'] = 'deny'
        for left, right in zip(letters, letters[1:]):
            start, end = ord(left) + 1, ord(right) - 1
            if start <= end:
                rules['**/.env.' + prefix + '[' + (chr(start) if start == end else chr(start) + '-' + chr(end)) + ']*'] = 'deny'
        for letter in letters:
            visit(prefix + letter)
    visit('')
    return rules


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--case', action='append')
    parser.add_argument('--wide', action='store_true')
    parser.add_argument('--rules', type=Path, help='Use the rules printed by the production secret_policy example')
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    names = ['plain.txt', '.env', '.env.', '.env.d', '.env.dist', '.env.e', '.env.example',
             '.env.sample', '.env.template', '.env.production', '.env.custom', '.env.!test',
             '.env.^test', '.env.a', '.env.z', '.env.é', '.env.Ω', '.env.本番', '.env.😀', '.env.\ue000']
    if args.wide:
        names += ['.envrc', '.netrc', 'cert.key', 'cert.pem']
        for word in ['dist', 'example', 'sample', 'template']:
            for index in range(len(word)):
                for char in ['a', 'z', '0', '-', 'é', 'e\u0301', 'đ', 'ð', 'ß', 'ı', 'Ω', '本', '😀', '\ue000', '\U0010fffd', '\n']:
                    names.append('.env.' + word[:index] + char + word[index+1:])
            names += ['.env.' + word + suffix for suffix in ['X', '.local', 'é', '\u0301', '本', '😀', '\n']]
            names += ['.env.' + word[:index] for index in range(len(word))]
        names = sorted(set(names))
    cases = {
        'baseline': {},
        'whole': {'**/.env': 'deny', '**/.env.*': 'deny'},
        'bang_one': {'**/.env.[!d]*': 'deny'},
        'bang_four': {'**/.env.[!dest]*': 'deny'},
        'ascii_range': {'**/.env.[a-cf-ru-z]*': 'deny'},
        'latin_range': {'**/.env.[\u0080-\u00ff]*': 'deny'},
        'bmp_range': {'**/.env.[\u0100-\ud7ff]*': 'deny'},
        'unicode_literal': {'**/.env.[Ω本😀]*': 'deny'},
        'unicode_negated': {'**/.env.[!Ω]*': 'deny'},
        'negative_range': {'**/.env.[!d-t]*': 'deny'},
        'range_complement': {'**/.env.[!d-t]*': 'deny', '**/.env.[f-r]*': 'deny'},
        'partition_single_range': partition_rules(),
    }
    if args.rules:
        cases = {'production': json.loads(args.rules.read_text())}
    elif args.case:
        cases = {name: cases[name] for name in args.case}
    results = []
    for name, rules in cases.items():
        folder = output / name
        workspace = folder / 'workspace'
        config = folder / 'config'
        (config / 'muse').mkdir(parents=True)
        workspace.mkdir()
        for prefix in ('', 'nested/deep/'):
            (workspace / prefix).mkdir(parents=True, exist_ok=True)
            for file in names:
                (workspace / prefix / file).write_text('DUMMY_ONLY\n')
        script = "import json\nfrom pathlib import Path\nresult=[]\n"
        script += f"for prefix in ['', 'nested/deep/', 'new/']:\n Path(prefix or '.').mkdir(parents=True,exist_ok=True)\n for name in {names!r}:\n  path=Path(prefix+name);row={{'path':str(path)}}\n  for action in ['read','write','readAfterWrite']:\n   try:\n    if action=='write':path.write_text('DUMMY_ONLY\\n')\n    else:path.read_bytes()\n    row[action]=True\n   except OSError:row[action]=False\n  result.append(row)\n"
        script += "Path('results.json').write_text(json.dumps(result)); print('probe finished')\n"
        (workspace / 'probe.py').write_text(script)
        settings = {'schema_version': 1, 'provider': 'echo', 'permissions': {'schema_version': 1, 'default_profile': 'probe', 'profiles': {'probe': {'extends': ':ask-me', 'filesystem': {':workspace_roots': rules}}}}}
        (config / 'muse/settings.json').write_text(json.dumps(settings))
        host = Host(args.binary.resolve(), workspace, config, folder, False)
        row = {'case': name, 'paidTurns': 0}
        try:
            hello = host.request('initialize', {'clientInfo': {'name': 'native_pattern_probe', 'version': '1'}, 'capabilities': {'experimentalApi': True, 'requestedCapabilities': ['userShell']}})
            assert 'userShell' in hello['grantedCapabilities']
            host.send({'jsonrpc': '2.0', 'method': 'initialized'})
            session = host.request('session/start', {'commandId': command_id(), 'workspaceRoot': str(workspace), 'approvalMode': 'promptUnmatched', 'providerId': 'echo'})['session']
            command = command_id()
            host.request('session/userShell', {'sessionId': session['sessionId'], 'commandId': command, 'commandText': '/usr/bin/python3 probe.py'})
            deadline = time.monotonic() + 15
            while True:
                frame = host.receive(deadline)
                item = frame.get('params', {}).get('item', {})
                if frame.get('method') == 'item/completed' and item.get('commandId') == command:
                    row['shell'] = item
                    break
            path = workspace / 'results.json'
            row['access'] = json.loads(path.read_text()) if path.exists() else None
        except Exception as error:
            row['error'] = str(error)
        finally:
            host.close()
        results.append(row)
        print(json.dumps({'case': name, 'rows': len(row.get('access') or []), 'error': row.get('error'), 'exitCode': row.get('shell', {}).get('exitCode')}), flush=True)
    (output / 'results.json').write_text(json.dumps(results, indent=2) + '\n')


if __name__ == '__main__':
    main()
