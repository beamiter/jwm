/* Owned X11 fixture: records actual KeyPress text, no shell or network. */
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/Xatom.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <fcntl.h>
int main(int argc, char **argv) {
    if (argc != 4) return 2;
    Display *d = XOpenDisplay(NULL); if (!d) return 3;
    int fd = open(argv[2], O_WRONLY|O_CREAT|O_EXCL|O_CLOEXEC, 0600);
    if (fd < 0) return 4;
    Window w = XCreateSimpleWindow(d, DefaultRootWindow(d), 50,50,360,240,0,0,0x223344);
    XStoreName(d,w,argv[1]);
    XClassHint hint = {.res_name=argv[1],.res_class=argv[1]}; XSetClassHint(d,w,&hint);
    unsigned long pid = (unsigned long)getpid();
    XChangeProperty(d,w,XInternAtom(d,"_NET_WM_PID",False),XA_CARDINAL,32,PropModeReplace,(unsigned char*)&pid,1);
    XWMHints input = {.flags=InputHint,.input=True}; XSetWMHints(d,w,&input);
    XSelectInput(d,w,KeyPressMask|StructureNotifyMask); XMapWindow(d,w); XSync(d,False);
    FILE *ready = fopen(argv[3],"wx"); if (!ready) return 5;
    fprintf(ready,"%lu %ld\n",w,(long)getpid()); if(fclose(ready)!=0) return 6;
    for (;;) {
        XEvent event; XNextEvent(d,&event);
        if (event.type == DestroyNotify) break;
        if (event.type == KeyPress) {
            char buf[64]; KeySym sym; int n = XLookupString(&event.xkey,buf,sizeof(buf),&sym,NULL);
            if (n > 0 && write(fd,buf,(size_t)n) != n) return 7;
        }
    }
    close(fd); XCloseDisplay(d); return 0;
}
