"""Build an offline wire fixture from the pinned schema and recorded frames."""
import json
from pathlib import Path
import sqlite3
import sys

from audit_msp_corpus import audit

SESSION = '01900000-0000-7000-8000-000000000001'
COMMAND = '01900000-0000-7000-8000-000000000002'


def build(repository):
    inventory = audit(repository)
    schema = json.loads((repository / 'research/verify/support/schema-stable/msp.schema.json').read_text())

    def sample(definition, name='', depth=0):
        if depth > 40:
            raise ValueError('Recursive required schema: ' + name)
        if '$ref' in definition:
            return sample(schema['$defs'][definition['$ref'].split('/')[-1]], name, depth + 1)
        if 'const' in definition:
            return definition['const']
        if 'enum' in definition:
            return definition['enum'][0]
        for union in ['anyOf', 'oneOf']:
            if union in definition:
                return sample(definition[union][0], name, depth + 1)
        if 'allOf' in definition:
            merged = {}
            for part in definition['allOf']:
                merged.update(sample(part, name, depth + 1))
            return merged
        kind = definition.get('type', 'object')
        if isinstance(kind, list):
            kind = kind[0]
        if kind == 'object':
            return {key: sample(definition['properties'][key], key, depth + 1) for key in definition.get('required', [])}
        if kind == 'array':
            return [sample(definition['items'], name, depth + 1) for _ in range(definition.get('minItems', 0))]
        if kind == 'null':
            return None
        if kind == 'boolean':
            return False
        if kind in ['integer', 'number']:
            return definition.get('minimum', 0)
        if kind == 'string':
            if name == 'commandId':
                return COMMAND
            if name in ['sessionId', 'parentSessionId']:
                return SESSION
            if name in ['turnId', 'expectedTurnId']:
                return COMMAND
            if name == 'workspaceRoot':
                return '/private/tmp/intercat-msp-fixture'
            return 'fixture-' + (name or 'text')
        raise ValueError('Unknown schema type: ' + str(kind))

    validation = []
    def validate(definition, value):
        if '$ref' in definition:
            validation.append({'name': definition['$ref'].split('/')[-1], 'value': value})

    calls = []
    for index, method in enumerate(inventory['requiredClientMethods']):
        if method not in schema['methods']:
            continue
        entry = schema['methods'][method]
        params = sample(entry.get('params', {}))
        result = sample(entry.get('result', {}))
        if 'commandId' in params:
            params['commandId'] = f'01900000-0000-7000-8000-{index + 10:012x}'
            if 'commandId' in result:
                result['commandId'] = params['commandId']
        if method == 'initialize':
            result['schema']['fingerprint'] = 'sha256:c94c3d1f2f51e6424074b9ec43eac3eedcf532d3dcd77351ec1383a87d3049c6'
        validate(entry.get('params', {}), params)
        validate(entry.get('result', {}), result)
        calls.append({'method': method, 'params': params, 'result': result})

    def inbound(kind, method, entry):
        recorded = inventory['samples'].get(kind + ':' + method, [])
        if recorded:
            source = recorded[0]
            path = repository / source['path']
            if 'line' in source:
                frame = json.loads(path.read_text().splitlines()[source['line'] - 1])
            else:
                with sqlite3.connect(path.as_uri() + '?mode=ro', uri=True) as database:
                    frame = json.loads(database.execute('SELECT payload FROM events WHERE seq=?', (source['sequence'],)).fetchone()[0])
            if isinstance(frame.get('frame'), dict):
                frame = frame['frame']
            origin = source
        else:
            frame = {'jsonrpc': '2.0', 'method': method, 'params': sample(entry.get('params', {}))}
            origin = {'schema': method}
        if 'params' in frame:
            validate(entry.get('params', {}), frame['params'])
        return {'frame': frame, 'origin': origin}

    notifications = [inbound('notification', method, entry) for method, entry in schema['notifications'].items()]
    notifications.extend(inbound('notification', method, {}) for method in inventory['otherRecordedNotifications'])
    requests = [inbound('request', method, entry) for method, entry in schema['requests'].items()]
    for index, entry in enumerate(requests):
        entry['frame']['id'] = 1001 + index
    return {'calls': calls, 'notifications': notifications, 'requests': requests, 'validation': validation,
            'notify': {'method': 'userInput/engaged', 'params': sample(schema['notifications']['userInput/engaged']['params'])},
            'inventory': inventory}


if __name__ == '__main__':
    target = Path(sys.argv[1])
    target.parent.mkdir(parents=True, exist_ok=True)
    value = build(Path(__file__).resolve().parents[1])
    target.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({'calls': len(value['calls']), 'notifications': len(value['notifications']), 'requests': len(value['requests'])}))
