"""Generated authenticated LFS basic-transfer service, loopback only."""
import base64
import hashlib
import http.server
import json
from pathlib import Path
import ssl
import subprocess
import sys
import threading

root=Path(sys.argv[1]).resolve();root.mkdir(parents=True,exist_ok=True)
objects=root/'objects';objects.mkdir()
home=root/'home';home.mkdir()
env={'HOME':str(home),'PATH':'/usr/bin:/bin:/usr/sbin:/sbin','GIT_CONFIG_GLOBAL':'/dev/null','GIT_CONFIG_NOSYSTEM':'1'}
cert,key=root/'dummy.crt',root/'dummy.key'
subprocess.run(['/usr/bin/openssl','req','-x509','-newkey','rsa:2048','-nodes','-days','1','-keyout',key,'-out',cert,
                '-subj','/CN=dummy-lfs.localhost','-addext','subjectAltName=DNS:dummy-lfs.localhost,DNS:dummy-storage.localhost,IP:127.0.0.1'],env=env,check=True,
               stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
key.chmod(0o600)
expected='Basic '+base64.b64encode(b'dummy:dummy-local-only').decode()
storage_auth='Bearer dummy-storage-local-only'
events=[]
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self,*args):pass
    def reply(self,status,value):
        data=json.dumps(value).encode();self.send_response(status)
        self.send_header('Content-Type','application/vnd.git-lfs+json');self.send_header('Content-Length',str(len(data)))
        self.end_headers();self.wfile.write(data)
    def authorized(self):
        required=storage_auth if self.server.storage else expected
        if self.headers.get('Authorization')==required:return True
        self.send_response(401);self.send_header('WWW-Authenticate','Basic realm="Intercat dummy LFS"');self.send_header('Content-Length','0');self.end_headers();return False
    def do_POST(self):
        if not self.authorized():return
        body=json.loads(self.rfile.read(int(self.headers.get('Content-Length',0))))
        if self.path=='/lfs/objects/batch':
            if self.server.storage:self.reply(404,{});return
            if body.get('operation')!='upload':self.reply(400,{});return
            result=[]
            for obj in body.get('objects',[]):
                oid=obj['oid'];size=obj['size']
                if len(oid)!=64 or any(char not in '0123456789abcdef' for char in oid):self.reply(400,{});return
                item={'oid':oid,'size':size,'authenticated':True}
                if not (objects/oid).exists():
                    item['actions']={'upload':{'href':storage_url+'/objects/'+oid,'header':{'Authorization':storage_auth}},
                                     'verify':{'href':storage_url+'/verify','header':{'Authorization':storage_auth}}}
                result.append(item)
            events.append('batch');self.reply(200,{'transfer':'basic','objects':result});return
        if self.path=='/lfs/verify':
            if not self.server.storage:self.reply(404,{});return
            data=(objects/body['oid']).read_bytes()
            if len(data)!=body['size'] or hashlib.sha256(data).hexdigest()!=body['oid']:self.reply(422,{});return
            events.append('verify');self.reply(200,{});return
        self.reply(404,{})
    def do_PUT(self):
        if not self.authorized():return
        if not self.server.storage:self.reply(404,{});return
        oid=self.path.removeprefix('/lfs/objects/')
        data=self.rfile.read(int(self.headers.get('Content-Length',0)))
        if len(oid)!=64 or hashlib.sha256(data).hexdigest()!=oid:self.reply(422,{});return
        (objects/oid).write_bytes(data);events.append('upload');self.reply(200,{})
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler)
storage=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler)
server.storage=False;storage.storage=True
context=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER);context.load_cert_chain(cert,key)
server.socket=context.wrap_socket(server.socket,server_side=True)
storage.socket=context.wrap_socket(storage.socket,server_side=True)
url=f'https://dummy-lfs.localhost:{server.server_port}/lfs'
storage_url=f'https://dummy-storage.localhost:{storage.server_port}/lfs'
thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
storage_thread=threading.Thread(target=storage.serve_forever,daemon=True);storage_thread.start()
print(json.dumps({'url':url,'ca':str(cert),'objects':str(objects),'events':str(root/'events.json')}),flush=True)
try:sys.stdin.readline()
finally:
    server.shutdown();server.server_close();storage.shutdown();storage.server_close();(root/'events.json').write_text(json.dumps(events))
