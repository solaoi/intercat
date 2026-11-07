"""Exercise explicit cache modes through the real local /decide endpoint."""
import argparse
import json
from pathlib import Path
import re
from threading import Thread
import urllib.request

from verify_editor import Engine, Scheduler, create_server, payload


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--model', required=True)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    source = Path('crates/intercat-daemon/src/decision.rs').read_text()
    prefix = json.loads(re.search(r'const PREFIX: &str = ("(?:[^"\\]|\\.)*");', source)[1])
    scheduler = Scheduler(lambda: Engine(args.model))
    scheduler.ready.result(timeout=120)
    server = create_server(scheduler)
    Thread(target=server.serve_forever, daemon=True).start()
    rows = []
    try:
        for index, (cached, changed) in enumerate([(False, False), (True, False), (True, False),
                                                    (False, False), (True, True), (True, False)]):
            value = payload(0, prefix + ('\nDifferent dummy policy context.\n' if changed else ''))
            value['stages'] = [stage for stage in value['stages'] if stage['name'] != 'skill']
            value['cachePrefix'] = cached
            count_request = urllib.request.Request(f'http://127.0.0.1:{server.server_port}/tokens',
                                                   data=json.dumps({'text': value['tail']}).encode(),
                                                   headers={'Content-Type': 'application/json'})
            with urllib.request.urlopen(count_request, timeout=120) as response:
                token_count = json.load(response)['tokens']
            request = urllib.request.Request(f'http://127.0.0.1:{server.server_port}/decide',
                                             data=json.dumps(value).encode(),
                                             headers={'Content-Type': 'application/json'})
            with urllib.request.urlopen(request, timeout=120) as response:
                assert response.getheader('X-Intercat-Inference') == 'started'
                result = json.load(response)
            assert result['cachePrefix'] == cached
            assert result['tailTokens'] == token_count
            assert result['prefixCacheHit'] == (index == 2)
            assert (result['prefixCacheTokens'] > 0) == cached
            rows.append({'index': index, 'request': value, 'tokenCount': token_count, 'result': result})
            (args.output / 'results.json').write_text(json.dumps(rows, ensure_ascii=False, indent=2) + '\n')
            print(json.dumps({key: result[key] for key in ['cachePrefix', 'prefixCacheHit',
                                                          'prefixCacheTokens', 'inferenceMs']}), flush=True)
        assert rows[0]['result']['stages'] == rows[3]['result']['stages']
        assert rows[1]['result']['stages'] == rows[2]['result']['stages'] == rows[5]['result']['stages']
        (args.output / 'verification.json').write_text(json.dumps({
            'passed': True, 'requests': 6, 'cacheHit': True, 'differentPrefixEvicts': True,
            'uncachedResultUnchanged': True, 'cachedResultRepeatable': True, 'paidTurns': 0,
        }, indent=2) + '\n')
    finally:
        server.shutdown()
        server.server_close()
        scheduler.close()


if __name__ == '__main__':
    main()
