"""Small, deterministic delivery schedules for the real gap recovery paths."""
import json
from pathlib import Path
import sys

SESSION = '01900000-0000-7000-8000-000000000001'


def build():
    def name(cursor, value):
        return {'jsonrpc':'2.0', 'method':'session/nameChanged', 'params':{
            'sessionId':SESSION, 'viewCursor':cursor, 'sourceRange':{
                'stream':{'kind':'session','id':SESSION},
                'first':{'id':cursor,'sequence':1}, 'last':{'id':cursor,'sequence':1}}, 'name':value}}
    history = [name('opaque:z', 'initial'), name('opaque:a', 'middle'),
               name('opaque:q', 'latest'), name('opaque:b', 'extended')]
    def gap(target):
        return {'jsonrpc':'2.0','method':'view/gap','params':{
            'sessionId':SESSION,'after':'opaque:z','next':target}}
    def case(identifier, **options):
        return {'id':identifier, 'sessionId':SESSION, 'initial':[history[0]],
                'gap':gap('opaque:q'), 'history':history[:3], 'pageSize':1,
                'during':[], 'tail':[], **options}
    return {'cases':[
        case('buffered-live-overlap', during=[history[2]]),
        case('late-gap', initial=[history[0], history[2]]),
        case('late-twin', tail=[history[2]]),
        case('coalesced-gap', history=history, pageSize=2,
             gap=gap('opaque:a'), during=[history[1],gap('opaque:b'),history[3]]),
        case('ephemeral-target', gap=gap('ephemeral:tail'), during=[{
            'jsonrpc':'2.0','method':'session/statusChanged',
            'params':{'sessionId':SESSION,'viewCursor':'ephemeral:tail','status':'idle'}}]),
        case('stalled-page-releases-live', during=[history[2]], fault='stall', faultCursor='opaque:z'),
        case('foreign-page-releases-live', during=[history[2]], fault='foreign', faultCursor='opaque:z'),
        case('failed-second-page-releases-live', during=[history[2]], fault='error', faultCursor='opaque:a'),
    ]}


if __name__ == '__main__':
    Path(sys.argv[1]).write_text(json.dumps(build(),ensure_ascii=False,indent=2)+'\n')
