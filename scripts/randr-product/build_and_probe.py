#!/usr/bin/env python3
"""UNPUBLISHED: clean exact source builds and private RandR/JWM probe, one CI job.
Run with python3 -I. No caller-provided source roots, binaries, or receipt JSON.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import traceback

REPO='https://github.com/beamiter/jwm.git'
SOURCES={'baseline':'687cebf36585c7baeeb6850d851a1b8fef017865',
         'fixed':'3dbca462cadf37c005261b04195a50b409210084'}
# Generated from the precise local review candidate, never from checkout input.
FIXTURE_HASHES = {'product_probe.py': '6fff81ffb23d3bd4040a0ca40043ca575d25b8f22604f0cbf62a7b8a68ef4ac1', 'owned_xorg.py': '1273e3df52b02a8c8144303a785629e1be1b2213a811a5a7aa7dded56a134828', 'preflight.py': '447e1f5ec400f36fbf42406f9a403b12ca34fd33c6155f7e77e81636e50d73b7', 'xorg.conf': '86148c94e78b382ab32e6fe00e672faa68ab813daae1b42da3b774221e23a404', 'key_client.c': '334097c84dbe8e2496e614e588312b31b2c8b0f4b2c405f6154cdc20a0202d61'}


def require(value,message):
    if not value: raise RuntimeError(message)


def sha(path): return hashlib.sha256(path.read_bytes()).hexdigest()


def toolchain(directory):
    directory=directory.resolve(strict=True)
    allowed=(Path('/home/runner/.rustup/toolchains'),Path('/opt/hostedtoolcache'))
    require(any(directory.is_relative_to(p) for p in allowed),'toolchain outside reviewed runner locations')
    result={}
    for name in ('cargo','rustc'):
        path=(directory/name).resolve(strict=True)
        require(path.parent==directory and path.is_file() and os.access(path,os.X_OK),'invalid toolchain binary '+name)
        result[name]={'path':str(path),'sha256':sha(path)}
    for name in ('git','cc','python3'):
        path=Path('/usr/bin',name).resolve(strict=True)
        require(path.is_file() and os.access(path,os.X_OK),'missing system tool '+name)
        result[name]={'path':str(path),'sha256':sha(path)}
    return result


class Runner:
    def __init__(self,out,work,tools):
        self.out,self.work,self.tools=out,work,tools
        self.seq=0; self.phase='setup'; self.deadline=time.monotonic()+2400
        for name in ('home','cargo','cache','config','data','runtime'):
            (work/name).mkdir(mode=0o700)
        # Construct, do not filter, the environment. No inherited RUSTFLAGS,
        # wrappers, target dir, preload, Python path, Git config, display or auth.
        self.env={'PATH':str(Path(tools['cargo']['path']).parent)+':/usr/bin:/bin',
                  'HOME':str(work/'home'),'CARGO_HOME':str(work/'cargo'),
                  'RUSTC':tools['rustc']['path'],'LANG':'C.UTF-8','LC_ALL':'C.UTF-8',
                  'XDG_CONFIG_HOME':str(work/'config'),'XDG_CACHE_HOME':str(work/'cache'),
                  'XDG_DATA_HOME':str(work/'data'),'XDG_RUNTIME_DIR':str(work/'runtime'),
                  'GIT_CONFIG_NOSYSTEM':'1','GIT_CONFIG_GLOBAL':'/dev/null',
                  'GIT_TERMINAL_PROMPT':'0','CARGO_BUILD_JOBS':'2'}

    def command(self,args,cwd,extra=None,timeout=120):
        self.seq+=1; record={'argv':list(map(str,args)),'cwd':str(cwd),'phase':self.phase,
                             'exit_code':None,'status':'failed','started_ns':time.time_ns()}
        prefix=self.out/f'command-{self.seq:03d}'
        remaining=min(timeout,self.deadline-time.monotonic()); require(remaining>0,'overall build deadline')
        with prefix.with_suffix('.stdout').open('xb') as stdout,prefix.with_suffix('.stderr').open('xb') as stderr:
            proc=None; failure=None
            try:
                proc=subprocess.Popen(record['argv'],cwd=cwd,env={**self.env,**(extra or {})},
                                      stdin=subprocess.DEVNULL,stdout=stdout,stderr=stderr,start_new_session=True)
                record['exit_code']=proc.wait(timeout=remaining)
                require(record['exit_code']==0,'command failed: '+record['argv'][0])
                record['status']='passed'
            except BaseException as error:
                failure=error
                record['error']=repr(error); record['traceback']=traceback.format_exc()
            finally:
                try:
                    if proc is not None and proc.poll() is None:
                        os.killpg(proc.pid,signal.SIGTERM)
                        try: proc.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            os.killpg(proc.pid,signal.SIGKILL); proc.wait(timeout=5)
                except BaseException as error:
                    record['status']='failed'
                    record['cleanup_error']=repr(error)
                    record['cleanup_traceback']=traceback.format_exc()
                    if failure is None: failure=error
                finally:
                    record['finished_ns']=time.time_ns()
                    prefix.with_suffix('.json').write_text(json.dumps(record,indent=2))
            if failure is not None: raise failure
        return prefix.with_suffix('.stdout').read_text(errors='replace').strip()

    def git(self,root,*args):
        return self.command([self.tools['git']['path'],'-c','core.hooksPath=/dev/null',
                             '-c','core.fsmonitor=false',*args],root)

    def checkout(self,label,expected):
        self.phase='checkout-'+label
        root=self.work/label; root.mkdir(mode=0o700)
        self.git(root,'init','--quiet')
        self.git(root,'fetch','--no-tags','--depth=1',REPO,expected)
        self.git(root,'checkout','--detach','--quiet','FETCH_HEAD')
        require(self.git(root,'rev-parse','HEAD')==expected,'checkout hash mismatch')
        require(not self.git(root,'status','--porcelain','--untracked-files=all'),'checkout not clean')
        return root

    def build(self,label,root,expected,client):
        self.phase='build-'+label
        require(not (root/'target').exists(),'build target is not cold')
        output=self.command([self.tools['cargo']['path'],'build','--locked','--release',
                             '--no-default-features','--features','backend-x11rb',
                             '--bin','jwm','--bin','jwm-tool'],root,
                            extra={'CARGO_TARGET_DIR':str(root/'target')},timeout=1800)
        require(self.git(root,'rev-parse','HEAD')==expected,'source changed during build')
        require(not self.git(root,'status','--porcelain','--untracked-files=all'),'source dirtied during build')
        binary=root/'target/release/jwm'
        require(binary.is_file() and os.access(binary,os.X_OK),'build did not produce expected JWM executable')
        require(binary.resolve()==binary,'unexpected symlink in build artifact path')
        for name in ('cargo','rustc'):
            require(sha(Path(self.tools[name]['path']))==self.tools[name]['sha256'],'toolchain changed')
        receipt={'source':expected,'root':str(root),'binary':str(binary),'binary_sha256':sha(binary),
                 'client_sha256':sha(client),'exit_code':0,'tools':self.tools}
        (self.out/(label+'-build.json')).write_text(json.dumps(receipt,indent=2))
        return receipt


def verified_fixture_copy(destination):
    source=Path(__file__).resolve().parent
    require(set(FIXTURE_HASHES)=={'product_probe.py','owned_xorg.py','preflight.py','xorg.conf','key_client.c'},
            'fixture review manifest incomplete')
    destination.mkdir(mode=0o700)
    for name,expected in FIXTURE_HASHES.items():
        data=(source/name).read_bytes()
        require(hashlib.sha256(data).hexdigest()==expected,'unreviewed fixture '+name)
        (destination/name).write_bytes(data)
    return destination


def execute(out,tool_dir):
    result={'status':'failed','stage':'setup','errors':[]}; candidate=False; tmp=None
    (out/'run.json').write_text(json.dumps(result))
    try:
        tools=toolchain(tool_dir)
        (out/'tools.json').write_text(json.dumps(tools,indent=2))
        tmp=tempfile.TemporaryDirectory(prefix='jwm-randr-build-')
        work=Path(tmp.name); fixture=verified_fixture_copy(work/'fixture')
        # Only checked immutable private copies are imported, never checkout code.
        sys.path.insert(0,str(fixture))
        spec=importlib.util.spec_from_file_location('product_probe',fixture/'product_probe.py')
        probe=importlib.util.module_from_spec(spec); spec.loader.exec_module(probe)
        runner=Runner(out,work,tools)
        runner.phase='tool-versions'
        runner.command([tools['cargo']['path'],'--version','--verbose'],work)
        runner.command([tools['rustc']['path'],'--version','--verbose'],work)
        client=work/'key_client'; runner.phase='build-input-fixture'
        runner.command([tools['cc']['path'],'-std=c11','-Wall','-Wextra','-Werror',
                        '-D_POSIX_C_SOURCE=200809L',fixture/'key_client.c','-o',client,'-lX11'],work)
        builds={}
        for label,expected in SOURCES.items():
            result['stage']='build-'+label
            root=runner.checkout(label,expected)
            builds[label]=runner.build(label,root,expected,client)
        result['stage']='real-X11-probe'
        require(probe.run_pair(builds,client,out)==0,'real probe did not pass; see per-session results')
        candidate=True
    except BaseException as error:
        result['errors'].append({'stage':result['stage'],'error':repr(error),
                                'filename':getattr(error,'filename',None),'traceback':traceback.format_exc()})
    finally:
        if tmp is not None:
            try: tmp.cleanup()
            except BaseException as error:
                result['errors'].append({'stage':'build-directory-cleanup','error':repr(error),'traceback':traceback.format_exc()})
        if candidate and not result['errors']: result['status']='passed'; result['stage']='complete'
        (out/'run.json').write_text(json.dumps(result,indent=2))
    return 0 if result['status']=='passed' else 1


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--toolchain-bin',type=Path,required=True)
    args=parser.parse_args(); out=args.output.resolve(); out.mkdir(mode=0o700)
    def interrupted(sig,frame): raise RuntimeError(f'interrupted signal {sig}')
    for sig in (signal.SIGINT,signal.SIGTERM): signal.signal(sig,interrupted)
    return execute(out,args.toolchain_bin)


if __name__=='__main__': raise SystemExit(main())
