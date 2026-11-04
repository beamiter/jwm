/* Owned native XCB window fixture: no model, IPC permit or user desktop. */
#include <xcb/xcb.h>
#include <poll.h>
#include <fcntl.h>
#include <unistd.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <stdint.h>

static xcb_connection_t *connection;
static xcb_screen_t *screen;
static xcb_window_t window;
static xcb_atom_t protocols, delete_window, name_atom, pid_atom, class_atom;
static const char *name;
static unsigned generation;
static int present;
static int pending_delete;
static xcb_atom_t atom(const char *text) {
    xcb_intern_atom_reply_t *reply=xcb_intern_atom_reply(connection,xcb_intern_atom(connection,0,strlen(text),text),NULL);
    if(!reply){exit(2);}
    xcb_atom_t result=reply->atom;free(reply);return result;
}
static void event(const char *kind) {
    printf("{\"event\":\"%s\",\"pid\":%d,\"xid\":%u,\"generation\":%u}\n",kind,getpid(),window,generation);fflush(stdout);
}
static void barrier(void) {free(xcb_get_input_focus_reply(connection,xcb_get_input_focus(connection),NULL));}
static void create(void) {
    uint32_t values[]={screen->white_pixel,XCB_EVENT_MASK_STRUCTURE_NOTIFY|XCB_EVENT_MASK_FOCUS_CHANGE|XCB_EVENT_MASK_EXPOSURE};
    xcb_void_cookie_t created=xcb_create_window_checked(connection,XCB_COPY_FROM_PARENT,window,screen->root,90,90,360,220,0,XCB_WINDOW_CLASS_INPUT_OUTPUT,screen->root_visual,XCB_CW_BACK_PIXEL|XCB_CW_EVENT_MASK,values);
    xcb_generic_error_t *error=xcb_request_check(connection,created);if(error){free(error);exit(3);}
    char title[128];snprintf(title,sizeof(title),"OwnedClose-%s-%u",name,generation);
    const char klass[]="OwnedNativeClose\0OwnedNativeClose\0";uint32_t pid=(uint32_t)getpid();
    xcb_change_property(connection,XCB_PROP_MODE_REPLACE,window,name_atom,XCB_ATOM_STRING,8,strlen(title),title);
    xcb_change_property(connection,XCB_PROP_MODE_REPLACE,window,class_atom,XCB_ATOM_STRING,8,sizeof(klass)-1,klass);
    xcb_change_property(connection,XCB_PROP_MODE_REPLACE,window,pid_atom,XCB_ATOM_CARDINAL,32,1,&pid);
    xcb_change_property(connection,XCB_PROP_MODE_REPLACE,window,protocols,XCB_ATOM_ATOM,32,1,&delete_window);
    xcb_map_window(connection,window);xcb_flush(connection);barrier();present=1;event("created");
}
int main(int argc,char **argv) {
    if(argc!=3){return 2;}
    name=argv[1];setvbuf(stdout,NULL,_IOLBF,0);
    int index;connection=xcb_connect(NULL,&index);if(xcb_connection_has_error(connection))return 2;
    xcb_screen_iterator_t screens=xcb_setup_roots_iterator(xcb_get_setup(connection));while(index--)xcb_screen_next(&screens);screen=screens.data;
    window=xcb_generate_id(connection);protocols=atom("WM_PROTOCOLS");delete_window=atom("WM_DELETE_WINDOW");name_atom=atom("WM_NAME");pid_atom=atom("_NET_WM_PID");class_atom=atom("WM_CLASS");
    int control=open(argv[2],O_RDWR|O_NONBLOCK);if(control<0)return 2;create();
    for(;;) {
        struct pollfd fds[]={{xcb_get_file_descriptor(connection),POLLIN,0},{control,POLLIN,0}};
        if(poll(fds,2,-1)<0)continue;
        if(fds[1].revents&POLLIN) {
            char text[64]={0};ssize_t count=read(control,text,sizeof(text)-1);
            if(count>0&&(strstr(text,"destroy")||strstr(text,"recycle"))) {
                if(present){event("fixture_destroy");xcb_destroy_window(connection,window);xcb_flush(connection);barrier();present=0;}
                if(strstr(text,"recycle")){generation++;create();}
            }
            if(count>0&&strstr(text,"finish_close")&&pending_delete) {
                event("close_ack");if(present)xcb_destroy_window(connection,window);xcb_flush(connection);barrier();xcb_disconnect(connection);close(control);return 0;
            }
            if(count>0&&strstr(text,"quit"))break;
        }
        xcb_generic_event_t *raw;
        while((raw=xcb_poll_for_event(connection))) {
            uint8_t type=raw->response_type&127;
            if(type==XCB_FOCUS_IN||type==XCB_FOCUS_OUT) {
                xcb_focus_in_event_t *focus=(xcb_focus_in_event_t*)raw;
                printf("{\"event\":\"%s\",\"pid\":%d,\"xid\":%u,\"generation\":%u,\"mode\":%u,\"detail\":%u}\n",type==XCB_FOCUS_IN?"focus_in":"focus_out",getpid(),window,generation,focus->mode,focus->detail);fflush(stdout);
            }
            if(type==XCB_CLIENT_MESSAGE) {
                xcb_client_message_event_t *message=(xcb_client_message_event_t*)raw;
                if(message->type==protocols&&message->data.data32[0]==delete_window&&message->window==window) {
                    event("wm_delete_window");
                    if(getenv("JWM_NATIVE_CLIENT_HOLD_CLOSE")){pending_delete=1;free(raw);continue;}
                    free(raw);if(present)xcb_destroy_window(connection,window);xcb_flush(connection);barrier();xcb_disconnect(connection);close(control);return 0;
                }
            }
            free(raw);
        }
        if(xcb_connection_has_error(connection))break;
    }
    xcb_disconnect(connection);close(control);return 0;
}
