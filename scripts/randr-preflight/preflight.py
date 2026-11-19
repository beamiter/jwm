#!/usr/bin/env python3
"""Prerequisite only. Called exclusively by owned_xorg.py, never an inherited DISPLAY."""
import re


def run(out, xr):
    def snapshot(label, active):
        text = xr('--verbose'); monitors = xr('--listactivemonitors')
        (out / (label+'.verbose')).write_text(text)
        (out / (label+'.monitors')).write_text(monitors)
        rows = {}
        for block in re.split(r'(?m)(?=^\S+ (?:connected|disconnected))', text)[1:]:
            name = block.split()[0]
            identifier = re.search(r'(?m)^\s*Identifier:\s*(0x[0-9a-fA-F]+)', block)
            if identifier: rows[name] = int(identifier[1], 16)
        assert all(rows.get(n, 0) > 0 for n in ('DUMMY0','DUMMY1')), rows
        assert rows['DUMMY0'] != rows['DUMMY1'], rows
        lines = monitors.splitlines()[1:]
        # A setmonitor ... none row has no terminal output name: reject it.
        backing = [line.split()[-1] for line in lines]
        assert len(lines) == len(active) and set(backing) == set(active), monitors
        return {n: rows[n] for n in ('DUMMY0','DUMMY1')}

    xr('--addmode','DUMMY1','1280x720')
    xr('--output','DUMMY0','--mode','1280x720','--pos','0x0',
       '--output','DUMMY1','--mode','1280x720','--pos','1280x0')
    first = snapshot('both', ['DUMMY0','DUMMY1'])
    xr('--output','DUMMY0','--off')
    assert snapshot('only-b', ['DUMMY1']) == first
    xr('--output','DUMMY0','--mode','1280x720','--pos','0x0')
    assert snapshot('restored', ['DUMMY0','DUMMY1']) == first
    return {'status':'passed','output_xids':first,
            'scope':'private Xorg RandR activation only; no physical hotplug or JWM proof'}
