export interface Begin {opId:string;id:string;revision:number}
export interface Hunk {startLine:number;endLine:number;ours:string;theirs:string}
export interface Version {object:string;mode:number;text:string|null}
export interface FileView {sides:{path:string;target:Version|null;incoming:Version|null};version:string;text:string|null;editable:boolean}
export interface View {id:string;revision:number;paths:string[];file:FileView}
export type Edit = {kind:'text';path:string;version:string;text:string}|{kind:'side';path:string;version:string;side:'target'|'incoming'}|{kind:'current';path:string;version:string};
export interface Apply extends Begin {files:Edit[]}
export function hasMarkers(text:string,id:string) {
  return text.split('\n').some(line => new RegExp(`^(?:<{7,255}|\\|{7,255}|>{7,255}) Intercat (target|base|change) ${id}(?:$|[\\r :\\t])`).test(line));
}
export function hunks(text:string,id:string): Hunk[] {
  const lines = text.match(/[^\n]*\n|[^\n]+$/g) ?? [];
  const result:Hunk[] = [];
  const mark = (line:string|undefined,role:string) => !!line && new RegExp(`^([<|>])\\1{6,254} Intercat (?:${role}) ${id}(?:$|[ :\\t])`).test(line.replace(/[\r\n]+$/,''));
  for (let i=0;i<lines.length;i+=1) {
    if (!mark(lines[i],'target')) continue;
    const start=i, size=lines[i].match(/^<+/)?.[0].length ?? 7;
    const separator = (line:string) => line.replace(/[\r\n]+$/,'') === '='.repeat(size);
    i+=1;
    const ours=i;
    while (i<lines.length && !mark(lines[i],'base') && !separator(lines[i]) && !mark(lines[i],'target|change')) i+=1;
    const oursEnd=i;
    if (mark(lines[i],'base')) {i+=1;while (i<lines.length && !separator(lines[i]) && !mark(lines[i],'target|change')) i+=1;}
    if (i===lines.length || !separator(lines[i])) throw new Error('The conflict separator changed. Edit the result directly.');
    i+=1;const theirs=i;
    while(i<lines.length && !mark(lines[i],'change')) {
      if (separator(lines[i]) || mark(lines[i],'target|base')) throw new Error('The conflict contains ambiguous markers. Edit the result directly.');
      i+=1;
    }
    if(i===lines.length) throw new Error('The conflict end marker changed. Edit the result directly.');
    result.push({startLine:start+1,endLine:i+1,ours:lines.slice(ours,oursEnd).join(''),theirs:lines.slice(theirs,i).join('')});
  }
  return result;
}
