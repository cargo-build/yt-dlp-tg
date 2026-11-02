#!/usr/bin/env python3
"""Exercise the built bot against local Telegram and yt-dlp fixtures."""

import http.server
from email.parser import BytesParser
from email.policy import default
import json
import os
import re
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
import urllib.parse
import urllib.error


FAKE_YTDLP = r'''#!/usr/bin/env python3
import json, os, pathlib, re, shutil, sys, time, urllib.parse
a = sys.argv[1:]
with open(os.environ['FAKE_CALLS'], 'a') as calls:
    calls.write(json.dumps(a) + '\n')
if '--dump-single-json' in a and 'delayed' in a[-1]:
    pathlib.Path(os.environ['FAKE_STARTED']).write_text('started')
    while pathlib.Path(os.environ['FAKE_GATE']).exists():
        time.sleep(0.02)
if '--paths' in a:
    with open(os.environ['FAKE_DOWNLOADS'], 'a') as calls:
        calls.write(json.dumps(a) + '\n')
    if 'dQw4w9WgXcQ' in a[-1]:
        pathlib.Path(os.environ['FAKE_YT_STARTED']).write_text('downloading')
        while pathlib.Path(os.environ['FAKE_YT_GATE']).exists():
            time.sleep(.02)
if '--extractor-descriptions' in a:
    for n, k in [('youtube:search','ytsearch'), ('BiliBiliSearch','bilisearch'), ('nicovideo:search','nicosearch'), ('prxseries:search','prxseries'), ('rokfin:search','rkfnsearch'), ('soundcloud:search','scsearch'), ('yahoo:search','yvsearch')]:
        print(f'{n}: Search; "{k}:" prefix')
elif '--dump-single-json' in a:
    operand = a[-1]
    if 'unsupported' in operand and operand.startswith('http'):
        sys.exit(1)
    if not operand.startswith(('http://', 'https://')):
        if 'LookupFail' in operand:
            sys.exit(1)
        start, end = map(int, a[a.index('--playlist-items')+1].split(':'))
        entries = [{'title':f'Result {i}','url':f'https://media.invalid/track{i}'} for i in range(start,end+1)]
        if 'Forhill & Unfound' in operand and 'Atlas' in operand:
            entries[1]['title'] = 'Unfound & Forhill - Atlas'
        print(json.dumps({'_type':'playlist','entries':entries}))
    else:
        if os.environ.get('FAKE_REAL_VIDEO_DIR'):
            directory=pathlib.Path(os.environ['FAKE_REAL_VIDEO_DIR'])
            key=urllib.parse.urlparse(operand).path.rsplit('/',1)[-1]
            record=json.loads((directory/'manifest.json').read_text())[key]
            fmt={'format_id':'fixture','height':record['height'],'vcodec':record['video'],'acodec':record['audio'],
                 'filesize':(directory/record['file']).stat().st_size}
            print(json.dumps({'title':'Real media fixture','webpage_url':operand,'duration':1,'formats':[fmt]}));sys.exit(0)
        formats = [
            {'format_id':'a','vcodec':'none','acodec':'mp4a.40.2','filesize':1000000},
            {'format_id':'v1080','height':1080,'vcodec':'avc1','acodec':'none','filesize':80000000},
            {'format_id':'v720','height':720,'vcodec':'avc1','acodec':'none','filesize':4000000},
            {'format_id':'v480','height':480,'vcodec':'avc1','acodec':'none','filesize':2000000},
            {'format_id':'v360','height':360,'vcodec':'avc1','acodec':'none'},
        ]
        if operand.endswith('/format-error'): formats = 42
        if operand.endswith('/no-formats'): formats = []
        if operand.endswith('/header-sizes'):
            origin = os.environ['FAKE_HEADER_ORIGIN']
            formats = [{'format_id':'a','vcodec':'none','acodec':'mp4a.40.2','protocol':'http','url':origin+'/probe/audio'}]
            formats += [{'format_id':'v'+str(height),'height':height,'vcodec':'avc1','acodec':'none','protocol':'http','url':origin+'/probe/'+kind} for height,kind in [(640,'video'),(720,'partial'),(1080,'compressed'),(480,'missing'),(360,'error'),(240,'html')]]
        if operand.endswith('/opus-only'):
            formats = [{'format_id':'opus','vcodec':'none','acodec':'opus','filesize':10000}]
        metadata = {'title':'Direct media','webpage_url':operand,'formats':formats,
                    'acodec':'opus' if operand.endswith('/opus-only') else 'mp4a.40.2'}
        if operand.endswith('/header-sizes'): metadata['http_headers'] = {'Referer':'https://fixture.invalid/'}
        if urllib.parse.urlparse(operand).hostname in ('youtu.be','www.youtube.com','m.youtube.com','music.youtube.com'):
            metadata.update(id='dQw4w9WgXcQ', extractor_key='Youtube')
        print(json.dumps(metadata))
else:
    directory = pathlib.Path(a[a.index('--paths')+1])
    ext = a[a.index('--audio-format')+1] if '--extract-audio' in a else ('mkv' if 'mkv' in a[-1] else 'mp4')
    if ext == 'best': ext = 'm4a'
    path = directory / ('media.'+ext)
    if os.environ.get('FAKE_REAL_VIDEO_DIR'):
        fixtures=pathlib.Path(os.environ['FAKE_REAL_VIDEO_DIR'])
        key=urllib.parse.urlparse(a[-1]).path.rsplit('/',1)[-1]
        record=json.loads((fixtures/'manifest.json').read_text())[key]
        shutil.copyfile(fixtures/record['file'],path)
        print(json.dumps(str(path)));sys.exit(0)
    if a[-1].endswith('/slow-progress'):
        total = 8*1024*1024
        with path.open('wb') as file:
            for i in range(16):
                file.write(b'x'*(total//16)); file.flush()
                print(f'yt-dlp-tg-progress1|audio|downloading|{(i+1)*total//16}|{total}|NA',flush=True)
                time.sleep(.4)
        print(f'yt-dlp-tg-progress1|audio|finished|NA|{total}|NA',flush=True)
        print(json.dumps(str(path))); sys.exit(0)
    if a[-1].endswith('/combined-audio'):
        combined = directory / 'unknown-size-input.part'
        with combined.open('wb') as file:
            for i in range(60):
                file.write(b'x'*1024*1024)
        time.sleep(.3)
        combined.unlink()
    if a[-1].endswith('/underestimated'):
        with path.open('wb') as file:
            for i in range(120):
                file.write(b'x'*1024*1024); file.flush(); time.sleep(.01)
        print(json.dumps(str(path))); sys.exit(0)
    path.write_bytes(b'not-aac' if a[-1].endswith('/aac-mismatch') else b'fixture media content')
    if '--extract-audio' not in a:
        selector = a[a.index('--format')+1]
        height = int(re.search(r'\[height=(\d+)\]',selector)[1])
        path.write_text(json.dumps({'height':height,'video':'vp9' if path.suffix=='.mkv' else 'h264','audio':'aac'}))
    if '--progress' in a:
        print(f'yt-dlp-tg-progress1|audio|finished|NA|{path.stat().st_size}|NA')
    print(json.dumps(str(path)))
'''


FAKE_FFPROBE = r'''#!/usr/bin/env python3
import json,pathlib,sys
path=pathlib.Path(sys.argv[-1].removeprefix('file:'))
content=path.read_bytes()
if path.suffix in ('.mp4','.mkv'):
    payload=content[content.index(b'mdat')+4:] if content[4:8]==b'ftyp' else content
    data=json.loads(payload)
    streams=[{'index':0,'codec_type':'video','codec_name':data['video'],'pix_fmt':'yuv420p','width':1280,'height':data['height'],'disposition':{'attached_pic':0},'side_data_list':[]}]
    if data.get('audio'): streams.append({'index':1,'codec_type':'audio','codec_name':data['audio'],'channels':2,'disposition':{'attached_pic':0}})
    print(json.dumps({'streams':streams,'format':{'format_name':'matroska,webm' if path.suffix=='.mkv' else 'mov,mp4,m4a,3gp,3g2,mj2','duration':'3.0'}}))
else:
    print(json.dumps({'streams':[{'codec_type':'audio','codec_name':'mp3' if content==b'not-aac' else 'aac'}]}))
'''

FAKE_FFMPEG = r'''#!/usr/bin/env python3
import json,pathlib,struct,sys
a=sys.argv[1:]
source=pathlib.Path(a[a.index('-i')+1].removeprefix('file:'))
content=source.read_bytes()
data=json.loads(content[content.index(b'mdat')+4:] if content[4:8]==b'ftyp' else content)
data['video']='h264'
if data.get('audio'): data['audio']='aac'
def box(kind,payload=b''): return struct.pack('>I4s',len(payload)+8,kind)+payload
pathlib.Path(a[-1].removeprefix('file:')).write_bytes(box(b'ftyp',b'isom\0\0\0\0isom')+box(b'moov')+box(b'mdat',json.dumps(data).encode()))
'''


class Telegram(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def do_HEAD(self):
        self.server.state.setdefault('head_requests', []).append(self.path)
        assert self.headers.get('Referer') == 'https://fixture.invalid/'
        assert self.headers.get('Accept-Encoding') == 'identity'
        self.send_response(403 if self.path.endswith('/error') else 200)
        self.send_header('Content-Type', 'text/html' if self.path.endswith('/html') else 'video/mp4')
        if not self.path.endswith('/missing'):
            self.send_header('Content-Length', '100000')
        if self.path.endswith('/partial'):
            self.send_header('Content-Range', 'bytes 0-99999/200000')
        if self.path.endswith('/compressed'):
            self.send_header('Content-Encoding', 'identity')
            self.send_header('Content-Encoding', 'gzip')
        self.end_headers()

    def handle(self):
        try:
            super().handle()
        except (ConnectionResetError, BrokenPipeError):
            pass

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path.startswith('/probe/'):
            self.server.state.setdefault('probe_gets', []).append(self.path)
            self.send_error(405)
            return
        name = urllib.parse.unquote(self.path).rsplit('/', 1)[-1]
        state = self.server.state
        state.setdefault('image_downloads', []).append(name)
        if 'sample_images' in state:
            body = state['sample_images'][name]
        else:
            body = b'x' * (10 * 1024 * 1024 + 1) if name == 'large-body' else name.encode()
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_POST(self):
        if self.headers.get('Transfer-Encoding', '').lower() == 'chunked':
            chunks = []
            while True:
                size = int(self.rfile.readline().strip(), 16)
                if not size:
                    self.rfile.readline()
                    break
                if self.server.state.get('slow_upload') and self.path.lower().endswith('/sendaudio'):
                    remaining = size
                    while remaining:
                        piece = self.rfile.read(min(remaining, 64*1024))
                        if not piece:
                            raise ConnectionError('truncated fixture upload')
                        chunks.append(piece)
                        remaining -= len(piece)
                        self.server.state['upload_read_bytes'] = self.server.state.get('upload_read_bytes',0)+len(piece)
                        time.sleep(len(piece)/(768*1024))
                else:
                    chunks.append(self.rfile.read(size))
                self.rfile.read(2)
            body = b''.join(chunks)
        else:
            body = self.rfile.read(int(self.headers.get('Content-Length', 0)))
        method = self.path.rsplit('/', 1)[-1].lower()
        content_type = self.headers.get('Content-Type', '')
        if 'application/json' in content_type:
            payload = json.loads(body)
        elif 'multipart/form-data' in content_type:
            envelope = BytesParser(policy=default).parsebytes(f'Content-Type: {content_type}\r\n\r\n'.encode() + body)
            payload = {part.get_param('name', header='Content-Disposition'): part.get_payload(decode=True).decode() for part in envelope.iter_parts() if part.get_filename() is None}
        else:
            payload = {key: values[-1] for key, values in urllib.parse.parse_qs(body.decode()).items()}
        for key in ('chat_id', 'message_id', 'reply_markup'):
            if key in payload and isinstance(payload[key], str):
                payload[key] = json.loads(payload[key])
        state = self.server.state
        if method == 'getupdates':
            with state['condition']:
                if not state['updates']:
                    state['condition'].wait(0.2)
                result = state['updates'][:]
                state['updates'].clear()
        elif method == 'getfile':
            name = payload['file_id']
            state.setdefault('image_requests', []).append(name)
            result = {'file_id': name, 'file_unique_id': 'unique-'+name, 'file_path': 'images/'+name}
            if 'sample_images' in state:
                result['file_size'] = len(state['sample_images'][name])
            elif name != 'unknown-size':
                result['file_size'] = 10 * 1024 * 1024 + 1 if name == 'large-meta' else 10
        elif method == 'getme':
            if state.get('delay_setup'):
                state['setup_started'].set()
                state['release_setup'].wait(5)
            result = {'id': 1000, 'is_bot': True, 'first_name': 'Fixture', 'username': 'fixture_bot', 'can_join_groups': True, 'can_read_all_group_messages': False, 'supports_inline_queries': False, 'has_main_web_app': False}
        elif method in ('sendmessage', 'editmessagetext', 'senddocument', 'sendvoice', 'sendaudio', 'sendvideo'):
            with state['condition']:
                if method=='editmessagetext' and '⏱' in payload.get('text',''):
                    state.setdefault('progress_times',[]).append((time.monotonic(),payload['text'],payload['message_id'],state.get('upload_read_bytes',0)))
                rejected = [text for text in state.get('reject_progress', set()) if text in payload.get('text','')]
                if method == 'editmessagetext' and rejected:
                    state.setdefault('progress_rejections', []).extend(rejected)
                    state['condition'].notify_all()
                    response = json.dumps({'ok':False,'error_code':400,'description':'Bad Request: message to edit not found'}).encode()
                    self.send_response(400)
                    self.send_header('Content-Type','application/json')
                    self.send_header('Content-Length',str(len(response)))
                    self.end_headers()
                    self.wfile.write(response)
                    return
                if method == 'editmessagetext' and state.get('reject_duplicate_edits'):
                    key = (payload['chat_id'], payload['message_id'])
                    previous = state.setdefault('last_edits', {}).get(key)
                    if previous == payload:
                        response = json.dumps({'ok':False,'error_code':400,'description':'Bad Request: message is not modified'}).encode()
                        self.send_response(400)
                        self.send_header('Content-Type','application/json')
                        self.send_header('Content-Length',str(len(response)))
                        self.end_headers()
                        self.wfile.write(response)
                        state['duplicate_edits'] = state.get('duplicate_edits',0)+1
                        return
                    state['last_edits'][key] = payload
                state['sequence'] += 1
                result = {'message_id': payload.get('message_id', state['sequence']), 'date': 1700000000, 'chat': {'id': payload.get('chat_id', 42), 'type': 'private'}, 'text': payload.get('text', '')}
                if method in ('senddocument', 'sendvoice', 'sendaudio', 'sendvideo'):
                    result.pop('text', None)
                    field = method[4:]
                    file_id = payload.get(field)
                    if file_id and file_id.startswith('attach://'):
                        file_id = None
                    rejection = state['reject'].pop(file_id, None)
                    if rejection:
                        state['rejections'] += 1
                        error = {'ok': False, 'error_code': 400, 'description': 'Bad Request: wrong file id'} if rejection == 'invalid' else {'ok': False, 'error_code': 401, 'description': 'Unauthorized'}
                        response = json.dumps(error).encode()
                        self.send_response(error['error_code'])
                        self.send_header('Content-Type', 'application/json')
                        self.send_header('Content-Length', str(len(response)))
                        self.end_headers()
                        self.wfile.write(response)
                        return
                    if file_id:
                        assert file_id in state['files'], f'unknown cached file ID: {file_id}'
                        assert state['files'][file_id] == field, 'cached ID changed media type'
                    else:
                        file_id = f'{field}-file-{state["sequence"]}'
                        state['files'][file_id] = field
                        state['uploads'] += 1
                    if not state.pop('omit_media_once', False):
                        result[field] = {'file_id': file_id, 'file_unique_id': 'unique-'+file_id, 'file_size': 21, 'duration': 1, 'width': 1280, 'height': 720, 'mime_type': {'voice': 'audio/ogg', 'audio': 'audio/mp4', 'video': 'video/mp4', 'document': 'video/x-matroska'}[field]}
                        if field == 'audio':
                            result[field]['title'] = payload.get('title')
                if 'reply_markup' in payload:
                    result['reply_markup'] = payload['reply_markup']
                state['events'].append((method, payload, result, body))
                state['condition'].notify_all()
        elif method == 'setwebhook':
            state['webhook'] = payload
            result = True
        else:
            result = True
            if method == 'answercallbackquery':
                with state['condition']:
                    state['events'].append((method, payload, {}, body))
                    state['condition'].notify_all()
        response = json.dumps({'ok': True, 'result': result}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(response)))
        self.end_headers()
        try:
            self.wfile.write(response)
        except BrokenPipeError:
            pass


def smoke(binary, mode, startup_stop=False, images=False, vision=False, queue_env=None, broker_control=None, status_only=False, public_access=False, progress_only=False, real_video_root=None):
    state = {'condition': threading.Condition(), 'updates': [], 'events': [], 'sequence': 100, 'webhook': None, 'delay_setup': startup_stop, 'setup_started': threading.Event(), 'release_setup': threading.Event(), 'files': {}, 'uploads': 0, 'reject': {}, 'rejections': 0}
    state['reject_duplicate_edits'] = vision
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Telegram)
    if progress_only:
        server.socket.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 64*1024)
    server.daemon_threads = True
    server.state = state
    threading.Thread(target=server.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix='yt-dlp-tg-smoke-') as scratch:
        root = Path(scratch)
        fixture = root / 'yt-dlp'
        fixture.write_text(FAKE_YTDLP)
        fixture.chmod(0o700)
        ffprobe = root / 'ffprobe'
        if real_video_root is None:
            ffprobe.write_text(FAKE_FFPROBE)
            ffprobe.chmod(0o700)
        ffmpeg = root / 'ffmpeg'
        if real_video_root is None:
            ffmpeg.write_text(FAKE_FFMPEG)
            ffmpeg.chmod(0o700)
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            webhook_port = sock.getsockname()[1]
        env = {**os.environ, 'BOT_TOKEN': '1000:fixture', 'ALLOWED_USER_IDS': '42,7', 'TG_API_BASE': f'http://127.0.0.1:{server.server_port}', 'YTDLP_PATH': str(fixture), 'DOWNLOAD_DIR': str(root / 'jobs'), 'MODE': mode, 'LISTEN_ADDR': f'127.0.0.1:{webhook_port}', 'WEBHOOK_PUBLIC_URL': 'https://fixture.invalid', 'NO_PROXY': '127.0.0.1,localhost'}
        env['PATH'] = str(root)+os.pathsep+env['PATH']
        if public_access:
            env['ALLOWED_USER_IDS'] = 'all'
        env['FAKE_HEADER_ORIGIN'] = f'http://127.0.0.1:{server.server_port}'
        if real_video_root is not None:
            env['FAKE_REAL_VIDEO_DIR'] = str(real_video_root)
        env.pop('TG_PROXY', None)
        env.pop('YTDLP_PROXY', None)
        for key in tuple(env):
            if key.startswith('IGGY_'):
                env.pop(key)
        if queue_env:
            env.update(queue_env)
        for key in ('OCR_PYTHON', 'OCR_SCRIPT', 'OCR_TIMEOUT_SECS', 'OCR_CONFIG', 'OCR_SECRETS_FILE'):
            env.pop(key, None)
        if images or vision:
            private_jobs = root / 'private-jobs'
            private_jobs.mkdir(mode=0o700)
            (root / 'jobs').symlink_to(private_jobs, target_is_directory=True)
            helper = root / 'fake-ocr.py'
            helper.write_text('''import json, os, pathlib, sys, time
name = pathlib.Path(sys.argv[sys.argv.index('--image')+1]).read_text()
with open(os.environ['FAKE_OCR_CALLS'], 'a') as calls:
    calls.write(name+'\\n')
if name == 'empty': sys.exit(2)
if name == 'invalid': sys.exit(3)
if name == 'too-many-pixels': sys.exit(4)
if name == 'timeout': time.sleep(5)
if name == 'bad-json': print('bad-json')
elif name == 'overflow': print('x'*70000)
else: print(json.dumps({'query':'FORHILL & JNFOUND ATLAS'}))
''')
            env.update(OCR_PYTHON=sys.executable, OCR_SCRIPT=str(helper), OCR_TIMEOUT_SECS='1', FAKE_OCR_CALLS=str(root/'ocr-calls'))
            if vision:
                helper.write_text('''import json, os, pathlib, sys
if '--render' in sys.argv:
    artist = next(arg.split('=',1)[1] for arg in sys.argv if arg.startswith('--artist='))
    title = next(arg.split('=',1)[1] for arg in sys.argv if arg.startswith('--title='))
else:
    name = pathlib.Path(sys.argv[sys.argv.index('--image')+1]).read_text()
    with open(os.environ['FAKE_OCR_CALLS'], 'a') as calls: calls.write(name+'\\n')
    if name == 'rejected':
        print(json.dumps({'kind':'rejected','message':'No track could be identified.'})); sys.exit(0)
    artist, title = 'Forhill & Unfound', 'Atlas'
    if name == 'no-match': artist, title = 'YKU', 'Gradient'
    if name == 'search-failed': artist, title = 'LookupFail', 'Track'
print(json.dumps({'kind':'track','artist':artist,'title':title,'plain_query':artist+' '+title,'quoted_query':'"'+artist+'" "'+title+'"'}))
''')
                env['OCR_CONFIG'] = str(root/'policy.json')
        env['CACHE_PATH'] = str(root / 'state/files.redb')
        env['FAKE_DOWNLOADS'] = str(root / 'downloads.jsonl')
        env['FAKE_CALLS'] = str(root / 'calls.jsonl')
        env['FAKE_STARTED'] = str(root / 'started')
        env['FAKE_GATE'] = str(root / 'gate')
        env['FAKE_YT_STARTED'] = str(root / 'youtube-started')
        env['FAKE_YT_GATE'] = str(root / 'youtube-gate')
        with (root / 'bot.log').open('w+') as log:
            process = subprocess.Popen([binary], env=env, stdout=log, stderr=log)
            update_id = 1

            def wait(method, predicate=lambda p: True):
                deadline = time.monotonic() + (45 if progress_only else 15)
                with state['condition']:
                    while time.monotonic() < deadline:
                        for index, event in enumerate(state['events']):
                            if event[0] == method and predicate(event[1]):
                                return state['events'].pop(index)
                        if process.poll() is not None:
                            break
                        state['condition'].wait(0.1)
                log.seek(0)
                raise AssertionError(f'{mode}: missing {method}; bot log: {log.read()}')

            def send(update):
                nonlocal update_id
                update['update_id'] = update_id
                update_id += 1
                if mode == 'polling':
                    with state['condition']:
                        state['updates'].append(update)
                        state['condition'].notify_all()
                else:
                    deadline = time.monotonic() + 10
                    while state['webhook'] is None and time.monotonic() < deadline:
                        time.sleep(0.05)
                    assert state['webhook'] is not None, 'setWebhook was not called'
                    request = urllib.request.Request(f'http://127.0.0.1:{webhook_port}/tg-webhook', json.dumps(update).encode(), {'Content-Type': 'application/json', 'X-Telegram-Bot-Api-Secret-Token': state['webhook']['secret_token']})
                    for attempt in range(20):
                        try:
                            with urllib.request.urlopen(request, timeout=2) as response:
                                assert response.status == 200
                            break
                        except urllib.error.URLError as error:
                            if not isinstance(error.reason, ConnectionRefusedError):
                                raise
                            time.sleep(0.05)
                    else:
                        raise AssertionError('webhook listener did not become ready')

            def user(id=42, language='en'):
                return {'id': id, 'is_bot': False, 'first_name': 'User', 'language_code': language}

            def message(text, id=42, language='en', chat=42):
                send({'message': {'message_id': update_id, 'date': 1700000000, 'chat': {'id': chat, 'type': 'private'}, 'from': user(id, language), 'text': text}})

            def photo(id=42, language='en', name='photo-fixture', size=None):
                item = {'file_id': name, 'file_unique_id': 'photo-unique', 'width': 441, 'height': 160}
                if size is not None:
                    item['file_size'] = size
                send({'message': {'message_id': update_id, 'date': 1700000000, 'chat': {'id': id, 'type': 'private'}, 'from': user(id, language), 'photo': [item]}})

            def menu(chat=42):
                return wait('editmessagetext', lambda p: p.get('chat_id') == chat and p.get('reply_markup', {}).get('inline_keyboard'))

            def press(event, action, value=0, id=42):
                rows = event[1]['reply_markup']['inline_keyboard']
                button = next(button for row in rows for button in row if button.get('callback_data', '').endswith(f':{action}:{value}'))
                sent = event[2]
                send({'callback_query': {'id': str(update_id), 'from': user(id), 'chat_instance': 'fixture', 'message': sent, 'data': button['callback_data']}})

            def pick_video(event, height=720, id=42):
                button = next(button for row in event[1]['reply_markup']['inline_keyboard'] for button in row if button['text'].startswith(str(height)+'p '))
                action, value = button['callback_data'].split(':')[-2:]
                press(event, action, int(value), id=id)

            def downloads():
                path = root / 'downloads.jsonl'
                return len(path.read_text().splitlines()) if path.exists() else 0

            def active(payload, phase):
                return '▶ '+phase in payload.get('text','').splitlines()

            def choose(url, action, height=720):
                message(url)
                chosen = menu()
                press(chosen, action)
                if action == 'v':
                    qualities = menu()
                    pick_video(qualities,height)
                return chosen

            try:
                if real_video_root is not None:
                    from video_smoke import verify_upload
                    manifest=json.loads((real_video_root/'manifest.json').read_text())
                    for key,record in manifest.items():
                        before_uploads=state['uploads']
                        choose('https://media.invalid/'+key,'v',record['height'])
                        if record.get('rejected'):
                            wait('editmessagetext',lambda p:p.get('text','').startswith('The request failed.'))
                            assert state['uploads']==before_uploads, 'unsupported video reached Telegram'
                            continue
                        delivered=wait('sendvideo')
                        verify_upload(real_video_root/record['file'],delivered,record,root)
                        wait('editmessagetext',lambda p:p.get('text')=='Sent.')
                        before_downloads=downloads()
                        choose('https://media.invalid/'+key,'v',record['height'])
                        reused=wait('sendvideo')
                        assert reused[1]['video']==delivered[2]['video']['file_id']
                        wait('editmessagetext',lambda p:p.get('text')=='Sent.')
                        assert downloads()==before_downloads, 'MP4 cache hit downloaded again'
                    assert not any(event[0]=='senddocument' for event in state['events'])
                    assert not list((root/'jobs').iterdir()), 'MP4 job scratch leaked'
                    process.send_signal(signal.SIGTERM);assert process.wait(timeout=10)==0
                    print(f'{mode}: real FFmpeg MP4 codecs, copy/conversion, faststart, limits and typed cache passed')
                    return
                if progress_only:
                    state['slow_upload'] = True
                    choose('https://media.invalid/slow-progress','m')
                    wait('editmessagetext', lambda p: active(p,'Downloading…'))
                    wait('sendaudio')
                    wait('editmessagetext',lambda p:p.get('text')=='Sent.')
                    events = [event for event in state['events'] if event[0]=='editmessagetext' and '⏱' in event[1].get('text','')]
                    assert events, 'no combined progress updates'
                    assert all('Downloading…' in event[1]['text'] and 'Sending…' in event[1]['text'] for event in events)
                    assert len({event[1]['message_id'] for event in events})==1, 'progress created another status message'
                    sends = [event[1]['text'] for event in events if active(event[1],'Sending…')]
                    percents = [int(re.search(r'(\d+)%',text.split('▶ Sending…',1)[1])[1]) for text in sends if '%' in text.split('▶ Sending…',1)[1]]
                    assert any(0<value<100 for value in percents) and len(set(percents))>=2, ('upload did not stream measured progress',percents,[(round(event[0]-state['progress_times'][0][0],2),event[3]) for event in state['progress_times']])
                    cadence=[]
                    for previous,current in zip(state['progress_times'],state['progress_times'][1:]):
                        old_phase='sending' if '▶ Sending…' in previous[1] else 'downloading'
                        phase='sending' if '▶ Sending…' in current[1] else 'downloading'
                        if old_phase==phase:
                            cadence.append(current[0]-previous[0])
                    assert cadence and all(2.75<=delta<=4.75 for delta in cadence), cadence
                    assert state['uploads']==downloads()==1
                    for url in ('opus-only','aac-mismatch'):
                        choose('https://media.invalid/'+url,'m')
                        alternative=menu()
                        assert 'Native AAC is unavailable' in alternative[1]['text']
                        press(alternative,'a',id=7)
                        wait('answercallbackquery',lambda p:'expired' in p.get('text',''))
                        press(alternative,'a')
                        wait('sendvoice')
                        wait('editmessagetext',lambda p:p.get('text')=='Sent.')
                    process.send_signal(signal.SIGTERM); assert process.wait(timeout=10)==0
                    print(f'{mode}: one-message measured progress and owned native-AAC/Opus fallback passed')
                    return
                if public_access:
                    message('/start', id=99, chat=99)
                    assert 'Send a video link' in wait('sendmessage', lambda p: p.get('chat_id') == 99)[1]['text']
                    message('https://media.invalid/public', id=99, chat=99)
                    selected = menu(chat=99)
                    press(selected, 'm', id=7)
                    wait('answercallbackquery', lambda p: 'expired' in p.get('text', ''))
                    assert state['uploads'] == downloads() == 0, 'public access authorized a foreign callback'
                    press(selected, 'm', id=99)
                    first = wait('sendaudio', lambda p: p.get('chat_id') == 99)
                    wait('editmessagetext', lambda p: p.get('chat_id') == 99 and p.get('text') == 'Sent.')
                    message('https://media.invalid/public')
                    shared = menu()
                    press(shared, 'm')
                    reused = wait('sendaudio', lambda p: p.get('chat_id') == 42)
                    wait('editmessagetext', lambda p: p.get('text') == 'Sent.')
                    assert reused[1]['audio'] == first[2]['audio']['file_id']
                    assert state['uploads'] == downloads() == 1, 'public users did not share the typed cache'
                    message('https://media.invalid/header-sizes')
                    header_menu = menu()
                    press(header_menu, 'v')
                    qualities = menu()
                    rows = qualities[1]['reply_markup']['inline_keyboard']
                    labels = [button['text'] for row in rows for button in row]
                    assert any(label.startswith('640p ') and 'unavailable' not in label for label in labels)
                    for height in (1080,720,480,360,240):
                        assert any(label.startswith(str(height)+'p ') and 'unavailable' in label for label in labels)
                    assert len(state['head_requests']) == 7
                    assert not state.get('probe_gets'), 'menu discovery fetched a media body'
                    before = downloads()
                    pick_video(qualities, height=720)
                    wait('answercallbackquery')
                    assert downloads() == before
                    pick_video(qualities, height=640)
                    wait('sendvideo')
                    wait('editmessagetext', lambda p: p.get('text') == 'Sent.')
                    message('https://www.instagram.com/reel/Fixture_123/?tracking=DO_NOT_LOG_ME#DO_NOT_LOG_ME')
                    menu()
                    process.send_signal(signal.SIGTERM)
                    assert process.wait(timeout=10) == 0
                    log.seek(0)
                    logged = re.sub(r'\x1b\[[0-9;]*m', '', log.read())
                    assert 'DO_NOT_LOG_ME' not in logged
                    assert '/probe/audio' not in logged
                    assert 'access="all"' in logged
                    lines = logged.splitlines()
                    intake = next(line for line in lines if 'Fixture_123' in line and 'stage="accepted"' in line)
                    request = re.search(r'request_id=(\d+)', intake)[1]
                    assert any('Fixture_123/' in line and 'stage="probe"' in line and 'outcome="ok"' in line and 'request_id='+request in line for line in lines)
                    for availability in ('available','unknown_size'):
                        assert any('stage="video_options"' in line and 'availability="'+availability+'"' in line for line in lines)
                    assert any('stage="video_selection"' in line and 'reason="unknown_size"' in line for line in lines)
                    requested = next(line for line in lines if 'stage="download"' in line)
                    request = re.search(r'request_id=(\d+)', requested)[1]
                    if queue_env:
                        queued = next(line for line in lines if 'stage="queue"' in line and 'request_id='+request in line)
                        job = re.search(r'job_id=([0-9a-f]+)', queued)[1]
                        correlation = 'job_id='+job
                    else:
                        correlation = 'request_id='+request
                    assert any('stage="delivery"' in line and 'outcome="sent"' in line and correlation in line for line in lines)
                    print(f'{mode}: public access/ownership/cache, safe header sizes and correlated redacted diagnostics passed')
                    return
                if status_only:
                    message('https://media.invalid/waiting-status')
                    selected = menu()
                    gate = root/'gate'
                    gate.write_text('hold metadata slot')
                    message('https://media.invalid/delayed', id=7, chat=7)
                    wait('editmessagetext', lambda p: p.get('chat_id') == 7 and p.get('text') == 'Checking the link…')
                    state['reject_progress'] = {'Another request is running. Waiting for a free slot…'}
                    try:
                        press(selected, 'm')
                        deadline = time.monotonic() + 10
                        with state['condition']:
                            while not state.get('progress_rejections') and time.monotonic() < deadline:
                                state['condition'].wait(.05)
                        assert state.get('progress_rejections'), 'waiting edit was not attempted'
                    finally:
                        gate.unlink(missing_ok=True)
                    terminal = wait('editmessagetext', lambda p: p.get('chat_id') == 42 and p.get('text') in ('Sent.', 'The request failed. Try another link or search query.'))
                    assert terminal[1]['text'] == 'Sent.', 'cosmetic waiting failure consumed the download'
                    wait('sendaudio')
                    menu(chat=7)
                    state['reject_progress'] = {'Queuing download…', 'Downloading…', 'Sending…'}
                    choose('https://media.invalid/progress-status', 'm')
                    first = wait('sendaudio')
                    wait('editmessagetext', lambda p: p.get('text') == 'Sent.')
                    before = downloads()
                    choose('https://media.invalid/progress-status', 'm')
                    reused = wait('sendaudio')
                    wait('editmessagetext', lambda p: p.get('text') == 'Sent.')
                    assert reused[1]['audio'] == first[2]['audio']['file_id'] and downloads() == before
                    assert 'Downloading…' in state['progress_rejections'] and 'Sending…' in state['progress_rejections']
                    if queue_env:
                        assert 'Queuing download…' in state['progress_rejections']
                    process.send_signal(signal.SIGTERM)
                    assert process.wait(timeout=10) == 0
                    print(f'{mode}: failed cosmetic progress edits preserve cold/cached delivery')
                    return
                if vision:
                    photo(name='good')
                    wait('sendmessage', lambda p: p.get('text') == 'Image received…')
                    wait('editmessagetext', lambda p: p.get('text') == 'Searching…')
                    preview = menu()
                    assert preview[1]['text'].startswith('Artist: Forhill & Unfound\nTitle: Atlas\n')
                    assert 'Unfound & Forhill - Atlas\nhttps://media.invalid/track2' in preview[1]['text']
                    assert json.loads((root/'calls.jsonl').read_text().splitlines()[-1])[-1] == 'ytsearch5:"Forhill & Unfound" "Atlas"'
                    assert state['uploads'] == downloads() == 0, 'automatic lookup downloaded media'
                    before = (root/'calls.jsonl').read_text()
                    press(preview, 'm', id=7)
                    wait('answercallbackquery', lambda p: 'expired' in p.get('text',''))
                    assert (root/'calls.jsonl').read_text() == before
                    press(preview, 'b')
                    results = menu()
                    assert 'Title: Atlas' in results[1]['text']
                    assert 'Search results' in results[1]['text']
                    press(results, 'q', 1)
                    suggested = menu()
                    assert json.loads((root/'calls.jsonl').read_text().splitlines()[-1])[-1] == 'ytsearch5:"Forhill & Unfound" "Atlas"'
                    press(suggested, 'e', 0)
                    prompt = wait('sendmessage', lambda p: p.get('reply_markup', {}).get('force_reply'))
                    editing = menu()

                    def correction(value, prompt_event=prompt, owner=42):
                        send({'message': {'message_id':update_id,'date':1700000000,'chat':{'id':42,'type':'private'},'from':user(owner),'text':value,'reply_to_message':prompt_event[2]}})

                    correction('Hijacked artist', owner=7)
                    wait('sendmessage', lambda p: 'expired' in p.get('text',''))
                    correction('   ')
                    retry = wait('sendmessage', lambda p: p.get('reply_markup',{}).get('force_reply'))
                    assert 'non-empty' in retry[1]['text']
                    correction('Forhill & Unfound', retry)
                    corrected = menu()
                    assert 'Artist: Forhill & Unfound' in corrected[1]['text']
                    correction('Replay', retry)
                    wait('sendmessage', lambda p: 'expired' in p.get('text',''))
                    press(corrected, 'e', 1)
                    title_prompt = wait('sendmessage', lambda p: p.get('reply_markup',{}).get('force_reply'))
                    current = menu()
                    before = (root/'calls.jsonl').read_text()
                    correction('-Literal {artist} title', title_prompt)
                    final = menu()
                    assert 'Title: -Literal {artist} title' in final[1]['text']
                    assert (root/'calls.jsonl').read_text() == before, 'field edit searched implicitly'
                    assert (root/'ocr-calls').read_text().splitlines() == ['good'], 'field edit repeated inference'
                    press(current, 'q')
                    wait('answercallbackquery', lambda p: 'expired' in p.get('text',''))
                    press(final, 'q')
                    assert 'Title: -Literal {artist} title' in menu()[1]['text']
                    for name, expected in [('no-match','Search results'),('search-failed','The request failed')]:
                        photo(name=name)
                        fallback = menu()
                        assert expected in fallback[1]['text']
                        assert not any(button['callback_data'].split(':')[-2] in ('m','v') for row in fallback[1]['reply_markup']['inline_keyboard'] for button in row if 'callback_data' in button)
                        assert state['uploads'] == downloads() == 0
                    before = (root/'calls.jsonl').read_text()
                    photo(name='rejected')
                    wait('editmessagetext', lambda p: p.get('text') == 'No track could be identified.')
                    assert (root/'calls.jsonl').read_text() == before, 'recognition rejection searched'
                    for action in ('m','m','v'):
                        before_downloads = downloads()
                        photo(name='good')
                        quick = menu()
                        press(quick, action)
                        if action == 'v':
                            qualities = menu()
                            pick_video(qualities)
                        delivered = wait('sendaudio' if action == 'm' else 'sendvideo')
                        wait('editmessagetext', lambda p: p.get('text') == 'Sent.')
                        if action == 'm' and before_downloads:
                            assert downloads() == before_downloads, 'quick music did not reuse cache'
                        else:
                            assert downloads() == before_downloads+1
                            assert json.loads((root/'downloads.jsonl').read_text().splitlines()[-1])[-1] == 'https://media.invalid/track2'
                        assert delivered[2].get('audio') or delivered[2].get('video')
                    press(quick, 'v' if action == 'v' else 'm')
                    wait('answercallbackquery', lambda p: 'expired' in p.get('text',''))
                    photo(name='good')
                    quick = menu()
                    press(quick, 'f')
                    formats = menu()
                    assert any(button.get('callback_data','').endswith(':a:0') for row in formats[1]['reply_markup']['inline_keyboard'] for button in row)
                    press(formats, 'b')
                    assert 'Search results' in menu()[1]['text']
                    assert not list((root/'jobs').iterdir())
                    assert state['uploads'] == 2
                    assert state.get('duplicate_edits',0) == 0
                    process.send_signal(signal.SIGTERM)
                    assert process.wait(timeout=10) == 0
                    print(f'{mode}: automatic matching, direct music/video, cache reuse, fallback, owned corrections and rejection passed')
                    return
                if images:
                    photo(id=99, name='good')
                    assert 'private' in wait('sendmessage')[1]['text']
                    assert not state.get('image_requests')
                    before_calls = (root/'calls.jsonl').read_text()
                    photo(name='good')
                    received = wait('sendmessage', lambda p: p.get('text') == 'Image received…')
                    wait('editmessagetext', lambda p: p.get('text') == 'Downloading the image…')
                    wait('editmessagetext', lambda p: p.get('text') == 'Reading text from the image…')
                    preview = menu()
                    assert preview[2]['message_id'] == received[2]['message_id']
                    assert 'FORHILL & JNFOUND ATLAS' in preview[1]['text']
                    assert (root/'calls.jsonl').read_text() == before_calls, 'photo searched before confirmation'
                    press(preview, 'q', id=7)
                    assert 'expired' in wait('answercallbackquery', lambda p: 'text' in p)[1]['text']
                    press(preview, 'q')
                    assert 'Search results' in menu()[1]['text']
                    message('corrected artist title')
                    assert 'Search results' in menu()[1]['text']

                    for name, expected in [('unknown-size','Recognized text:'),('empty','No usable text'),('invalid','Could not read'),('too-many-pixels','too large'),('bad-json','Could not read'),('overflow','Could not read'),('timeout','timed out'),('large-meta','too large'),('large-body','too large')]:
                        photo(name=name)
                        wait('sendmessage', lambda p: p.get('text') == 'Image received…')
                        try:
                            wait('editmessagetext', lambda p: expected in p.get('text',''))
                        except AssertionError as error:
                            recent = [(event[0], event[1].get('text')) for event in state['events'][-12:]]
                            calls = (root/'ocr-calls').read_text() if (root/'ocr-calls').exists() else None
                            raise AssertionError(f'image case={name}, events={recent}, helper_calls={calls}: {error}') from error
                        assert not list((root/'jobs').iterdir()), name+' image scratch leaked'
                    requests = len(state['image_requests'])
                    photo(name='too-big', size=10*1024*1024+1)
                    wait('editmessagetext', lambda p: 'too large' in p.get('text',''))
                    assert len(state['image_requests']) == requests
                    send({'message': {'message_id': update_id, 'date':1700000000, 'chat':{'id':42,'type':'private'}, 'from':user(), 'document':{'file_id':'good-doc','file_unique_id':'doc-unique','file_size':10,'mime_type':'image/png'}}})
                    assert 'Recognized text' in menu()[1]['text']
                    process.send_signal(signal.SIGTERM)
                    assert process.wait(timeout=10) == 0
                    print(f'{mode}: authorized image fetch, explicit preview/search, corrected query, bounded/error/timeout cases and cleanup passed')
                    return
                if startup_stop:
                    assert state['setup_started'].wait(10), 'delayed getMe was not reached'
                    process.send_signal(signal.SIGTERM)
                    assert process.wait(timeout=2) == 0
                    print(f'{mode}: SIGTERM during delayed startup exits cleanly')
                    return
                message('/id', 99)
                assert wait('sendmessage')[1]['text'] == '99'
                message('/start', language='ru')
                assert 'Отправьте' in wait('sendmessage')[1]['text']
                for locale in ('en','zh','hi','es','ar','fr','pt','ru'):
                    message('/help', language=locale)
                    help_text = wait('sendmessage')[1]['text']
                    assert all(value in help_text for value in ('50','1080p','900','300','Premium'))
                    assert '{limit}' not in help_text and '{download}' not in help_text
                message('query', 99)
                assert 'private' in wait('sendmessage')[1]['text']
                calls_before_photo = (root / 'calls.jsonl').read_text()
                photo()
                assert 'Image recognition' in wait('sendmessage')[1]['text']
                photo(language='ru')
                assert 'не включено' in wait('sendmessage')[1]['text']
                photo(id=99)
                assert 'private' in wait('sendmessage')[1]['text']
                assert (root / 'calls.jsonl').read_text() == calls_before_photo, 'photo started an extractor'

                (root / 'gate').write_text('blocked')
                message('query delayed')
                received = wait('sendmessage', lambda p: p.get('text') == 'Request received…')
                searching = wait('editmessagetext', lambda p: p.get('text') == 'Searching…' and p.get('message_id') == received[2]['message_id'])
                deadline = time.monotonic() + 10
                while not (root / 'started').exists() and time.monotonic() < deadline:
                    time.sleep(0.02)
                assert (root / 'started').exists(), 'delayed extractor never started'
                message('query', id=7, chat=7)
                received7 = wait('sendmessage', lambda p: p.get('chat_id') == 7 and p.get('text') == 'Request received…')
                waiting = wait('editmessagetext', lambda p: p.get('chat_id') == 7 and p.get('text') == 'Another request is running. Waiting for a free slot…')
                assert waiting[1]['message_id'] == received7[2]['message_id']
                assert not searching[1]['reply_markup']['inline_keyboard']
                assert not waiting[1]['reply_markup']['inline_keyboard']
                assert sum('query' in line for line in (root / 'calls.jsonl').read_text().splitlines()) == 1, 'queued work started before slot release'
                (root / 'gate').unlink()
                assert menu()[2]['message_id'] == received[2]['message_id']
                assert menu(chat=7)[2]['message_id'] == received7[2]['message_id']

                (root / 'started').unlink()
                (root / 'gate').write_text('blocked')
                message('https://media.invalid/delayed')
                link_received = wait('sendmessage', lambda p: p.get('text') == 'Request received…' and p.get('chat_id') == 42)
                wait('editmessagetext', lambda p: p.get('text') == 'Checking the link…' and p.get('message_id') == link_received[2]['message_id'])
                deadline = time.monotonic() + 10
                while not (root / 'started').exists() and time.monotonic() < deadline:
                    time.sleep(0.02)
                assert (root / 'started').exists(), 'delayed link probe never started'
                (root / 'gate').unlink()
                assert menu()[2]['message_id'] == link_received[2]['message_id']

                message('query')
                results = menu()
                assert 'youtube' in results[1]['text']
                press(results, 'r', id=7)
                assert 'expired' in wait('answercallbackquery', lambda p: 'text' in p)[1]['text']
                press(results, 'p')
                providers = menu()
                press(providers, 'p', 1)
                providers2 = menu()
                assert providers2[1]['reply_markup'] != providers[1]['reply_markup']
                press(providers2, 'h', 5)
                other = menu()
                assert 'soundcloud' in other[1]['text']
                press(other, 's', 1)
                page2 = menu()
                assert 'Result 6' in str(page2[1]['reply_markup'])
                press(page2, 'r')
                formats = menu()
                press(formats, 'a')
                wait('editmessagetext', lambda p: active(p, 'Downloading…'))
                uploaded = wait('sendvoice')
                wait('editmessagetext', lambda p: p.get('text') == 'Sent.' and p.get('message_id') == formats[2]['message_id'])
                assert b'.ogg"' in uploaded[3]
                assert 'voice' in uploaded[2]
                before = downloads()
                choose('https://media.invalid/track6', 'a')
                wait('editmessagetext', lambda p: active(p, 'Sending…'))
                reused = wait('sendvoice')
                assert reused[1]['voice'] == uploaded[2]['voice']['file_id']
                assert downloads() == before, 'cache hit downloaded audio again'

                choose('https://media.invalid/music', 'm')
                wait('editmessagetext', lambda p: active(p, 'Downloading…'))
                music = wait('sendaudio')
                assert b'.m4a"' in music[3]
                assert music[1]['title'] == 'Direct media'
                assert downloads() == before + 1
                state['reject'][music[2]['audio']['file_id']] = 'invalid'
                choose('https://media.invalid/music', 'm')
                wait('editmessagetext', lambda p: active(p, 'Sending…'))
                wait('editmessagetext', lambda p: active(p, 'Downloading…'))
                recovered = wait('sendaudio')
                assert downloads() == before + 2 and state['rejections'] == 1
                assert recovered[2]['audio']['file_id'] != music[2]['audio']['file_id']
                state['reject'][recovered[2]['audio']['file_id']] = 'auth'
                choose('https://media.invalid/music', 'm')
                wait('editmessagetext', lambda p: active(p, 'Sending…'))
                wait('editmessagetext', lambda p: p.get('text') == 'The request failed. Try another link or search query.')
                assert downloads() == before + 2, 'auth failure triggered a download'
                choose('https://media.invalid/music', 'm')
                wait('editmessagetext', lambda p: active(p, 'Sending…'))
                assert wait('sendaudio')[1]['audio'] == recovered[2]['audio']['file_id']
                assert downloads() == before + 2, 'auth failure evicted a valid cached ID'
                for audio_action, method in [('a','sendvoice'),('m','sendaudio')]:
                    combined = choose('https://media.invalid/combined-audio', audio_action)
                    wait(method)
                    wait('editmessagetext', lambda p: p.get('text') == 'Sent.' and p.get('message_id') == combined[2]['message_id'])
                    assert not list((root/'jobs').iterdir()), 'combined audio cleanup leaked files'
                message('https://media.invalid/direct')
                direct = menu()
                assert 'Choose a format' in direct[1]['text']
                audio_buttons = direct[1]['reply_markup']['inline_keyboard'][0]
                assert 'AAC' in audio_buttons[0]['text'] and 'Opus' in audio_buttons[1]['text'], 'native music must precede voice'
                press(direct, 'v')
                qualities = menu()
                assert any('1080p' in button['text'] and 'unavailable' in button['text'] and '>50' in button['text'] for row in qualities[1]['reply_markup']['inline_keyboard'] for button in row)
                before_choices = (root/'calls.jsonl').read_text()
                before_downloads = downloads()
                pick_video(qualities, 1080)
                wait('answercallbackquery', lambda p: 'upload limit' in p.get('text',''))
                pick_video(qualities, 360)
                wait('answercallbackquery', lambda p: 'size unknown' in p.get('text',''))
                pick_video(qualities, 720, id=7)
                wait('answercallbackquery', lambda p: 'expired' in p.get('text',''))
                assert (root/'calls.jsonl').read_text() == before_choices and downloads() == before_downloads
                pick_video(qualities, 720)
                wait('editmessagetext', lambda p: active(p, 'Downloading…'))
                video = wait('sendvideo')
                assert '[format_id=v720][height=720]' in json.loads((root/'downloads.jsonl').read_text().splitlines()[-1])[json.loads((root/'downloads.jsonl').read_text().splitlines()[-1]).index('--format')+1]
                assert b'.mp4"' in video[3]
                assert video[1]['supports_streaming'] is True or video[1]['supports_streaming'] == 'true'
                before = downloads()
                choose('https://media.invalid/direct', 'v')
                wait('editmessagetext', lambda p: active(p, 'Sending…'))
                assert wait('sendvideo')[1]['video'] == video[2]['video']['file_id']
                assert downloads() == before
                message('https://media.invalid/direct')
                press(menu(), 'v')
                choices480 = menu()
                pick_video(choices480, 480)
                video480 = wait('sendvideo')
                wait('editmessagetext', lambda p: p.get('text') == 'Sent.')
                assert video480[2]['video']['file_id'] != video[2]['video']['file_id']
                assert downloads() == before + 1, 'different resolution reused the previous file'
                uploads_before = state['uploads']
                message('https://media.invalid/underestimated')
                press(menu(), 'v')
                pick_video(menu(), 720)
                wait('editmessagetext', lambda p: p.get('text') == "The file exceeds this bot's upload limit.")
                assert state['uploads'] == uploads_before, 'oversized output reached Telegram'
                assert not list((root/'jobs').iterdir()), 'oversize cleanup leaked files'
                message('https://media.invalid/format-error')
                press(menu(), 'v')
                failed_video = menu()
                assert 'Could not check video sizes' in failed_video[1]['text']
                press(failed_video, 'm')
                wait('sendaudio')
                wait('editmessagetext', lambda p: p.get('text') == 'Sent.')
                choose('https://media.invalid/movie.mkv', 'v')
                wait('editmessagetext', lambda p: active(p, 'Downloading…'))
                assert b'.mp4"' in wait('sendvideo')[3]
                assert not any(event[0]=='senddocument' for event in state['events'])

                before = downloads()
                uploads = state['uploads']
                message('https://youtu.be/dQw4w9WgXcQ?si=mobile&t=60', chat=42)
                menu42 = menu(chat=42)
                message('https://www.youtube.com/watch?v=dQw4w9WgXcQ&si=desktop#start', id=7, chat=7)
                menu7 = menu(chat=7)
                gate = root/'youtube-gate'
                gate.write_text('hold first download')
                try:
                    press(menu42, 'm')
                    wait('editmessagetext', lambda p: active(p, 'Downloading…') and p.get('message_id') == menu42[2]['message_id'])
                    deadline = time.monotonic() + 10
                    while not (root/'youtube-started').exists() and time.monotonic() < deadline:
                        time.sleep(.02)
                    assert (root/'youtube-started').exists(), 'first cold download never started'
                    if broker_control:
                        broker_control.stop()
                    press(menu7, 'm', id=7)
                    waiting = 'Queuing download…' if queue_env else 'Another request is running. Waiting for a free slot…'
                    wait('editmessagetext', lambda p: p.get('chat_id') == 7 and p.get('text') == waiting)
                    if broker_control:
                        time.sleep(1)
                        broker_control.start()
                    assert downloads() == before + 1 and state['uploads'] == uploads, 'second request downloaded while the first was unfinished'
                finally:
                    gate.unlink(missing_ok=True)
                first = wait('sendaudio', lambda p: p.get('chat_id') == 42)
                second = wait('sendaudio', lambda p: p.get('chat_id') == 7)
                shared_youtube_id = first[2]['audio']['file_id']
                assert shared_youtube_id == second[2]['audio']['file_id'], 'YouTube aliases split the cache identity'
                assert second[1]['audio'] == shared_youtube_id, 'second request uploaded instead of sending the stored Telegram ID'
                assert downloads() == before + 1 and state['uploads'] == uploads + 1, 'concurrent miss duplicated work'
                message('https://media.invalid/unsupported')
                fallback = menu()
                assert 'Search results' in fallback[1]['text']
                process.send_signal(signal.SIGTERM)
                assert process.wait(timeout=10) == 0
                assert not list((root / 'jobs').iterdir()), 'job files leaked'
                before = downloads()
                state['events'].clear()
                state['webhook'] = None
                process = subprocess.Popen([binary], env=env, stdout=log, stderr=log)
                choose('https://media.invalid/music', 'm')
                wait('editmessagetext', lambda p: active(p, 'Sending…'))
                assert wait('sendaudio')[1]['audio'] == recovered[2]['audio']['file_id']
                assert downloads() == before, 'service restart lost the reusable ID'
                choose('https://music.youtube.com/watch?v=dQw4w9WgXcQ&feature=share', 'm')
                assert wait('sendaudio')[1]['audio'] == shared_youtube_id
                assert downloads() == before, 'another YouTube alias missed the durable cache after restart'
                process.send_signal(signal.SIGTERM)
                assert process.wait(timeout=10) == 0
                print(f'{mode}: photo replies, acknowledgment/search/queue ordering, terminal status, existing menus, typed media and durable cache flows passed')
            finally:
                state['release_setup'].set()
                if process.poll() is None:
                    process.kill()
                    process.wait()
                server.shutdown()


if __name__ == '__main__':
    binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else 'target/debug/yt-dlp-tg').resolve())
    for mode in ('polling', 'webhook'):
        if '--progress-only' in sys.argv:
            smoke(binary, mode, progress_only=True)
            continue
        if '--images-only' in sys.argv:
            smoke(binary, mode, images=True)
            smoke(binary, mode, vision=True)
            continue
        if '--startup-only' not in sys.argv:
            smoke(binary, mode)
            smoke(binary, mode, public_access=True)
            smoke(binary, mode, images=True)
            smoke(binary, mode, vision=True)
        smoke(binary, mode, startup_stop=True)
