import asyncio
import ctypes as C
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time

BASELINE = '--baseline' in sys.argv
SCRIPT = Path(__file__).resolve()
ROOT = SCRIPT.parents[3]
BASE = ROOT / '.tmp/gdesk-session-e2e/credit'
WORK = Path(os.environ.get('E2E_OUTPUT_DIR', str(BASE / 'runs' / ('baseline' if BASELINE else 'clipboard')))).resolve()

def fields(data):
    def varint(pos):
        value = shift = 0
        while True:
            b = data[pos]; pos += 1
            value |= (b & 127) << shift
            if b < 128:
                return value, pos
            shift += 7
            assert shift < 70
    pos = 0
    result = {}
    while pos < len(data):
        tag, pos = varint(pos)
        kind = tag & 7
        if kind == 0:
            value, pos = varint(pos)
        elif kind == 2:
            n, pos = varint(pos)
            value = data[pos:pos+n]; pos += n
        elif kind in (1, 5):
            n = 8 if kind == 1 else 4
            value = data[pos:pos+n]; pos += n
        else:
            raise ValueError(f'unknown protobuf wire type {kind}')
        result.setdefault(tag >> 3, []).append(value)
    return result
assert fields(b'\x08\x96\x01\x12\x03abc') == {1: [150], 2: [b'abc']}
assert fields(b'\xa2\x02\x02\x30\x04')[36] == [b'\x30\x04']
assert fields(b'\xaa\x02\x02\x08\x04')[37] == [b'\x08\x04']

class Screen:
    def __init__(self, display):
        self.x = C.CDLL('libX11.so.6')
        signatures = {
            'XOpenDisplay': ([C.c_char_p], C.c_void_p),
            'XDefaultRootWindow': ([C.c_void_p], C.c_ulong),
            'XCreateGC': ([C.c_void_p,C.c_ulong,C.c_ulong,C.c_void_p],C.c_void_p),
            'XSetForeground': ([C.c_void_p,C.c_void_p,C.c_ulong],C.c_int),
            'XFillRectangle': ([C.c_void_p,C.c_ulong,C.c_void_p,C.c_int,C.c_int,C.c_uint,C.c_uint],C.c_int),
            'XSync': ([C.c_void_p,C.c_int],C.c_int),
            'XWarpPointer': ([C.c_void_p,C.c_ulong,C.c_ulong,C.c_int,C.c_int,C.c_uint,C.c_uint,C.c_int,C.c_int],C.c_int),
            'XFreeGC': ([C.c_void_p,C.c_void_p],C.c_int),
            'XCloseDisplay': ([C.c_void_p],C.c_int),
        }
        for name,(args,result) in signatures.items():
            f=getattr(self.x,name); f.argtypes=args; f.restype=result
        self.d=self.x.XOpenDisplay(display.encode()); assert self.d
        self.root=self.x.XDefaultRootWindow(self.d)
        self.gc=self.x.XCreateGC(self.d,self.root,0,None)
    def paint(self, phase):
        expected=bytearray()
        # Deterministic multicolor tile pattern, no scaling or lossy reference conversion.
        for y in range(0,128,8):
            for x in range(0,256,8):
                r=(x*3+y*5+phase*31)&255; g=(x*7+y*11+phase*17)&255; b=(x*13+y*3+phase*7)&255
                self.x.XSetForeground(self.d,self.gc,(r<<16)|(g<<8)|b)
                self.x.XFillRectangle(self.d,self.root,self.gc,x,y,8,8)
        self.x.XWarpPointer(self.d,0,self.root,0,0,0,0,(phase*11)%256,(phase*7)%128)
        self.x.XSync(self.d,0)
        return bytes(v for y in range(128) for x in range(256) for v in (((x//8*8)*13+(y//8*8)*3+phase*7)&255,((x//8*8)*7+(y//8*8)*11+phase*17)&255,((x//8*8)*3+(y//8*8)*5+phase*31)&255,255))
    def close(self):
        self.x.XFreeGC(self.d,self.gc); self.x.XCloseDisplay(self.d)

async def inside():
    assert os.environ.get('HOME') == os.environ['E2E_ORIGINAL_HOME']
    assert os.environ.get('CODEX_HOME','') == os.environ['E2E_ORIGINAL_CODEX_HOME']
    processes=[]; handles=[]; screens=[]
    events=[]
    transfers={}
    with open(WORK/'wire.jsonl','w') as wire:
        def launch(cmd, name, env=None):
            f=open(WORK/name,'wb'); handles.append(f)
            p=subprocess.Popen(cmd,env=env,stdout=f,stderr=f)
            processes.append(p); return p
        try:
            displays=[]
            for label in ('server','client'):
                rd,wr=os.pipe()
                f=open(WORK/f'xvfb-{label}.log','wb'); handles.append(f)
                p=subprocess.Popen(['Xvfb','-displayfd',str(wr),'-screen','0','256x128x24','-nolisten','tcp','-noreset'],pass_fds=(wr,),stdout=f,stderr=f)
                processes.append(p); os.close(wr)
                number=os.read(rd,64).decode().strip(); os.close(rd)
                assert number.isdigit(), f'Xvfb failed: {label}'
                displays.append(':'+number)
            screen=Screen(displays[0]); screens.append(screen); screen.paint(0)
            print('Private Xvfb',displays,'HOME preserved; private net/mount/PID namespaces',flush=True)
            async def forward(reader, writer, direction):
                queue=asyncio.Queue(maxsize=32)
                async def ingest():
                    try:
                        while True:
                            first=await reader.readexactly(1)
                            nhead=(first[0]&3)+1
                            head=first+await reader.readexactly(nhead-1)
                            n=int.from_bytes(head,'little')>>2
                            data=await reader.readexactly(n)
                            await queue.put((head,data,time.monotonic(),time.time()))
                    except (asyncio.IncompleteReadError,ConnectionError):
                        await queue.put(None)
                async def shape():
                    deadline=0.0
                    while True:
                        packet=await queue.get()
                        if packet is None: break
                        head,data,arrived,read_wall=packet
                        n=len(data)
                        if direction=='s2c':
                            deadline=max(deadline,arrived)+(len(head)+n)/(4*1024*1024)
                            delay=deadline-time.monotonic()
                            if delay>0: await asyncio.sleep(delay)
                        egress=time.time()
                        writer.write(head+data); await writer.drain()
                        ev={'t':egress,'read_t':read_wall,'dir':direction,'bytes':n,'framing_bytes':len(head),'queue_ms':(egress-read_wall)*1000,'after_drain_t':time.time(),'clock_late_ms':max(0,(time.monotonic()-deadline)*1000) if direction=='s2c' else 0}
                        try:
                            f=fields(data); ev['fields']=list(f)
                            if 36 in f:
                                chunk=fields(f[36][0]); ev.update(kind='bulk',index=chunk.get(2,[0])[0],total=chunk.get(3,[0])[0],payload=len(chunk.get(4,[b''])[0]),transfer=chunk.get(1,[0])[0],credit_seq=chunk.get(6,[0])[0])
                                tid=(direction,ev['transfer'])
                                pieces=transfers.setdefault(tid,[])
                                assert ev['index']==len(pieces), 'non-sequential transfer'
                                pieces.append(chunk.get(4,[b''])[0])
                                if len(pieces)==ev['total']:
                                    f=fields(b''.join(pieces)); del transfers[tid]
                                    ev['reassembled_fields']=list(f)
                                    if 16 in f:
                                        text=fields(f[16][0]).get(2,[b''])[0]
                                        ev.update(clipboard_bytes=len(text),clipboard_sha256=hashlib.sha256(text).hexdigest())
                            if 37 in f:
                                ev.update(kind='credit_ack',credit_seq=fields(f[37][0]).get(1,[0])[0])
                            if 6 in f:
                                vf=fields(f[6][0]); ev['video']=True; ev['codecs']=list(vf)
                                if 15 in vf:
                                    frames=fields(vf[15][0]).get(1,[])
                                    ev['backend']=[fields(frame).get(5,[b''])[0].decode(errors='replace') for frame in frames]
                            elif 19 in f and 40 in fields(f[19][0]):
                                ack=fields(fields(f[19][0])[40][0]); ev.update(kind='ack',seq=ack.get(1,[0])[0],bitmap=len(ack.get(5,[b''])[0]))
                            elif any(k in f for k in (12,13,14)): ev['cursor']=True
                            elif 35 in f: ev['kind']='capabilities'
                        except Exception as e:
                            ev['parse_error']=str(e)
                        events.append(ev); wire.write(json.dumps(ev)+'\n'); wire.flush()
                    writer.close()
                await asyncio.gather(ingest(),shape())
            async def proxy(reader, writer):
                sr,sw=await asyncio.open_connection('127.0.0.1',29876)
                for w in (writer,sw):
                    w.transport.set_write_buffer_limits(high=32768)
                    w.get_extra_info('socket').setsockopt(socket.SOL_SOCKET,socket.SO_SNDBUF,32768)
                await asyncio.gather(forward(reader,sw,'c2s'),forward(sr,writer,'s2c'))
            proxy_server=await asyncio.start_server(proxy,'127.0.0.1',29877)
            env=dict(os.environ,DISPLAY=displays[0],XDG_CONFIG_HOME=str(WORK/'server-config'),RUSTDESK_FORCED_DISPLAY_SERVER='x11')
            server=launch([str(BASE/'target/debug/session-e2e'),'server'],'server.log',env)
            for i in range(100):
                if (WORK/'ready').exists(): break
                assert server.poll() is None, 'server exited before ready'
                await asyncio.sleep(.1)
            assert (WORK/'ready').exists(), 'server readiness timeout'
            env=dict(os.environ,DISPLAY=displays[1],XDG_CONFIG_HOME=str(WORK/'client-config'),RUSTDESK_FORCED_DISPLAY_SERVER='x11',E2E_PIXELS=str(WORK/'pixels.bgra'))
            client=launch([str(BASE/'target/debug/session-e2e'),'client'],'client.log',env)
            payload=bytes(33+(i*17+i//97)%90 for i in range(20*1024*1024))
            (WORK/'clipboard.txt').write_bytes(payload)
            await asyncio.sleep(4)
            if not BASELINE: (WORK/'inject').write_text('inject')
            painter=launch([sys.executable,str(SCRIPT),'--paint',displays[0]],'painter.log')
            for _ in range(500):
                if painter.poll() is not None: break
                assert client.poll() is None, 'client exited during active drawing'
                await asyncio.sleep(.1)
            assert painter.poll()==0, 'painter failed or timed out'
            expected=(WORK/'expected.bgra').read_bytes()
            await asyncio.sleep(4)
            try:
                result=await asyncio.to_thread(subprocess.run,['xclip','-display',displays[1],'-selection','clipboard','-o'],capture_output=True,timeout=3)
                received=result.stdout; os_clipboard_error=result.stderr.decode(errors='replace')
            except subprocess.TimeoutExpired:
                received=b''; os_clipboard_error='X11 selection read timed out; see arboard Maximum request length exceeded in client.log'
            (WORK/'clipboard-received.txt').write_bytes(received)
            pixels=(WORK/'pixels.bgra').read_bytes() if (WORK/'pixels.bgra').exists() else b''
            maximum=max((abs(a-b) for i,(a,b) in enumerate(zip(expected,pixels)) if i%4!=3),default=None) if len(pixels)==len(expected) else None
            bulk=[e for e in events if e.get('kind')=='bulk' and e['dir']=='s2c' and e['total']>1000]
            window=(min(e['t'] for e in bulk),max(e['t'] for e in bulk)) if bulk else (0,0)
            if BASELINE:
                reference=[json.loads(line) for line in (Path(os.environ.get('E2E_BASELINE_REFERENCE', str(BASE/'runs/clipboard')))/'wire.jsonl').read_text().splitlines()]
                ref_bulk=[e for e in reference if e.get('kind')=='bulk' and e['dir']=='s2c' and e['total']>1000]
                offset=min(e['t'] for e in ref_bulk)-min(e['t'] for e in reference)
                start=min(e['t'] for e in events)+offset
                window=(start,start+max(e['t'] for e in ref_bulk)-min(e['t'] for e in ref_bulk))
            interleaved=[e for e in events if window[0]<e['t']<window[1]]
            times=[e['t'] for e in interleaved if e.get('video') and e['dir']=='s2c']
            gaps=sorted(1000*(b-a) for a,b in zip(times,times[1:]))
            summary={'server_exit':server.poll(),'client_exit':client.poll(),'clipboard_bytes':len(received),'clipboard_sha256':hashlib.sha256(received).hexdigest(),'clipboard_expected_sha256':hashlib.sha256(payload).hexdigest(),'clipboard_equal':received==payload,'pixel_bytes':len(pixels),'max_error_rgb':maximum,'bulk_chunks':len(bulk),'bulk_max_payload':max((e['payload'] for e in bulk),default=0),'bulk_window_s':window[1]-window[0],'video_during_bulk':len(times),'cursor_during_bulk':sum(e.get('cursor',False) and e['dir']=='s2c' for e in interleaved),'ack_during_bulk':sum(e.get('kind')=='ack' and e['dir']=='c2s' for e in interleaved),'video_gap_p99_ms':gaps[min(len(gaps)-1,int(len(gaps)*.99))] if gaps else None,'video_gap_max_ms':max(gaps,default=None),'video_backends':sorted(set(b for e in events for b in e.get('backend',[]))),'wire_parse_errors':sum('parse_error' in e for e in events),'events':len(events),'os_clipboard_error':os_clipboard_error,'wire_clipboards':[{'bytes':e['clipboard_bytes'],'sha256':e['clipboard_sha256']} for e in events if 'clipboard_bytes' in e]}
            (WORK/'summary.json').write_text(json.dumps(summary,indent=2)); print(json.dumps(summary,indent=2),flush=True)
            proxy_server.close()
            if BASELINE:
                assert maximum==0, 'baseline decoded final image mismatch'
                assert len(times)>5 and summary['cursor_during_bulk']>5 and summary['ack_during_bulk']>5, 'baseline activity missing'
                loaded=json.loads((Path(os.environ.get('E2E_BASELINE_REFERENCE', str(BASE/'runs/clipboard')))/'summary.json').read_text())
                budget_ms=4*16384/(4*1024*1024)*1000
                assert loaded['video_gap_p99_ms'] <= summary['video_gap_p99_ms'] + budget_ms, 'bulk P99 exceeds baseline plus one credit window serialization budget'
                return
            expected_hash=hashlib.sha256(payload).hexdigest()
            assert any(e.get('clipboard_sha256')==expected_hash for e in events), '20MiB wire clipboard mismatch'
            assert 'clipboard updated on client' in (WORK/'client.log').read_text(), 'client business acceptance missing'
            assert maximum==0, 'decoded final image mismatch'
            assert bulk and all(e['payload']<=16384 for e in bulk), 'missing/bad bulk chunks'
            assert len(times)>5 and summary['cursor_during_bulk']>5 and summary['ack_during_bulk']>5, 'insufficient P0/P1 interleaving'
            assert any(b.startswith('Gpu') for b in summary['video_backends']), 'GPU backend not proved'
        finally:
            for p in reversed(processes):
                if p.poll() is None: p.terminate()
            for p in reversed(processes):
                try: p.wait(timeout=4)
                except subprocess.TimeoutExpired: p.kill(); p.wait()
            for f in handles: f.close()
            print('Private children terminated',flush=True)

if __name__=='__main__':
    if '--paint' in sys.argv:
        screen=Screen(sys.argv[-1])
        for phase in range(1,321):
            started=time.time(); screen.paint(phase)
            print(json.dumps({'phase':phase,'start':started,'end':time.time()}),flush=True)
            time.sleep(.05)
        expected=screen.paint(211); (WORK/'expected.bgra').write_bytes(expected)
        screen.close()
    elif '--inside' in sys.argv:
        os.chdir(WORK)
        asyncio.run(inside())
    else:
        assert WORK.is_relative_to(BASE), 'output must stay inside the owned acceptance directory'
        assert (BASE/'target/debug/session-e2e').is_file(), 'run prepare.py and build.py first'
        WORK.mkdir(parents=True, exist_ok=False)
        for name in ('runtime','run','server-config','client-config'):
            (WORK/name).mkdir(exist_ok=True)
        for name in ('ready','inject','pixels.bgra'):
            (WORK/name).unlink(missing_ok=True)
        args=['bwrap','--unshare-net','--unshare-ipc','--unshare-pid','--die-with-parent','--ro-bind','/','/','--bind',str(WORK),str(WORK),'--bind',str(WORK/'runtime'),'/tmp','--bind',str(WORK/'run'),'/run','--dev','/dev','--proc','/proc','--chdir',str(WORK),'--setenv','E2E_OUTPUT_DIR',str(WORK),'--setenv','E2E_ORIGINAL_HOME',os.environ.get('HOME',''),'--setenv','E2E_ORIGINAL_CODEX_HOME',os.environ.get('CODEX_HOME','')]
        if Path('/dev/dri').exists(): args+=['--dev-bind','/dev/dri','/dev/dri']
        import pwd
        user = pwd.getpwuid(os.getuid()).pw_name
        (WORK/'passwd').write_text(''.join(line for line in Path('/etc/passwd').read_text().splitlines(keepends=True) if line.split(':')[0] in (user, 'root')))
        args += ['--ro-bind',str(WORK/'passwd'),'/etc/passwd']
        args += [sys.executable,str(SCRIPT),'--inside'] + (['--baseline'] if BASELINE else [])
        print('Reproduce:', ' '.join(args),flush=True)
        raise SystemExit(subprocess.call(args))
