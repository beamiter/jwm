/* Real Xlib requests against the fixture's verified managed top dock.
 * No synthetic events are sent by this client: only the WM can supply the ack.
 */
#include <X11/Xlib.h>
#include <X11/Xatom.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static double now(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}
static int geometry(Display *d, Window w, XWindowAttributes *a) {
    if (!XGetWindowAttributes(d,w,a)) return 0;
    return a->map_state == IsViewable && !a->override_redirect;
}
static int same(XWindowAttributes *a, XWindowAttributes *b) {
    return a->x==b->x && a->y==b->y && a->width==b->width && a->height==b->height && a->border_width==b->border_width;
}
static int owner(Display *d, Window w, unsigned long pid) {
    Atom type; int format; unsigned long count,rest; unsigned char *data=NULL;
    int ok=XGetWindowProperty(d,w,XInternAtom(d,"_NET_WM_PID",False),0,1,False,XA_CARDINAL,
                             &type,&format,&count,&rest,&data)==Success;
    ok=ok && type==XA_CARDINAL && format==32 && count==1 && rest==0 && data && *(unsigned long*)data==pid;
    if(data) XFree(data);
    return ok;
}
int main(int argc,char **argv) {
    if(argc!=3) return 2;
    Window w=strtoul(argv[1],NULL,0); unsigned long pid=strtoul(argv[2],NULL,10);
    Display *d=XOpenDisplay(NULL); if(!d) return 2;
    Window root,parent,*children=NULL; unsigned int count;
    XWindowAttributes baseline;
    if(!owner(d,w,pid) || !geometry(d,w,&baseline) || !XQueryTree(d,w,&root,&parent,&children,&count)) return 3;
    if(children) XFree(children);
    if(parent!=root) return 3;
    XSelectInput(d,w,StructureNotifyMask); XSync(d,False);
    /* Discard startup traffic before issuing the first measured request. */
    double quiet=now()+0.3;
    while(now()<quiet) { while(XPending(d)){ XEvent e;XNextEvent(d,&e);quiet=now()+0.3; } usleep(1000); }
    const char *names[]={"rejected_resize","same_geometry","rejected_position","rejected_border","stack_only"};
    int failed=0;
    printf("{\"window\":%lu,\"pid\":%lu,\"geometry\":[%d,%d,%d,%d,%d],\"cases\":[",w,pid,baseline.x,baseline.y,baseline.width,baseline.height,baseline.border_width);
    for(int i=0;i<5;i++) {
        XWindowAttributes before,after={0};
        if(!owner(d,w,pid) || !geometry(d,w,&before) || !same(&baseline,&before)) return 4;
        while(XPending(d)){XEvent e;XNextEvent(d,&e);}
        XWindowChanges request={.x=baseline.x,.y=baseline.y,.width=baseline.width,.height=baseline.height,.border_width=baseline.border_width,.stack_mode=Above};
        unsigned int mask;
        if(i==0){request.width+=10;mask=CWWidth|CWHeight;}
        else if(i==1)mask=CWX|CWY|CWWidth|CWHeight|CWBorderWidth;
        else if(i==2){request.x+=10;request.y+=10;mask=CWX|CWY;}
        else if(i==3){request.border_width+=2;mask=CWBorderWidth;}
        else mask=CWStackMode;
        unsigned long serial=NextRequest(d);
        XConfigureWindow(d,w,mask,&request);XFlush(d);
        int synthetic=0,real=0,bad=0;double deadline=now()+2.0;
        printf("%s{\"name\":\"%s\",\"mask\":%u,\"request_serial\":%lu,\"events\":[",i?",":"",names[i],mask,serial);
        int events=0;
        while(now()<deadline) {
            while(XPending(d)) {
                XEvent e;XNextEvent(d,&e);
                if(e.type!=ConfigureNotify || e.xconfigure.window!=w) continue;
                XConfigureEvent *c=&e.xconfigure;
                printf("%s{\"send_event\":%s,\"event\":%lu,\"window\":%lu,\"geometry\":[%d,%d,%d,%d,%d],\"above\":%lu,\"override_redirect\":%s}",events++?",":"",c->send_event?"true":"false",c->event,c->window,c->x,c->y,c->width,c->height,c->border_width,c->above,c->override_redirect?"true":"false");
                if(c->send_event){synthetic++;bad|=c->event!=w || c->x!=baseline.x || c->y!=baseline.y || c->width!=baseline.width || c->height!=baseline.height || c->border_width!=baseline.border_width || c->above!=None || c->override_redirect;}
                else real++;
            }
            usleep(1000);
        }
        int stable=owner(d,w,pid) && geometry(d,w,&after) && same(&baseline,&after);
        int pass=synthetic==1 && !bad && stable;
        failed|=!pass;
        printf("],\"synthetic_count\":%d,\"real_count\":%d,\"server_geometry\":[%d,%d,%d,%d,%d],\"server_geometry_unchanged\":%s,\"passed\":%s}",synthetic,real,after.x,after.y,after.width,after.height,after.border_width,stable?"true":"false",pass?"true":"false");fflush(stdout);
    }
    printf("],\"passed\":%s}\n",failed?"false":"true");XCloseDisplay(d);return failed?1:0;
}
