import fs from 'node:fs/promises';
import {constants} from 'node:fs';
import path from 'node:path';
import {createHash} from 'node:crypto';

export async function applyMergeInput({directory,client,request}) {
  if(!request||typeof request.opId!=='string'||!/^[a-f0-9]{8}-[a-f0-9]{4}-7[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/.test(request.opId)||!Array.isArray(request.files))throw new Error('Invalid manual resolution');
  // Keep the canonical field order used by the domain snapshot, so retries
  // bind to the same digest independently of the IPC caller's property order.
  const files=request.files.map(file=>{
    const {kind,path,version}=file;
    if(kind==='text')return {kind,path,version,text:file.text};
    if(kind==='side')return {kind,path,version,side:file.side};
    if(kind==='current')return {kind,path,version};
    throw new Error('Invalid conflict file choice');
  });
  const bytes=Buffer.from(JSON.stringify({opId:request.opId,id:request.id,revision:request.revision,files}));
  if(bytes.length>8*1024*1024)throw new Error('The manual resolution exceeds 8 MiB');
  await fs.mkdir(directory,{recursive:true,mode:0o700});
  const metadata=await fs.lstat(directory);
  if(!metadata.isDirectory()||metadata.uid!==process.getuid()||metadata.mode&0o077)throw new Error('Unsafe manual resolution directory');
  const box=await fs.mkdtemp(path.join(directory,request.opId+'-'));
  try {
    const file=await fs.open(path.join(box,'input.json'),constants.O_WRONLY|constants.O_CREAT|constants.O_EXCL|constants.O_NOFOLLOW,0o600);
    try {await file.writeFile(bytes);await file.sync();}finally{await file.close();}
    const folder=await fs.open(box,constants.O_RDONLY|constants.O_DIRECTORY|constants.O_NOFOLLOW);
    try {await folder.sync();}finally{await folder.close();}
    return await (await client()).request('merge.apply',{opId:request.opId,id:request.id,revision:request.revision,digest:createHash('sha256').update(bytes).digest('hex'),input:path.basename(box)});
  }finally{await fs.rm(box,{recursive:true,force:true});}
}
