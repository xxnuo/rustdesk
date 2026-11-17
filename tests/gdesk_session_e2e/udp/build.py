from pathlib import Path
import hashlib
import os
import re
import shutil
import subprocess
import sys

source=Path(__file__).resolve().parent
root=source.parents[2]
work=root/'.tmp/gdesk-session-e2e/udp'
work.mkdir(parents=True,exist_ok=True)
manifest=(root/'Cargo.toml').read_text()
manifest=re.sub(r'\[lib\].*?(?=\n\[)','',manifest,flags=re.S)
manifest=re.sub(r'\[\[bin\]\].*?(?=\n\[)','',manifest,flags=re.S)
manifest=re.sub(r'\[workspace\].*?(?=\n\[)','[workspace]\n',manifest,flags=re.S)
manifest=manifest.replace('name = "rustdesk"','name = "rfc2-udp-e2e"',1).replace('default-run = "rustdesk"','autobins = false').replace('build= "build.rs"','build = false')
manifest=re.sub(r'path\s*=\s*"(libs/[^\"]+)"',lambda m:'path = "'+str(root/m[1])+'"',manifest)
manifest+='\n[[bin]]\nname = "udp-session-e2e"\npath = "session.rs"\n'
(work/'Cargo.toml').write_text(manifest)
shutil.copyfile(root/'Cargo.lock',work/'Cargo.lock')
(work/'session.rs').write_text((source/'observer.rs').read_text()+(source/'runtime.rs').read_text())
sources=['Cargo.toml','Cargo.lock','libs/gdesk/src/sched.rs','src/server/video_service.rs','src/lib.rs','src/client.rs','src/client/io_loop.rs','src/server/connection.rs','src/gdesk_bulk.rs',
    'src/rendezvous_mediator.rs','src/kcp_stream.rs','src/gdesk_session.rs','src/gdesk_datagram.rs',
    'libs/gdesk/src/udp.rs','libs/scrap/src/common/gdesk_codec.rs','libs/base/protos/message.proto']
def digest():return ''.join(f'{hashlib.sha256((root/p).read_bytes()).hexdigest()}  {p}\n' for p in sources)
before=digest();(work/'build-source.sha256').write_text(before)
if '--prepare-only' in sys.argv:
    print('Prepared:', work)
    raise SystemExit(0)
env=dict(os.environ)
env.setdefault('CXXFLAGS','-include cstdint')
command=['cargo','build','--offline','--manifest-path',str(work/'Cargo.toml'),'--target-dir',str(work/'target'),
    '--features','gdesk','--bin','udp-session-e2e']
print('Command:', ' '.join(command),flush=True)
with open(work/'build.log','wb') as log:
    result=subprocess.run(command,cwd=root,env=env,stdout=log,stderr=log)
after=digest();(work/'build-source-after.sha256').write_text(after)
print('Exit:',result.returncode,'source_changed_during_build:',before!=after,flush=True)
if result.returncode==0:
    binary=work/'target/debug/udp-session-e2e'
    (work/'binary.sha256').write_text(hashlib.file_digest(binary.open('rb'),'sha256').hexdigest()+'  '+str(binary)+'\n')
raise SystemExit(result.returncode)
