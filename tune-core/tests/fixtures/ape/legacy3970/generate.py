from pathlib import Path
import struct,subprocess,hashlib,json
out=Path('fixtures');out.mkdir(exist_ok=True)
manifest=[]
cases=[('stereo16_c4000',2,16,4000,4096,'mixed'),('stereo16_c2000',2,16,2000,4096,'mixed'),('mono16_c4000',1,16,4000,4096,'mixed'),('stereo24_c4000',2,24,4000,4096,'mixed'),('stereo8_c4000',2,8,4000,4096,'mixed'),('pseudo16_c4000',2,16,4000,4096,'pseudo'),('silence16_c4000',2,16,4000,4096,'silence'),('multiframe16_c4000',2,16,4000,294912+4096,'periodic')]
for name,channels,bits,level,count,mode in cases:
    pcm=bytearray()
    for i in range(count):
        for ch in range(channels):
            if mode=='silence': value=0
            elif mode=='periodic': value=((i*(ch+1))%1001-500)*16
            else: value=((i*193)%40001-20000) if ch==0 or mode=='pseudo' else ((i*i*17)%30001-15000)
            if bits==24: value*=256
            if bits==8: value=(value//256)+128
            pcm.extend(value.to_bytes(bits//8,'little',signed=bits!=8))
    (out/(name+'.pcm')).write_bytes(pcm)
    frames=[]
    for f in range((count+294911)//294912):
        chunk=pcm[f*294912*channels*(bits//8):(f+1)*294912*channels*(bits//8)]
        Path('chunk.pcm').write_bytes(chunk)
        subprocess.run(['encoder397/encode','chunk.pcm','chunk.raw',str(channels),str(bits),str(level)],check=True)
        frames.append(Path('chunk.raw').read_bytes())
    flags=34+(8 if bits==24 else 1 if bits==8 else 0)
    header=struct.pack('<4sHHHHIIIII',b'MAC ',3970,level,flags,channels,44100,0,0,len(frames),(count-1)%294912+1)
    offset=32+4*len(frames);seek=b''
    for frame in frames:
        seek+=struct.pack('<I',offset);offset+=len(frame)
    ape=header+seek+b''.join(frames)
    (out/(name+'.ape')).write_bytes(ape)
    tune_pcm=pcm if bits != 8 else b''.join(((b-128)*256).to_bytes(2,'little',signed=True) for b in pcm)
    manifest.append(dict(tune_pcm_sha256=hashlib.sha256(tune_pcm).hexdigest(),name=name,channels=channels,bits=bits,compression=level,blocks=count,frames=len(frames),ape_sha256=hashlib.sha256(ape).hexdigest(),pcm_sha256=hashlib.sha256(pcm).hexdigest(),bytes=len(ape)))
(out/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
print(json.dumps(manifest,indent=2))
