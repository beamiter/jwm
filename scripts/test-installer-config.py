#!/usr/bin/env python3
"""Exercise only the actual config-edit function; never install software."""
from pathlib import Path
import os
import subprocess
import sys
import tempfile
import tomllib
source = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parents[1] / 'scripts/install_jwm_scripts.sh'
s = source.read_text()
a = s.index('update_toml_status_bar_name() ')
b = s.index('\nsync_selected_bar_config() {', a)
function = 'err() { printf "%s\\n" "$*" >&2; }\n' + s[a:b]
def run(path):
    return subprocess.run(['bash', '-c', function + '\nupdate_toml_status_bar_name "$1" tao_glow_bar', 'bash', str(path)], capture_output=True, text=True, timeout=5)
with tempfile.TemporaryDirectory(prefix='jwm-config-test-') as directory:
    root = Path(directory)
    normal = '[status_bar]\nname="old"\nshow_bar=false\n[other]\nx=7\n'
    cases = [('normal',normal,True),('new-section','[other]\nx=7\n',True),('multiline','command="""\n[status_bar]\nname="literal"\n"""\n[status_bar]\nname="old"\n',False),('invalid','this is not TOML',False),('inline','status_bar={name="old",show_bar=false}\n',False)]
    for name, initial, success in cases:
        path=root/(name+'.toml'); path.write_text(initial); path.chmod(0o640)
        result=run(path)
        assert (result.returncode==0)==success,(name,result.stderr)
        if success:
            before=tomllib.loads(initial); after=tomllib.loads(path.read_text())
            assert after['status_bar']['name']=='tao_glow_bar'
            for item in (before,after):
                if isinstance(item.get('status_bar'),dict):
                    item['status_bar'].pop('name',None)
                    if not item['status_bar']: item.pop('status_bar')
            assert before==after,name
            assert path.stat().st_mode&0o777==0o640,name
        else: assert path.read_text()==initial,name
        assert not list(root.glob(name+'.toml.tmp.*')),name
        print(name,'PASS')
    target=root/'target';target.write_text(normal)
    link=root/'link';link.symlink_to(target)
    result=run(link)
    assert result.returncode!=0 and link.is_symlink() and target.read_text()==normal
    print('symlink-preserved PASS')
    fifo=root/'fifo';os.mkfifo(fifo)
    assert run(fifo).returncode!=0
    print('fifo-refused PASS')
print('7 cases PASS')
