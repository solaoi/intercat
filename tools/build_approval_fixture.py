"""An offline, two-stage approval using the pinned recorded wire shapes."""
import copy
import json
from pathlib import Path
import sys

root = Path(__file__).resolve().parents[1]
output = Path(sys.argv[1]).resolve()
output.parent.mkdir(parents=True, exist_ok=True)
workspace = output.parent / 'workspace'
workspace.mkdir(exist_ok=True)
transport = json.loads((root / 'research/verify/msp-oracle-20261104/transport-fixture.json').read_text())
request = copy.deepcopy(next(x['frame']['params'] for x in transport['notifications']
                            if x['frame']['method'] == 'approval/requested'))
session = '01900000-0000-7000-8000-000000000001'
turn = '01900000-0000-7000-8000-000000000002'
approval = '01900000-0000-7000-8000-000000000003'
request.update(sessionId=session, turnId=turn, approvalId=approval, viewCursor='request-0')
request['currentRequirementId'] = {'approvalId': approval, 'sourceIndex': 0}
request['sourceRange']['stream']['id'] = session
request['protectedWrite'] = False
request['judgeEscalated'] = False
request['availableChoices'] = [
    {'choiceId': 'allow_once', 'label': 'Allow once', 'decision': 'approved', 'scope': 'once'},
    {'choiceId': 'abort', 'label': 'Reject', 'decision': 'abort', 'scope': 'once', 'acceptsFeedback': True},
]
request['subject']['workspaceRoot'] = str(workspace)
request['subject']['command'] = 'echo stage0 && echo stage1'
stage = request['subject']['stages'][0]
request['subject']['stages'] = []
for i in range(2):
    current = copy.deepcopy(stage)
    current.update(requirementId={'approvalId': approval, 'sourceIndex': i}, position=i,
                   totalStages=2, argv=['echo', f'stage{i}'], argvComplete=True,
                   resolution={'kind': 'unresolved'},
                   suggestedPrefix={'argvPrefix': ['echo'], 'label': 'Allow echo in this workspace'})
    request['subject']['stages'].append(current)
request['rawArgs'] = json.dumps({'command': request['subject']['command']})
update = {key: copy.deepcopy(request[key]) for key in ['approvalId', 'availableChoices',
          'currentRequirementId', 'sessionId', 'sourceRange', 'subject', 'viewCursor']}
update['currentRequirementId']['sourceIndex'] = 1
update['viewCursor'] = 'request-1'
update['subject']['stages'][0]['resolution'] = {'kind': 'allowOnce'}
update['change'] = {'kind': 'stageResolved', 'requirementId': request['currentRequirementId'],
                    'choiceId': 'allow_once', 'decision': 'approved'}
output.write_text(json.dumps({'session': session, 'turn': turn, 'workspace': str(workspace),
                             'terminalAt': 2, 'updateBeforeAck': False, 'duplicateRequest': False,
                             'serverRequest': False,
                             'request': request, 'update': update}, ensure_ascii=False, indent=2) + '\n')
