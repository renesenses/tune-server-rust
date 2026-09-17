#!/usr/bin/env python3
"""Execute the pinned pre-extraction DSP and the extracted code on identical PCM.

This is an independent historical oracle, not two imports of the new engine.
Compares every rendered byte across integer depths/rates/channels and hot swaps.
No hardware or listening claim. Uses only Python stdlib; artifacts use Cargo cache.
"""
from pathlib import Path
import argparse, json, subprocess, tempfile
ROOT = Path(__file__).resolve().parents[2]
BASE = 'af70d7e251735d8be2c5d7ddc9d6539f61e2ac34'
RUNNER = r'''
use audio::eq::{EqProfile,EqBandSpec,EqProcessor};
use audio::crossfeed::CrossfeedProcessor;
fn main() {
 let mut result=Vec::new();
 for sr in [44100,48000,96000,192000] { for bd in [16,24,32] {for channels in [1,2,6] {
  for kind in 0..5 {
   let band=EqBandSpec{freq:1700.0,gain:if kind==3{6.0}else{-7.0},q:1.2,channel:if kind==4{Some(1)}else{None},..Default::default()};
   let profile=EqProfile{enabled:kind!=0,bands:if kind>=2{vec![band]}else{vec![]},bass_gain_db:if kind==1{3.0}else{0.0},..Default::default()};
   let bps=bd as usize/8;
   let mut pcm=Vec::new();
   for i in 0..channels as usize*2048 {let v=if i%53==0 {0} else {((i as f64*0.083).sin() * ((1_u64<<(bd-2)) as f64)) as i32};pcm.extend_from_slice(&v.to_le_bytes()[..bps]);}
   let mut p=EqProcessor::new(&profile,sr,channels);let split=channels as usize*bps*256;
   p.process_pcm(&mut pcm[..split],bd);
   let mut next=EqProcessor::new(&profile,sr,channels);next.inherit_state_from(&p);
   for block in pcm[split..].chunks_mut(channels as usize*bps*128) {next.process_pcm(block,bd);}
   result.extend_from_slice(&pcm);
  }
 }for amount in [0.0,0.3,0.5] {for delay in [0.0,0.3,1.0] {
  let bps=bd as usize/8;let mut pcm=Vec::new();for i in 0..4096 {let v=((i as f64*0.071).sin()*((1_u64<<(bd-2)) as f64)) as i32;pcm.extend_from_slice(&v.to_le_bytes()[..bps]);}
  let mut p=CrossfeedProcessor::new(sr,amount,delay);p.process_pcm(&mut pcm[..512*bps],bd,2);
  let mut next=CrossfeedProcessor::new(sr,amount,delay);next.inherit_state_from(&p);for block in pcm[512*bps..].chunks_mut(128*bps) {next.process_pcm(block,bd,2);}result.extend_from_slice(&pcm);
 }} }}
 use std::io::Write;std::io::stdout().write_all(&result).unwrap();
}
'''
def execute(project, binary):
    subprocess.run(['cargo','build','--quiet','--manifest-path',str(project/'Cargo.toml')],check=True)
    metadata=json.loads(subprocess.check_output(['cargo','metadata','--no-deps','--format-version','1','--manifest-path',str(project/'Cargo.toml')]))
    import os
    path=Path(metadata['target_directory'])/'debug'/(binary+('.exe' if os.name=='nt' else ''))
    return subprocess.check_output([str(path)])
def main():
 with tempfile.TemporaryDirectory(prefix='tune-dsp-parity-') as directory:
    root=Path(directory);outputs=[]
    for mode in ['historical','extracted','native']:
        p=root/mode;(p/'src/audio').mkdir(parents=True)
        package=f'tune-parity-{mode}'
        deps='serde = {version="1",features=["derive"]}\nserde_json="1"\ntracing="0.1"\n'
        if mode=='historical':
            for name in ['eq','crossfeed','dither','ecretage']:
                source=subprocess.check_output(['git','show',f'{BASE}:tune-core/src/audio/{name}.rs'],cwd=ROOT).decode()
                if name=='crossfeed':source=source[:source.index('/// Contrainte qui prive')]
                else:source=source.split('#[cfg(test)]')[0]
                (p/f'src/audio/{name}.rs').write_text(source)
            (p/'src/audio/mod.rs').write_text('pub mod eq; pub mod crossfeed; pub mod dither; pub mod ecretage;')
        else:
            for name in ['equalizer','crossfeed']:
                deps+=f'tune-plugin-{name} = {{path={json.dumps(str(ROOT/"sdk"/f"tune-plugin-{name}"))}}}\n'
            (p/'src/audio/mod.rs').write_text('pub use tune_plugin_equalizer as eq; pub use tune_plugin_crossfeed as crossfeed;')
        runner=RUNNER
        if mode=='native':
            import os
            for name in ['native','sdk','audio-support']:
                deps+=f'tune-plugin-{name} = {{path={json.dumps(str(ROOT/"sdk"/f"tune-plugin-{name}"))}}}\n'
            for name in ['eq','crossfeed']:
                source=(ROOT/f'tune-core/src/audio/{name}.rs').read_text()
                if name=='crossfeed':source=source[:source.index('/// Contrainte qui prive')]
                (p/f'src/audio/{name}.rs').write_text(source)
            (p/'src/audio/mod.rs').write_text('pub mod eq;pub mod crossfeed;pub use tune_plugin_audio_support::ecretage;')
            meta=json.loads(subprocess.check_output(['cargo','metadata','--manifest-path',str(ROOT/'sdk/Cargo.toml'),'--format-version','1','--no-deps']))
            directory=Path(meta['target_directory'])/'sdk-native'/'debug'
            extension='dll' if os.name=='nt' else ('dylib' if __import__('sys').platform=='darwin' else 'so')
            prefix='' if os.name=='nt' else 'lib'
            initialization=''
            for name in ['equalizer','crossfeed']:
                lib=str(directory/f'{prefix}tune_plugin_{name}.{extension}')
                initialization+=f'tune_plugin_native::register(unsafe{{tune_plugin_native::Library::load_trusted(std::path::Path::new({json.dumps(lib)}))}}.unwrap()).unwrap();'
            runner=RUNNER.replace('fn main() {','fn main() {'+initialization)
        (p/'Cargo.toml').write_text(f'[workspace]\n[package]\nname="{package}"\nversion="0.1.0"\nedition="2024"\n[dependencies]\n{deps}')
        (p/'src/main.rs').write_text('#![allow(dead_code)]\nmod audio;\n'+runner)
        outputs.append(execute(p,package))
    assert outputs[0]==outputs[1]==outputs[2], 'PCM differs from the pinned pre-extraction implementations'
    import hashlib
    print(f'PARITY: {len(outputs[0])} bytes identical; SHA-256 {hashlib.sha256(outputs[0]).hexdigest()}; baseline {BASE}')
if __name__=='__main__':main()
