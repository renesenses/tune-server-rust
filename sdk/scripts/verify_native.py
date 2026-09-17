#!/usr/bin/env python3
"""Load actual native artifacts in an isolated feature build directory.

Cargo cdylib output names lack feature hashes. A source-only parity build can
replace libfoo.so while the native fingerprint remains fresh. Separate targets
prevent accidentally testing that stale public filename.
"""
import json, os, pathlib, subprocess
sdk=pathlib.Path(__file__).resolve().parents[1]
meta=json.loads(subprocess.check_output(['cargo','metadata','--manifest-path',str(sdk/'Cargo.toml'),'--no-deps','--format-version','1']))
env=os.environ.copy()
directory=pathlib.Path(meta['target_directory'])/'sdk-native'
env['CARGO_TARGET_DIR']=str(directory)
subprocess.run(['cargo','build','--manifest-path',str(sdk/'Cargo.toml'),'--workspace','--all-features','--locked'],check=True,env=env)
env['TUNE_NATIVE_TEST_DIR']=str(directory/'debug')
subprocess.run(['cargo','test','--manifest-path',str(sdk/'Cargo.toml'),'-p','tune-plugin-native','--features','native-conformance','--test','native','--locked'],check=True,env=env)
