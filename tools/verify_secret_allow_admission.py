"""Check exact-path exceptions against the pinned host without a model turn."""
import argparse
import json
from pathlib import Path

from verify_host_hooks import Host, command_id


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    results = []
    for access in ('read', 'write'):
        folder = output / access
        root = folder / 'workspace'
        config = folder / 'config'
        (config / 'muse').mkdir(parents=True)
        root.mkdir()
        (root / '.env').write_text('DUMMY_ONLY=1\n')
        example = root / '.env.example'
        example.write_text('DUMMY_EXAMPLE=1\n')
        rules = {':workspace_roots': {'**/.env': 'deny', '**/.env.*': 'deny'}, str(example): access}
        settings = {'schema_version': 1, 'provider': 'echo', 'permissions': {
            'schema_version': 1, 'default_profile': 'intercat-worker', 'profiles': {
                'intercat-worker': {'extends': ':ask-me', 'filesystem': rules}}}}
        (config / 'muse/settings.json').write_text(json.dumps(settings, indent=2))
        host = Host(args.binary.resolve(), root, config, folder, False)
        try:
            host.request('initialize', {'clientInfo': {'name': 'exact_exception_probe', 'version': '1'}, 'capabilities': {'experimentalApi': True}})
            host.send({'jsonrpc': '2.0', 'method': 'initialized'})
            session = host.request('session/start', {'commandId': command_id(), 'workspaceRoot': str(root), 'approvalMode': 'promptUnmatched', 'providerId': 'echo'})
            result = {'access': access, 'accepted': True, 'profile': session['session']['permissionState']['profileId'], 'paidTurns': 0}
        except Exception as error:
            result = {'access': access, 'accepted': False, 'reason': str(error), 'paidTurns': 0}
        finally:
            host.close()
        results.append(result)
        print(json.dumps(result), flush=True)
    (output / 'result.json').write_text(json.dumps(results, indent=2) + '\n')


if __name__ == '__main__':
    main()
