"""Ephemeral SSH filesystem operations. Inputs/outputs are framed JSON, not shell text."""
import base64
import ctypes
import hashlib
import json
import os
import pathlib
import shutil
import stat
import subprocess
import tempfile


def safe(path, missing=False):
    path = pathlib.Path(os.path.abspath(path))
    for part in reversed((path, *path.parents)):
        try:
            info = part.lstat()
        except FileNotFoundError:
            if missing:
                continue
            raise
        if stat.S_ISLNK(info.st_mode):
            raise ValueError('Symbolic links are not supported: ' + str(part))
    return path


def digest(path, limit=None):
    path = safe(path)
    with open(path, 'rb') as stream:
        before = os.fstat(stream.fileno())
        if not stat.S_ISREG(before.st_mode):
            raise ValueError('Not a regular file')
        size = before.st_size if limit is None else int(limit)
        if size > before.st_size:
            raise ValueError('Resume offset exceeds source size')
        h = hashlib.sha256()
        remaining = size
        while remaining:
            data = stream.read(min(1048576, remaining))
            if not data:
                raise ValueError('Source changed while hashing')
            h.update(data)
            remaining -= len(data)
        after = os.fstat(stream.fileno())
        if (before.st_size, before.st_mtime_ns) != (after.st_size, after.st_mtime_ns):
            raise ValueError('Source changed while hashing')
        return {'size': size, 'hash': h.hexdigest()}


def manifest(path):
    result = []
    def visit(p, rel):
        p = safe(p)
        info = p.lstat()
        if stat.S_ISDIR(info.st_mode):
            result.append({'path': rel, 'directory': True, 'size': 0, 'hash': ''})
            for item in sorted(p.iterdir()):
                visit(item, rel + '/' + item.name if rel else item.name)
        elif stat.S_ISREG(info.st_mode):
            result.append(dict(path=rel, directory=False, **digest(p)))
        else:
            raise ValueError('Special files are not supported')
    visit(path, '')
    return result


def run(req):
    op = req['op']
    if op == 'list':
        import datetime
        p = safe(req['path'])
        entries = []
        for item in p.iterdir():
            info = item.lstat()
            entries.append({'path': str(item), 'name': item.name,
                            'directory': stat.S_ISDIR(info.st_mode),
                            'link': stat.S_ISLNK(info.st_mode), 'size': info.st_size,
                            'modified': datetime.datetime.fromtimestamp(info.st_mtime, datetime.timezone.utc).strftime('%Y-%m-%d %H:%M UTC')})
        return {'path': str(p), 'entries': entries}
    if op == 'exists':
        return safe(req['path'], True).exists()
    if op == 'manifest':
        return manifest(req['path'])
    if op == 'hash':
        p = safe(req['path'], True)
        return digest(p, req.get('limit')) if p.exists() else None
    if op == 'prepare':
        base = safe(req.get('base') or str(pathlib.Path.home() / '.cache' / 'transfer-files'), True)
        base.mkdir(mode=0o700, parents=True, exist_ok=True)
        safe(base)
        p = base / ('.transfer-files-' + req['id'])
        p.mkdir(mode=0o700, exist_ok=True)
        info = safe(p).stat()
        if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) & 0o077:
            raise ValueError('Staging directory must be private and owned by this user')
        return str(p)
    if op == 'mkdir':
        p = safe(req['path'], True)
        p.mkdir(mode=0o700, parents=True, exist_ok=True)
        safe(p)
        return True
    if op == 'publish':
        source, target = safe(req['source']), safe(req['target'], True)
        # Linux renameat2(RENAME_NOREPLACE) publishes files or whole trees without races.
        libc = ctypes.CDLL(None, use_errno=True)
        rename = libc.renameat2
        rename.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
        if rename(-100, os.fsencode(source), -100, os.fsencode(target), 1) != 0:
            code = ctypes.get_errno()
            raise OSError(code, os.strerror(code), str(target))
        return True
    if op == 'remove':
        p = safe(req['path'])
        if not p.name.startswith('.transfer-files-'):
            raise ValueError('Refusing to remove a non-staging directory')
        shutil.rmtree(p)
        return True
    if op == 'batch':
        # Passwords still go through the controlling terminal, not the batch file.
        with tempfile.TemporaryDirectory(prefix='transfer-files-sftp-') as directory:
            batch = pathlib.Path(directory) / 'commands'
            batch.write_text(req['batch'], encoding='utf-8')
            os.chmod(batch, 0o600)
            print(req['marker'], flush=True)
            status = subprocess.call(['sftp', *req['args'], '-b', str(batch), '--', req['host']])
            if status:
                raise RuntimeError('SFTP failed with status ' + str(status))
        return True
    raise ValueError('Unknown operation')


try:
    result = run(REQUEST)
    answer = {'ok': True, 'value': result}
except Exception as error:
    answer = {'ok': False, 'error': str(error)}
encoded = base64.b64encode(json.dumps(answer, ensure_ascii=True).encode()).decode()
print('TF_RPC_' + TOKEN + '_BEGIN', flush=True)
for offset in range(0, len(encoded), 64):
    print(encoded[offset:offset+64], flush=True)
print('TF_RPC_' + TOKEN + '_END', flush=True)
if not answer['ok']:
    raise SystemExit(1)
