#!/usr/bin/env python3
"""Qualify installed yt-dlp/FFmpeg with local generated media and actual selectors."""

import functools
import ast
import http.server
import json
from pathlib import Path
import re
import subprocess
import tempfile
import threading


def run(args, data=None):
    return subprocess.check_output(args, input=data, stderr=subprocess.PIPE)


def packet_hashes(path):
    output = run(['ffprobe', '-v', 'error', '-select_streams', 'a:0', '-show_packets', '-show_data_hash', 'sha256', '-of', 'json', str(path)])
    return [packet['data_hash'] for packet in json.loads(output)['packets']]


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass


def main():
    source = (Path(__file__).resolve().parents[1] / 'src/media.rs').read_text()
    selectors = {kind: re.search(rf'const {kind}_SELECTOR: &str = r?"([^"]+)";', source)[1] for kind in ('AUDIO', 'MUSIC')}
    opus_arguments = ast.literal_eval(re.search(r'Format::Opus => \{\s*command.args\((\[.*?\])\)', source, re.S)[1].replace('AUDIO_SELECTOR', repr(selectors['AUDIO'])))
    music_arguments = ast.literal_eval(re.search(r'Format::M4a => \{\s*command.args\((\[.*?\])\)', source, re.S)[1].replace('MUSIC_SELECTOR', repr(selectors['MUSIC'])))
    with tempfile.TemporaryDirectory(prefix='yt-dlp-tg-codecs-') as scratch:
        root = Path(scratch)
        opus = root / 'source.opus'
        combined = root / 'source.mp4'
        cover = root / 'cover.jpg'
        aac = root / 'source.m4a'
        best_aac = root / 'best.m4a'
        run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i', 'sine=frequency=440:duration=0.3', '-c:a', 'libopus', '-threads', '1', str(opus)])
        run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i', 'sine=frequency=440:duration=0.3', '-c:a', 'aac', '-b:a', '128k', '-threads', '1', str(aac)])
        run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i', 'sine=frequency=880:duration=0.3', '-c:a', 'aac', '-b:a', '256k', '-threads', '1', str(best_aac)])
        run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i', 'color=c=blue:size=32x32', '-frames:v', '1', '-threads', '1', str(cover)])
        run(['ffmpeg', '-v', 'error', '-i', str(cover), '-frames:v', '1', '-threads', '1', str(root / 'cover.webp')])
        run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i', 'color=size=1280x720:rate=5:duration=0.3', '-f', 'lavfi', '-i', 'sine=frequency=440:duration=0.3', '-c:v', 'libx264', '-threads', '1', '-c:a', 'aac', '-shortest', str(combined)])
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), functools.partial(QuietHandler, directory=str(root)))
        threading.Thread(target=server.serve_forever, daemon=True).start()
        base = f'http://127.0.0.1:{server.server_port}'
        common = ['yt-dlp', '--ignore-config', '--no-plugin-dirs', '--no-cache-dir', '--no-warnings', '--no-progress', '--proxy', '']
        try:
            for label, filename in [('native', opus.name), ('converted', combined.name)]:
                directory = root / label
                directory.mkdir()
                output = run([*common, *opus_arguments, '-P', str(directory), '-o', 'media.%(ext)s', '--print', 'after_move:%(filepath)j', '--', f'{base}/{filename}'])
                path = Path(json.loads(output))
                codec = json.loads(run(['ffprobe', '-v', 'error', '-show_streams', '-of', 'json', str(path)]))['streams'][0]['codec_name']
                assert codec == 'opus' and path.suffix == '.opus'
                run(['ffmpeg', '-v', 'error', '-i', str(path), '-f', 'null', '-'])
                if label == 'native':
                    assert packet_hashes(opus) == packet_hashes(path), 'native Opus packets were changed'

            for thumbnail in ('cover.jpg', 'cover.webp'):
                directory = root / ('covered-' + thumbnail)
                directory.mkdir()
                metadata = {'id': 'fixture', 'title': 'Cover fixture', 'extractor': 'generic', 'webpage_url': f'{base}/source.opus', 'thumbnails': [{'url': f'{base}/{thumbnail}'}], 'formats': [{'format_id': 'audio', 'url': f'{base}/source.opus', 'ext': 'opus', 'vcodec': 'none', 'acodec': 'opus'}]}
                output = run([*common, *opus_arguments, '--load-info-json', '-', '-P', str(directory), '-o', 'media.%(ext)s', '--print', 'after_move:%(filepath)j'], json.dumps(metadata).encode())
                path = Path(json.loads(output))
                info = json.loads(run(['ffprobe', '-v', 'error', '-show_streams', '-show_format', '-of', 'json', str(path)]))
                pictures = [stream for stream in info['streams'] if stream.get('disposition', {}).get('attached_pic')]
                assert len(pictures) == 1 and pictures[0]['codec_name'] == 'mjpeg', info
                assert pictures[0]['width'] == 32 and pictures[0]['height'] == 32
                assert packet_hashes(opus) == packet_hashes(path), 'cover embedding changed native Opus packets'
                if thumbnail.endswith('.jpg'):
                    embedded = run(['ffmpeg', '-v', 'error', '-i', str(path), '-map', '0:v:0', '-c:v', 'copy', '-frames:v', '1', '-f', 'image2pipe', '-'])
                    assert embedded == cover.read_bytes(), 'embedded image differs from the selected thumbnail'
                assert list(directory.iterdir()) == [path], 'thumbnail sidecars were retained'

            for label, filename, formats, expected in [
                ('aac-copy', combined.name, [{'format_id':'combined','url':f'{base}/{combined.name}','ext':'mp4','vcodec':'avc1','acodec':'mp4a.40.2'}], combined),
                ('best-native-aac', aac.name, [
                    {'format_id':'low','url':f'{base}/{aac.name}','ext':'m4a','vcodec':'none','acodec':'mp4a.40.2','abr':128,'tbr':128},
                    {'format_id':'high','url':f'{base}/{best_aac.name}','ext':'m4a','vcodec':'none','acodec':'aac','abr':256,'tbr':256}],best_aac),
            ]:
                directory = root / label
                directory.mkdir()
                metadata = {'id':'fixture','title':'Native AAC','extractor':'generic','webpage_url':f'{base}/{filename}','formats':formats}
                output = run([*common,*music_arguments,'--load-info-json','-','-P',str(directory),'-o','media.%(ext)s','--print','after_move:%(filepath)j'],json.dumps(metadata).encode())
                path = Path(json.loads(output))
                codec = json.loads(run(['ffprobe', '-v', 'error', '-show_streams', '-of', 'json', str(path)]))['streams'][0]['codec_name']
                assert codec == 'aac' and path.suffix == '.m4a'
                run(['ffmpeg', '-v', 'error', '-i', str(path), '-f', 'null', '-'])
                assert packet_hashes(expected) == packet_hashes(path), 'native AAC packets or selected source changed'

            directory=root/'reject-opus-for-aac'
            directory.mkdir()
            metadata={'id':'fixture','title':'Opus only','extractor':'generic','webpage_url':f'{base}/{opus.name}',
                      'formats':[{'format_id':'opus','url':f'{base}/{opus.name}','ext':'opus','vcodec':'none','acodec':'opus'}]}
            rejected=subprocess.run([*common,*music_arguments,'--load-info-json','-','-P',str(directory)],input=json.dumps(metadata).encode(),capture_output=True)
            assert rejected.returncode!=0 and not list(directory.iterdir()), 'native AAC silently converted another codec'

            def select_formats(formats, selector):
                metadata = {'id': 'fixture', 'title': 'fixture', 'extractor': 'generic', 'webpage_url': f'{base}/source.mp4', 'formats': formats}
                result = subprocess.run([*common, '--simulate', '--load-info-json', '-', '-f', selector, '--print', '%(format_id)s'], input=json.dumps(metadata).encode(), capture_output=True)
                return result.returncode, result.stdout.decode().strip()

            def video(id, height):
                return {'format_id': id, 'url': f'{base}/source.mp4', 'ext': 'mp4', 'vcodec': 'avc1', 'acodec': 'mp4a', **({'height': height} if height else {})}

            status, selected = select_formats([video('v2160', 2160), video('v1080', 1080), video('unknown', None)], 'all[format_id=v1080][height=1080]')
            assert status == 0 and selected == 'v1080', (status, selected)
            assert select_formats([video('unknown', None)], 'all[format_id=v1080][height=1080]')[0] != 0
            status, selected = select_formats([video('best',720),video('higher',720)], 'all[format_id=best][height=720]')
            assert status == 0 and selected == 'best', 'reserved format ID was interpreted as a selector alias'
            status, selected = select_formats([video('combined_aac', 720)], selectors['AUDIO'])
            assert status == 0 and selected == 'combined_aac'
            print('real yt-dlp/FFmpeg: JPEG/WebP covers embedded with unchanged Opus packets and no sidecars; native AAC copy, codec conversions and pinned resolution selectors passed')
        finally:
            server.shutdown()


if __name__ == '__main__':
    main()
