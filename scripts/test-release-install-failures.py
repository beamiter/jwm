#!/usr/bin/env python3
"""Offline release-install transaction regressions, using only private DESTDIRs."""
from pathlib import Path
import tempfile, subprocess, os, shutil, argparse
import atexit
ROOT=Path(__file__).resolve().parent.parent
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--installer', type=Path, default=ROOT/'scripts/install-release.sh')
args = parser.parse_args()
temporary = tempfile.TemporaryDirectory(prefix='jwm-release-regressions-')
p=Path(temporary.name)
atexit.register(temporary.cleanup)
patch=args.installer
manifest=ROOT/'packaging/release-manifest.tsv'
def bundle(version):
 b=p/('bundle-'+version); payload=b/('payload/usr/local/lib/jwm/versions/'+version);payload.mkdir(parents=True)
 shutil.copy(patch,b/'install-release.sh');shutil.copy(manifest,b/'release-manifest.tsv');(b/'VERSION').write_text(version+'\n')
 for l in manifest.read_text().splitlines():
  if not l or l.startswith('#'):continue
  kind,src,vpath,stable,mode=l.split('\t');x=payload/vpath
  if kind in ['repo-tree','stable-link']:x.mkdir(parents=True,exist_ok=True)
  else:x.parent.mkdir(parents=True,exist_ok=True);x.write_text(version+'\n');x.chmod(int(mode,8))
 return b
b1,b2=bundle('1.0.0'),bundle('2.0.0');records=[]
for stage in ['install','ln','mv']:
 for nth in ([1,5,15] if stage=='install' else [1,2,5]):
  for upgrade in [False,True]:
   case=p/f'{stage}-{nth}-{upgrade}';root=case/'root';(root/'usr/local/bin').mkdir(parents=True); (root/'usr/local/bin/jwm').write_text('legacy\n')
   def invoke(b,env=None): return subprocess.run(['bash',str(b/'install-release.sh'),'install','--destdir',str(root),'--replace'],env=env,capture_output=True,text=True,timeout=30)
   if upgrade:
    r=invoke(b1);assert r.returncode==0,r.stderr
   bindir=case/'bin';bindir.mkdir(); counter=case/'count';exe=bindir/stage
   exe.write_text(f'''#!/bin/bash
n=0
[[ ! -f '{counter}' ]] || read -r n < '{counter}'
n=$((n+1)); echo "$n" > '{counter}'
if (( n == {nth} )); then echo 'injected {stage} failure' >&2; exit 74; fi
exec '{shutil.which(stage)}' "$@"
''');exe.chmod(0o755)
   r=invoke(b2 if upgrade else b1,dict(os.environ,PATH=str(bindir)+':'+os.environ['PATH']))
   if r.returncode!=0:
    assert (root/'usr/local/bin/jwm').read_text()==('1.0.0\n' if upgrade else 'legacy\n'),(stage,nth,upgrade,r.stderr)
    rr=invoke(b2 if upgrade else b1);assert rr.returncode==0,(stage,nth,upgrade,rr.stderr)
   records.append({'command':stage,'nth':nth,'upgrade':upgrade,'exit':r.returncode,'restored_then_retry':r.returncode!=0,'stderr':r.stderr})
print(f'test-release-install-failures: PASS ({len(records)} cases)')
