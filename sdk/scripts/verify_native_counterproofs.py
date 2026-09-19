#!/usr/bin/env python3
"""Green/red/restored-green witnesses. Mutate implementation, never the test.
Use only an isolated worktree; restores by copy with a new mtime even on error.
"""
from pathlib import Path
import subprocess,tempfile,shutil
ROOT=Path(__file__).resolve().parents[2]
def run(command):
 r=subprocess.run(command,cwd=ROOT,text=True,encoding="utf-8",stdout=subprocess.PIPE,stderr=subprocess.STDOUT)
 print(r.stdout,flush=True);return r

def mutation(path,transform,command,witness):
 path=ROOT/path
 with tempfile.TemporaryDirectory(prefix='sdk-counterproof-') as tmp:
  backup=Path(tmp)/'original';shutil.copyfile(path,backup)
  try:
   assert run(command).returncode==0,'baseline failed'
   original=path.read_text(encoding="utf-8");changed=transform(original);assert changed!=original,'mutation did not apply';path.write_text(changed, encoding="utf-8")
   red=run(command);assert red.returncode!=0 and witness in red.stdout and 'FAILED' in red.stdout,'must fail behaviorally in the named test'
  finally:shutil.copyfile(backup,path)
  assert run(command).returncode==0,'restoration failed'
  print(f'COUNTERPROOF green/red/green: {witness}',flush=True)
def omit_dsp(s):
 a=s.index('let report = instance.processor.process(');b=s.index('r.flags =',a)
 return s[:a]+'let report = ProcessReport::default();\n                    '+s[b:]
def trust_any(s):
 a=s.index('    if keys.is_empty() {',s.index('fn verify_signature('));b=s.index('\nfn unpack(',a)
 return s[:a]+'    let _ = (bytes, signature, keys); Ok(())\n}\n'+s[b:]
mutation('sdk/tune-plugin-abi/src/lib.rs',omit_dsp,['python3','sdk/scripts/verify_native.py'],'native_dsp_processes_real_buffers_preserves_history_and_library_lifetime')
mutation('sdk/tune-plugin-native/src/package.rs',trust_any,['cargo','test','--manifest-path','sdk/Cargo.toml','-p','tune-plugin-native','--test','packages','signed_install_update_rollback_and_tamper_refusal'],'signed_install_update_rollback_and_tamper_refusal')
mutation('tune-core/src/audio/sdk_observation.rs',lambda s:s.replace('if frame.generation != current {','if false && frame.generation != current {'),['cargo','test','-p','tune-core','--lib','premium_sdk_real_spectrum_without_plugins_refuses_fake_points_and_stale_epochs','--no-default-features','--features','oaat'],'premium_sdk_real_spectrum_without_plugins_refuses_fake_points_and_stale_epochs')
