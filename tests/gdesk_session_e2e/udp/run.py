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

SCRIPT = Path(__file__).resolve()
ROOT = SCRIPT.parents[3]
BASE = ROOT / '.tmp/gdesk-session-e2e/udp'
WORK = Path(os.environ.get('E2E_OUTPUT_DIR', str(BASE/'runs'/('loss'+os.environ.get('E2E_LOSS','0'))))).resolve()

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

def kcp_segments(data):
    if len(data)<14 or data[13]!=0 or not (data[12]&8): return []
    conv=int.from_bytes(data[:4],'little'); src=int.from_bytes(data[4:8],'little'); dst=int.from_bytes(data[8:12],'little')
    pos=14; result=[]
    while pos<len(data):
        assert pos+24<=len(data), 'truncated KCP segment'
        cmd=data[pos+4]; sn=int.from_bytes(data[pos+12:pos+16],'little'); n=int.from_bytes(data[pos+20:pos+24],'little')
        assert cmd in (81,82,83,84) and pos+24+n<=len(data),'bad KCP segment'
        assert int.from_bytes(data[pos:pos+4],'little')==conv
        result.append((conv,src,dst,cmd,sn,n));pos+=24+n
    return result

# Multi-segment and PUSH/ACK parser check against the locked little-endian layout.
import struct
h=struct.pack('<IIIBB',7,8,9,8,0)
push=struct.pack('<IBBHIIII',7,81,0,32,1,3,0,2)+b'ab'
ack=struct.pack('<IBBHIIII',7,82,0,32,1,3,4,0)
assert kcp_segments(h+push+ack)==[(7,8,9,81,3,2),(7,8,9,82,3,0)]

class Proxy(asyncio.DatagramProtocol):
    def __init__(self, direction, loss, stats, log):
        self.direction, self.loss, self.stats, self.log = direction, loss, stats, log
        self.state = 0x9E3779B97F4A7C15
        self.push_seen=set()
    def connection_made(self, transport): self.transport = transport
    def datagram_received(self, data, address):
        target_file = 'server-udp-address' if self.direction == 'c2s' else 'client-udp-address'
        if not (WORK/target_file).exists(): return
        host, port = (WORK/target_file).read_text().split(':')
        # Same public magic test as production; never parse or decrypt application data here.
        aead = len(data) >= 14 and data[:2] == bytes([0x47, 0x44]) and data[12:14] == bytes([0xc7, 0x64])
        self.stats['max_udp'] = max(self.stats['max_udp'], len(data))
        key = self.direction + ('_aead' if aead else '_kcp_or_punch')
        self.stats[key] = self.stats.get(key, 0)+1
        segments=[]
        if not aead:
            try:
                segments=kcp_segments(data)
                if len(data)>=14 and data[13]==0:
                    flags=data[12]
                    for bit,name in [(1,'syn'),(2,'outer_ack'),(4,'fin'),(16,'rst'),(32,'ping'),(64,'pong')]:
                        if flags&bit:
                            k=self.direction+'_'+name;self.stats[k]=self.stats.get(k,0)+1
                for conv,src,dst,cmd,sn,n in segments:
                    k=self.direction+'_cmd_'+str(cmd);self.stats[k]=self.stats.get(k,0)+1
                    if cmd==81:
                        key=(self.direction,conv,src,dst,sn)
                        if key in self.push_seen:
                            k=self.direction+'_duplicate_push';self.stats[k]=self.stats.get(k,0)+1
                        else:self.push_seen.add(key)
            except AssertionError as exc:
                self.stats['kcp_parse_errors']=self.stats.get('kcp_parse_errors',0)+1
        drop = False
        if aead: self.stats['max_aead']=max(self.stats['max_aead'],len(data))
        if aead and self.direction == 's2c':
            self.state ^= (self.state << 13) & ((1<<64)-1)
            self.state ^= self.state >> 7
            self.state ^= (self.state << 17) & ((1<<64)-1)
            drop = self.state % 100 < self.loss
            if drop: self.stats['dropped'] += 1
        self.log.write(json.dumps({'monotonic':time.monotonic(), 'dir':self.direction,'bytes':len(data),'aead':aead,'drop':drop,'kcp_segments':segments})+'\n'); self.log.flush()
        if not drop: self.peer.transport.sendto(data, (host,int(port)))

def read_screen(screen):
    x=screen.x
    x.XGetImage.argtypes=[C.c_void_p,C.c_ulong,C.c_int,C.c_int,C.c_uint,C.c_uint,C.c_ulong,C.c_int];x.XGetImage.restype=C.c_void_p
    x.XGetPixel.argtypes=[C.c_void_p,C.c_int,C.c_int];x.XGetPixel.restype=C.c_ulong
    x.XDestroyImage.argtypes=[C.c_void_p];x.XDestroyImage.restype=C.c_int
    image=x.XGetImage(screen.d,screen.root,0,0,256,128,0xffffffff,2);assert image
    try:
        result=bytearray()
        for y in range(128):
            for xx in range(256):
                pixel=x.XGetPixel(image,xx,y);result.extend((pixel&255,(pixel>>8)&255,(pixel>>16)&255,255))
        return bytes(result)
    finally:x.XDestroyImage(image)

async def inside():
    assert os.environ['HOME'] == os.environ['E2E_ORIGINAL_HOME']
    assert os.environ.get('CODEX_HOME','') == os.environ['E2E_ORIGINAL_CODEX_HOME']
    loss = int(os.environ.get('E2E_LOSS','0'))
    processes=[]; handles=[]; screens=[]; transports=[]
    stats={'loss_percent':loss,'drop_scope':'s2c GDesk AEAD only; KCP never deliberately dropped','max_udp':0,'dropped':0,'kcp_parse_errors':0,'c2s_duplicate_push':0,'s2c_duplicate_push':0,'max_aead':0}
    def launch(role, display=None):
        config=WORK/(role+'-config'); config.mkdir(exist_ok=True)
        env=dict(os.environ,XDG_CONFIG_HOME=str(config),RUSTDESK_FORCED_DISPLAY_SERVER='x11',E2E_PIXELS=str(WORK/'pixels.bgra'))
        if display: env['DISPLAY']=display
        f=open(WORK/(role+'.log'),'wb'); handles.append(f)
        p=subprocess.Popen([str(BASE/'target/debug/udp-session-e2e'),role],env=env,stdout=f,stderr=f)
        processes.append(p); return p
    async def waitfile(name, proc, seconds=15):
        deadline=time.monotonic()+seconds
        while not (WORK/name).exists():
            assert proc.poll() is None, name+' process exited'
            assert time.monotonic()<deadline, name+' readiness timeout'
            await asyncio.sleep(.1)
    try:
        displays=[]
        for label in ('server','client'):
            rd,wr=os.pipe(); f=open(WORK/f'xvfb-{label}.log','wb'); handles.append(f)
            p=subprocess.Popen(['Xvfb','-displayfd',str(wr),'-screen','0','256x128x24','-nolisten','tcp','-noreset'],pass_fds=(wr,),stdout=f,stderr=f)
            processes.append(p); os.close(wr)
            number=os.read(rd,64).decode().strip();os.close(rd);assert number.isdigit()
            displays.append(':'+number)
        screen=Screen(displays[0]);screens.append(screen)
        expected=screen.paint(0);assert read_screen(screen)==expected,'initial Xvfb fixture mismatch';(WORK/'expected.bgra').write_bytes(expected)
        wire=open(WORK/'wire.jsonl','w');handles.append(wire)
        loop=asyncio.get_running_loop()
        protocols=[]
        for direction,port in [('c2s',29871),('s2c',29872)]:
            t,p=await loop.create_datagram_endpoint(lambda d=direction:Proxy(d,loss,stats,wire),local_addr=('127.0.0.1',port))
            transports.append(t);protocols.append(p)
        protocols[0].peer=protocols[1];protocols[1].peer=protocols[0]
        rv=launch('rendezvous');await waitfile('rendezvous-ready',rv)
        server=launch('server',displays[0]);await waitfile('registered',server)
        client=launch('client',displays[1])
        deadline=time.monotonic()+60
        first_pixel=None; converged=None; settled_at=None
        # Await real login, decode and a GDesk AEAD packet before changing the private desktop.
        while time.monotonic()<deadline:
            assert client.poll() is None,'client stopped before handshake'
            log=(WORK/'client.log').read_text(errors='replace')
            if (WORK/'pixels.bgra').exists() and stats.get('s2c_aead',0)>0:
                first_pixel=time.monotonic();break
            await asyncio.sleep(.1)
        assert first_pixel, 'production KCP login/GDesk AEAD handshake did not produce decoded pixels'
        assert 'type=UDP' in log and 'secured=true' in log,'transport must be production secured KCP/UDP'
        print('Real Remote login and AEAD decode reached',json.dumps(stats),flush=True)
        # Exercise ACK/refinement after actual OS changes, then stop drawing.
        for phase in range(1,9):
            screen.paint(phase);await asyncio.sleep(.15)
        expected=screen.paint(211);actual=read_screen(screen);(WORK/'source.bgra').write_bytes(actual);assert actual==expected,'final Xvfb source/expectation mismatch';stats['source_readback_max_error']=0;(WORK/'expected.bgra').write_bytes(expected)
        settled_at=time.monotonic()
        deadline=settled_at+45
        while time.monotonic()<deadline:
            pixels=(WORK/'pixels.bgra').read_bytes() if (WORK/'pixels.bgra').exists() else b''
            if len(pixels)==len(expected):
                error=max(abs(a-b) for i,(a,b) in enumerate(zip(expected,pixels)) if i%4!=3)
                if error==0:
                    converged=time.monotonic();break
            await asyncio.sleep(.1)
        log=(WORK/'client.log').read_text(errors='replace')
        stats.update(converged=bool(converged),convergence_wall_s=converged-settled_at if converged else None,
            decoded_pixel_bytes=len(pixels),max_error_rgb=error if len(pixels)==len(expected) else None,
            gpu_backend_observed='GpuVulkan' in log,secured_udp='type=UDP' in log and 'secured=true' in log)
        # Let the real ACK travel back and observe whether the encrypted video lane settles.
        before=stats.get('s2c_aead',0);await asyncio.sleep(3)
        stats['tail_aead_packets_3s']=stats.get('s2c_aead',0)-before
        stats['server_exit']=server.poll();stats['client_exit']=client.poll()
        (WORK/'summary.json').write_text(json.dumps(stats,indent=2));print(json.dumps(stats,indent=2),flush=True)
        assert stats['tail_aead_packets_3s']==0,'video AEAD did not settle after pixel convergence'
        assert stats['c2s_duplicate_push']==0 and stats['s2c_duplicate_push']==0,'repeated KCP PUSH observed'
        assert stats['server_exit'] is None and stats['client_exit'] is None,'production peer exited early'
        assert stats['max_aead']<=1200,'AEAD MTU exceeded'
        assert stats['kcp_parse_errors']==0,'KCP packet parsing errors'
        assert converged, 'production GDesk pixels did not converge after desktop stopped'
        assert stats['gpu_backend_observed'],'remote GPU metadata not observed'
        assert loss==0 or stats['dropped']>0,'loss injection not exercised'
    finally:
        for t in transports:t.close()
        for screen in screens:screen.close()
        for p in reversed(processes):
            if p.poll() is None:p.terminate()
        for p in reversed(processes):
            try:p.wait(timeout=4)
            except subprocess.TimeoutExpired:p.kill();p.wait()
        for f in handles:f.close()
        print('Private namespaces/children cleaned; user HOME/daemon/desktop untouched',flush=True)

if __name__=='__main__':
    if '--inside' in sys.argv:
        os.chdir(WORK);asyncio.run(inside())
    else:
        assert WORK.is_relative_to(BASE), 'output must stay inside the acceptance directory'
        assert 0 <= int(os.environ.get('E2E_LOSS','0')) <= 100, 'E2E_LOSS must be 0..100'
        assert (BASE/'target/debug/udp-session-e2e').is_file(), 'run build.py first'
        WORK.mkdir(parents=True,exist_ok=False)
        for name in ('runtime','run','server-config','client-config','rendezvous-config'):(WORK/name).mkdir(exist_ok=True)
        for name in ('server-ready','rendezvous-ready','registered','client-udp-address','server-udp-address','pixels.bgra','rendezvous-key'):(WORK/name).unlink(missing_ok=True)
        args=['bwrap','--unshare-net','--unshare-ipc','--unshare-pid','--die-with-parent','--ro-bind','/','/','--bind',str(WORK),str(WORK),'--bind',str(WORK/'runtime'),'/tmp','--bind',str(WORK/'run'),'/run','--dev','/dev','--proc','/proc','--chdir',str(WORK),'--setenv','E2E_OUTPUT_DIR',str(WORK),'--setenv','E2E_ORIGINAL_HOME',os.environ.get('HOME',''),'--setenv','E2E_ORIGINAL_CODEX_HOME',os.environ.get('CODEX_HOME','')]
        if Path('/dev/dri').exists():args+=['--dev-bind','/dev/dri','/dev/dri']
        import pwd
        user=pwd.getpwuid(os.getuid()).pw_name
        (WORK/'passwd').write_text(''.join(line for line in Path('/etc/passwd').read_text().splitlines(keepends=True) if line.split(':')[0] in (user,'root')))
        args += ['--ro-bind',str(WORK/'passwd'),'/etc/passwd']
        args += [sys.executable,str(SCRIPT),'--inside']
        print('Command:', ' '.join(args),flush=True)
        raise SystemExit(subprocess.call(args))
