"""Generated loopback SSH agent/server. Never use personal SSH files."""
import json
import os
from pathlib import Path
import pwd
import signal
import shlex
import socket
import subprocess
import sys
import time

root=Path(sys.argv[1]).resolve()
root.mkdir(parents=True,exist_ok=True)
home=root/'home';home.mkdir()
client_home=root/'client-home';(client_home/'.ssh').mkdir(parents=True)
env={'HOME':str(home),'ZDOTDIR':str(home),'PATH':'/usr/bin:/bin:/usr/sbin:/sbin',
     'GIT_CONFIG_GLOBAL':'/dev/null','GIT_CONFIG_NOSYSTEM':'1'}
owned=[]
def run(args,extra=None):
    return subprocess.check_output(list(map(str,args)),env={**env,**(extra or {})},stderr=subprocess.PIPE,text=True).strip()
def start(args,name):
    stream=(root/name).open('wb')
    child=subprocess.Popen(list(map(str,args)),env=env,stdout=stream,stderr=stream,start_new_session=True)
    owned.append((child,stream));return child
try:
    for name in ['host','client']:
        run(['/usr/bin/ssh-keygen','-q','-t','ed25519','-N','','-f',root/name])
    agent=root/'agent.sock'
    process=start(['/usr/bin/ssh-agent','-D','-a',agent],'agent.log')
    for _ in range(100):
        if agent.exists():break
        if process.poll() is not None:raise RuntimeError('Dummy agent did not start')
        time.sleep(.02)
    run(['/usr/bin/ssh-add',root/'client'],{'SSH_AUTH_SOCK':str(agent)})
    with socket.socket() as listener:
        listener.bind(('127.0.0.1',0));port=listener.getsockname()[1]
    user=pwd.getpwuid(os.getuid()).pw_name
    config=root/'sshd.conf'
    config.write_text(f'Port {port}\nListenAddress 127.0.0.1\nHostKey {root/"host"}\n'
                      f'PidFile {root/"sshd.pid"}\nAuthorizedKeysFile {root/"client.pub"}\nAllowUsers {user}\n'
                      'PasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\n'
                      'StrictModes no\nLogLevel ERROR\nPermitUserRC no\nPermitUserEnvironment no\n'
                      f'SetEnv HOME={home} ZDOTDIR={home}\n')
    if len(sys.argv)>2:
        authentication=json.loads(Path(sys.argv[2]).read_text())
        handler=root/'lfs-command.py'
        remote=root/'remote.git'
        handler.write_text('import json,os,shlex,subprocess,sys\n'
                           'args=shlex.split(os.environ.get("SSH_ORIGINAL_COMMAND",""))\n'
                           f'remote={str(remote)!r}\n'
                           'if len(args)==3 and args[0]=="git-lfs-authenticate" and args[1]==remote and args[2]=="upload":\n'
                           f' print(json.dumps({authentication!r}));sys.exit(0)\n'
                           'if len(args)==2 and args[0] in ("git-upload-pack","git-receive-pack") and args[1]==remote:\n'
                           ' sys.exit(subprocess.call(["/usr/bin/git",args[0][4:],remote]))\n'
                           'sys.exit(2)\n')
        with config.open('a') as stream:stream.write('ForceCommand /usr/bin/python3 '+shlex.quote(str(handler))+'\n')
    run(['/usr/sbin/sshd','-t','-f',config])
    daemon=start(['/usr/sbin/sshd','-D','-e','-f',config],'sshd.log')
    for _ in range(100):
        if daemon.poll() is not None:raise RuntimeError('Dummy sshd did not start')
        try:
            with socket.create_connection(('127.0.0.1',port),timeout=.1):break
        except OSError:time.sleep(.02)
    public=(root/'host.pub').read_text().split()
    (client_home/'.ssh/known_hosts').write_text(f'[127.0.0.1]:{port} {public[0]} {public[1]}\n')
    (client_home/'.ssh/client.pub').write_text((root/'client.pub').read_text())
    (client_home/'.ssh/config').write_text(f'Host dummy-push-alias\n HostName 127.0.0.1\n Port {port}\n User {user}\n'
                                          f' IdentityFile {client_home/".ssh/client.pub"}\n IdentityAgent {agent}\n IdentitiesOnly yes\n')
    remote=root/'remote.git';run(['/usr/bin/git','init','--bare','-q',remote])
    print(json.dumps({'url':f'ssh://dummy-push-alias{remote}','agent':str(agent),'home':str(client_home),
                      'remote':str(remote),'port':port}),flush=True)
    sys.stdin.readline()
finally:
    for child,stream in reversed(owned):
        if child.poll() is None:
            os.killpg(child.pid,signal.SIGTERM)
            try:child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid,signal.SIGKILL);child.wait(timeout=5)
        stream.close()
