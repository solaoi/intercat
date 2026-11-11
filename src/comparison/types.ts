import type {Change} from '../commit/types';
export interface Point{trackId:string;sha:string}
export interface Open{opId:string;parent:Point;child:Point}
export interface Control{opId:string;id:string;revision:number;attempt:number;action:'hold'|'retry'}
export interface Check{tree:string;settingsHash:string;command:string|null;code:number|null;stdout:string;stderr:string;truncated:boolean}
export interface Outcome{state:'passed'|'env-failed'|'dirty-before-check'|'cancelled'|'history-required'|'failed'|'mutated';check:Check|null;reason:string|null;dependencyRuns:{stdout:string;stderr:string}[];dependencyError:{message:string;output:string|null}|null}
export interface Side{source:{point:Point;label:string;checkpoint:{sha:string;turnId:string}};tree:string;settingsHash:string;changes:Change[];outcome:Outcome|null;cached:boolean}
export interface Comparison{ id:string;revision:number;attempt:number;request:Open;session:string;folder:{id:string;name:string};settingsHash:string;reuseChecks:boolean;phase:'ready'|'running'|'held'|'done';sides:[Side,Side];summary:{label:string;reason:string|null}}
export interface EvidenceSide{tests:{added:number;deleted:number;modified:number};skipAdded:number;runner:string[];unreadable:string[];commandUnparsed:boolean}
export interface Evidence{id:string;sides:[EvidenceSide,EvidenceSide];changes:Change[]}
export interface Selection{parent:Point;child:Point;jobId?:string}
export interface DiffRequest{id:string;basis:'parent'|'child'|'paths';path:string}
