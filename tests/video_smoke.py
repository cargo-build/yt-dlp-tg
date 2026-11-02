#!/usr/bin/env python3
"""Run the actual bot and FFmpeg on generated videos against localhost Telegram."""

import argparse
from email.parser import BytesParser
from email.policy import default
import json
from pathlib import Path
import subprocess
import tempfile

from smoke import smoke


def run(args):
    return subprocess.check_output(args, stderr=subprocess.PIPE, timeout=30)


def packets(path, selector):
    result=json.loads(run(['ffprobe','-v','error','-select_streams',selector,
                          '-show_packets','-show_data_hash','sha256','-of','json',str(path)]))
    return [packet['data_hash'] for packet in result['packets']]


def verify_upload(source, event, record, root):
    payload=event[1]
    assert payload['supports_streaming'] is True or payload['supports_streaming']=='true'
    body=event[3]
    boundary=body.split(b'\r\n',1)[0][2:]
    envelope=BytesParser(policy=default).parsebytes(
        b'Content-Type: multipart/form-data; boundary="'+boundary+b'"\r\n\r\n'+body)
    part=next(part for part in envelope.iter_parts() if part.get_filename())
    assert part.get_filename().endswith('.mp4'), part.get_filename()
    content=part.get_payload(decode=True)
    assert 0<len(content)<=50*1024*1024
    assert 0<content.find(b'moov')<content.find(b'mdat'), 'MP4 is not streamable'
    output=root/'uploaded.mp4'
    output.write_bytes(content)
    metadata=json.loads(run(['ffprobe','-v','error','-show_streams','-show_format','-of','json',str(output)]))
    assert 'mp4' in metadata['format']['format_name'].split(',')
    videos=[stream for stream in metadata['streams'] if stream['codec_type']=='video']
    audios=[stream for stream in metadata['streams'] if stream['codec_type']=='audio']
    assert len(videos)==1 and videos[0]['codec_name']=='h264'
    assert videos[0]['pix_fmt']=='yuv420p' and videos[0]['height']==record['height']
    assert all(stream['codec_name']=='aac' for stream in audios)
    assert bool(audios)==(record['audio']!='none'), 'source audio was dropped'
    run(['ffmpeg','-nostdin','-v','error','-threads','1','-i',str(output),'-f','null','-'])
    if record.get('video_copy'):
        assert packets(source,'v:0')==packets(output,'v:0'), 'compatible video was re-encoded'
    if record.get('audio_copy'):
        assert packets(source,'a:0')==packets(output,'a:0'), 'compatible AAC was re-encoded'


def fixtures(root):
    def generate(name, video='libx264', audio='aac', size='256x144', pixels='yuv420p', extra=()):
        path=root/name
        args=['ffmpeg','-nostdin','-v','error','-f','lavfi','-i',f'testsrc2=size={size}:rate=12:duration=1',
              '-f','lavfi','-i','sine=frequency=440:duration=1','-c:v',video,'-pix_fmt',pixels]
        args += ['-preset','ultrafast','-crf','18'] if video=='libx264' else ['-deadline','realtime','-cpu-used','8','-b:v','0','-crf','32']
        args += ['-c:a',audio,'-threads','1','-shortest',*extra,str(path)]
        run(args)
        return path
    h264=generate('h264.mp4')
    generate('vp9.mkv',video='libvpx-vp9')
    generate('opus.mkv',video='libvpx-vp9',audio='libopus')
    generate('yuv444.mp4',pixels='yuv444p')
    generate('hdr.mp4',extra=('-color_trc','smpte2084','-color_primaries','bt2020','-colorspace','bt2020nc'))
    generate('wrongheight.mp4',size='256x180')
    run(['ffmpeg','-nostdin','-v','error','-i',str(h264),'-c','copy',str(root/'h264.mkv')])
    run(['ffmpeg','-nostdin','-v','error','-i',str(root/'vp9.mkv'),'-an','-c:v','copy',str(root/'silent.mkv')])
    (root/'corrupt.mp4').write_bytes(b'invalid MP4 fixture')
    manifest={
        'h264.mp4':{'video':'h264','audio':'aac','video_copy':True,'audio_copy':True},
        'h264.mkv':{'video':'h264','audio':'aac','video_copy':True,'audio_copy':True},
        'vp9.mkv':{'video':'vp9','audio':'aac','audio_copy':True},
        'opus.mkv':{'video':'vp9','audio':'opus'},
        'silent.mkv':{'video':'vp9','audio':'none'},
        'yuv444.mp4':{'video':'h264','audio':'aac','audio_copy':True},
        'hdr.mp4':{'video':'h264','audio':'aac','rejected':True},
        'wrongheight.mp4':{'video':'h264','audio':'aac','rejected':True},
        'corrupt.mp4':{'video':'h264','audio':'aac','rejected':True},
    }
    for name,record in manifest.items():
        record.update(file=name,height=144)
    (root/'manifest.json').write_text(json.dumps(manifest))


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary',default='target/debug/yt-dlp-tg')
    args=parser.parse_args()
    binary=str(Path(args.binary).resolve())
    with tempfile.TemporaryDirectory(prefix='yt-dlp-tg-real-video-') as temporary:
        root=Path(temporary)
        fixtures(root)
        for mode in ('polling','webhook'):
            smoke(binary,mode,real_video_root=root)


if __name__=='__main__':
    main()
