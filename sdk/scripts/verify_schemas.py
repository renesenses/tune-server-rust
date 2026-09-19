#!/usr/bin/env python3
"""Generate JSON schemas from the compiled Rust types; reject checked-in drift."""
import argparse,json,pathlib,subprocess
sdk=pathlib.Path(__file__).resolve().parents[1]
p=argparse.ArgumentParser();p.add_argument('--write',action='store_true');args=p.parse_args()
catalog=json.loads((sdk/'plugins.json').read_text(encoding='utf-8'))
for name in ['sdk'] + [p['id'] for p in catalog['native']]:
 crate=sdk/('tune-plugin-'+name)
 schema=json.loads(subprocess.check_output(['cargo','run','--quiet','--manifest-path',str(crate/'Cargo.toml'),'--example','schema','--features','schemas']))
 items=schema.items() if name=='sdk' else [('config',schema)]
 for filename,value in items:
  target=crate/'schemas'/(filename+'.json')
  if args.write:
   target.parent.mkdir(exist_ok=True);target.write_text(json.dumps(value,indent=2,ensure_ascii=False)+'\n', encoding="utf-8")
  else:assert json.loads(target.read_text(encoding="utf-8"))==value,f'schema drift: {target}'
 print('Schema matches compiled types:',name)
