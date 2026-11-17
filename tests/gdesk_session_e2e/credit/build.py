from pathlib import Path
import hashlib
import os
import subprocess

root = Path(__file__).resolve().parents[3]
work = root / '.tmp/gdesk-session-e2e/credit'
assert (work / 'Cargo.toml').is_file(), 'run prepare.py first'
sources = ['src/lib.rs', 'src/client.rs', 'src/client/io_loop.rs',
           'src/server/connection.rs', 'src/gdesk_bulk.rs', 'src/gdesk_session.rs',
           'libs/gdesk/src/sched.rs', 'libs/gdesk/src/udp.rs',
           'libs/scrap/src/common/gdesk_codec.rs', 'libs/base/protos/message.proto']


def digest():
    return ''.join(f'{hashlib.sha256((root / p).read_bytes()).hexdigest()}  {p}\n'
                   for p in sources)


before = digest()
(work / 'build-source.sha256').write_text(before)
env = dict(os.environ)
env.setdefault('CXXFLAGS', '-include cstdint')
command = ['cargo', 'build', '--offline', '--manifest-path', str(work / 'Cargo.toml'),
           '--target-dir', str(work / 'target'), '--features', 'gdesk', '--bin', 'session-e2e']
print('Command:', ' '.join(command), flush=True)
with (work / 'build.log').open('wb') as log:
    result = subprocess.run(command, cwd=root, env=env, stdout=log, stderr=log)
after = digest()
(work / 'build-source-after.sha256').write_text(after)
print('Exit:', result.returncode, 'source_changed_during_build:', before != after, flush=True)
if result.returncode == 0:
    binary = work / 'target/debug/session-e2e'
    with binary.open('rb') as stream:
        checksum = hashlib.file_digest(stream, 'sha256').hexdigest()
    (work / 'binary.sha256').write_text(checksum + '  ' + str(binary) + '\n')
raise SystemExit(result.returncode)
