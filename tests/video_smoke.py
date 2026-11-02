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
    rotation=next((side.get('rotation',0) for side in videos[0].get('side_data_list',[]) if side.get('side_data_type')=='Display Matrix'),0)
    display_height=videos[0]['width'] if int(rotation)%180 else videos[0]['height']
    assert videos[0]['pix_fmt']=='yuv420p' and display_height==record['height'], (source.name, record['height'], display_height, videos[0]['pix_fmt'], rotation)
    assert all(stream['codec_name']=='aac' for stream in audios)
    assert bool(audios)==(record['audio']!='none'), 'source audio was dropped'
    run(['ffmpeg','-nostdin','-v','error','-threads','1','-i',str(output),'-f','null','-'])
    if record.get('orientation'):
        frames=[run(['ffmpeg','-nostdin','-v','error','-threads','1','-i',str(path),'-frames:v','1','-pix_fmt','rgb24','-f','rawvideo','-']) for path in (source,output)]
        assert len(frames[0])==len(frames[1])
        difference=sum(abs(left-right) for left,right in zip(*frames))/len(frames[0])
        assert difference<15, ('visible orientation changed',source.name,difference)
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
    generate('square-vp9.mkv',video='libvpx-vp9',size='144x144')
    generate('opus.mkv',video='libvpx-vp9',audio='libopus')
    generate('yuv444.mp4',pixels='yuv444p')
    run(['ffmpeg','-nostdin','-v','error','-i',str(h264),'-c','copy','-bsf:v',
         'h264_metadata=transfer_characteristics=16:colour_primaries=9:matrix_coefficients=9',str(root/'hdr.mp4')])
    hdr=json.loads(run(['ffprobe','-v','error','-select_streams','v:0','-show_streams','-of','json',str(root/'hdr.mp4')]))
    assert hdr['streams'][0].get('color_transfer')=='smpte2084', 'HDR fixture lacks transfer metadata'
    generate('wrongheight.mp4',size='256x180')
    run(['ffmpeg','-nostdin','-v','error','-i',str(h264),'-c','copy',str(root/'h264.mkv')])
    run(['ffmpeg','-nostdin','-v','error','-display_rotation:v:0','90','-i',str(h264),'-c','copy',str(root/'rotated.mp4')])
    rotated=json.loads(run(['ffprobe','-v','error','-select_streams','v:0','-show_streams','-of','json',str(root/'rotated.mp4')]))
    assert any(int(side.get('rotation',0))%180 for side in rotated['streams'][0].get('side_data_list',[])), 'rotation fixture has no display matrix'
    for source,name in [('vp9.mkv','rotated-vp9.mp4'),('square-vp9.mkv','rotated-square.mp4')]:
        run(['ffmpeg','-nostdin','-v','error','-display_rotation:v:0','90','-i',str(root/source),'-c','copy',str(root/name)])
        rotated=json.loads(run(['ffprobe','-v','error','-select_streams','v:0','-show_streams','-of','json',str(root/name)]))
        assert any(int(side.get('rotation',0))%180 for side in rotated['streams'][0].get('side_data_list',[])), 'converted rotation fixture has no display matrix'
    run(['ffmpeg','-nostdin','-v','error','-i',str(root/'vp9.mkv'),'-an','-c:v','copy',str(root/'silent.mkv')])
    (root/'corrupt.mp4').write_bytes(b'invalid MP4 fixture')
    manifest={
        'h264.mp4':{'video':'h264','audio':'aac','video_copy':True,'audio_copy':True},
        'h264.mkv':{'video':'h264','audio':'aac','video_copy':True,'audio_copy':True},
        'rotated.mp4':{'video':'h264','audio':'aac','video_copy':True,'audio_copy':True,'orientation':True},
        'rotated-vp9.mp4':{'video':'vp9','audio':'aac','audio_copy':True,'orientation':True},
        'rotated-square.mp4':{'video':'vp9','audio':'aac','audio_copy':True,'orientation':True},
        'vp9.mkv':{'video':'vp9','audio':'aac','audio_copy':True},
        'opus.mkv':{'video':'vp9','audio':'opus'},
        'silent.mkv':{'video':'vp9','audio':'none'},
        'yuv444.mp4':{'video':'h264','audio':'aac','audio_copy':True},
        'hdr.mp4':{'video':'h264','audio':'aac','rejected':True},
        'wrongheight.mp4':{'video':'h264','audio':'aac','rejected':True},
        'corrupt.mp4':{'video':'h264','audio':'aac','rejected':True},
    }
    for name,record in manifest.items():
        record.update(file=name,height=256 if name in ('rotated.mp4','rotated-vp9.mp4') else 144)
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
