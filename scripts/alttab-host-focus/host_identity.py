#!/usr/bin/env python3
"""Read XRes server-attested local client PID for one native host XID.
Only run with the probe's private DISPLAY/XAUTHORITY. No property/PID fallback.
"""
import ctypes as C
import json
import sys

class Spec(C.Structure):
    _fields_=[('client',C.c_ulong),('mask',C.c_uint)]
class Value(C.Structure):
    _fields_=[('spec',Spec),('length',C.c_long),('value',C.c_void_p)]

def main():
    if len(sys.argv)!=2 or not sys.argv[1].isdigit():raise RuntimeError('expected decimal XID')
    xid=int(sys.argv[1])
    if not 0<xid<=0xffffffff:raise RuntimeError('invalid XID')
    x11=C.CDLL('libX11.so.6');res=C.CDLL('libXRes.so.1')
    x11.XOpenDisplay.argtypes=[C.c_char_p];x11.XOpenDisplay.restype=C.c_void_p
    x11.XCloseDisplay.argtypes=[C.c_void_p];x11.XCloseDisplay.restype=C.c_int
    res.XResQueryClientIds.argtypes=[C.c_void_p,C.c_long,C.POINTER(Spec),C.POINTER(C.c_long),C.POINTER(C.POINTER(Value))]
    res.XResQueryClientIds.restype=C.c_int
    res.XResGetClientPid.argtypes=[C.POINTER(Value)];res.XResGetClientPid.restype=C.c_int
    res.XResClientIdsDestroy.argtypes=[C.c_long,C.POINTER(Value)];res.XResClientIdsDestroy.restype=None
    display=x11.XOpenDisplay(None)
    if not display:raise RuntimeError('private X display unavailable')
    count=C.c_long(0);values=C.POINTER(Value)()
    try:
        spec=Spec(xid,2) # XRES_CLIENT_ID_PID_MASK, XRes v1.2
        status=res.XResQueryClientIds(display,1,C.byref(spec),C.byref(count),C.byref(values))
        if status!=0 or not 0<count.value<=16:raise RuntimeError('server XRes PID unavailable')
        pids=[res.XResGetClientPid(C.byref(values[i])) for i in range(count.value)]
        pids=[pid for pid in pids if pid>0]
        if len(pids)!=1:raise RuntimeError('ambiguous/missing server client PID')
        print(json.dumps({'raw_xid':xid,'server_pid':pids[0],'source':'XResQueryClientIds'}))
    finally:
        if values:res.XResClientIdsDestroy(count,values)
        x11.XCloseDisplay(display)
if __name__=='__main__':main()
