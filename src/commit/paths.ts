export type PathBoundary={kind:'fork';checkpoint:number}|{kind:'entireTrack'};
export interface PathChoice {root:string;kept:string;retired:Record<string,PathBoundary>}
export interface PathFamily {
  root:string;
  members:{id:string;label:string;checkpoint:{number:number;sha:string}|null;available:boolean}[];
  forks:{kind:'branch'|'continuation';parent:string;child:string;checkpoint:{number:number;sha:string};origin:{endedAt:number|null;timelineAt:number|null;turnNumber:number}}[];
}
export interface PathReview {families:PathFamily[];choices:Record<string,PathChoice>}
export interface PathState {
  pending:string[];retired:Record<string,PathBoundary>;undoRecords:number;ready:boolean;
  mixed:{kind:'borrowedRetiredChanges'|'extendsCancelledContext';source:string;dependency:string;record:string;track:string;after:number;through:number}[];
  descendants:{track:string;included:boolean;parents:{track:string;checkpoint:{number:number;sha:string};onto:string}[]}[];
}
export interface Retirement {id:string;job:string;track:string;label:string;kind:'aside'|'leaveOut';phase:'stopping'|'saving'|'ready'|'aside'|'released';error:string|null}
