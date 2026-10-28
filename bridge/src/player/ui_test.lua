-- mpv --no-config --vo=null --ao=null --idle=yes --script=.../ui_test.lua
local real = require 'mp'
local utils = require 'mp.utils'
local bindings, messages, groups, observers, commands, pending, timers = {}, {}, {}, {}, {}, {}, {}
local mouse_x, mouse_y, now, osd_w, osd_h = 0, 0, 0, 960, 600
local props = {playlist={}, ['track-list']={}, sid='no', hwdec='auto-safe', fullscreen=false,
    ['idle-active']=false, duration=120, ['time-pos']=30, volume=50, speed=1}
for i=1,300 do props.playlist[i]={filename='/music/Track '..i..'.flac',current=i==1} end
local overlay = {update=function() end,remove=function() end}
local function set(name,value)
    if name=='pause' then value=value=='yes' or value==true end
    props[name]=value
    if observers[name] then observers[name]() end
end
local mock = {
    create_osd_overlay=function() return overlay end,
    get_time=function() return now end,
    get_osd_size=function() return osd_w,osd_h end,
    get_mouse_pos=function() return mouse_x,mouse_y end,
    get_property=function(name,default) local v=props[name]; if v==nil then return default end; return v end,
    get_property_native=function(name,default) local v=props[name]; if v==nil then return default,'unavailable' end; return v end,
    set_property=set, set_property_number=set,
    commandv=function(...)
        local args={...}; commands[#commands+1]=args
        if args[1]=='cycle' then set(args[2],not props[args[2]]) end
        if args[1]=='seek' then set('time-pos',args[2]) end
    end,
    command_native_async=function(args,cb) pending[#pending+1]={args=args,cb=cb}; return #pending end,
    osd_message=function(value) commands[#commands+1]={'osd',value} end,
    abort_async_command=function() end,
    set_mouse_area=function() end,
    set_key_bindings=function(keys,group,flags) assert(flags=='force'); groups[group]=keys end,
    enable_key_bindings=function(group) groups[group].enabled=true end,
    disable_key_bindings=function(group) if groups[group] then groups[group].enabled=false end end,
    add_key_binding=function(key,name,cb) bindings[key]=cb end,
    observe_property=function(name,kind,cb) observers[name]=cb end,
    add_periodic_timer=function(_,cb) timers[#timers+1]=cb end,
    register_script_message=function(name,cb) messages[name]=cb end,
    register_event=function() end,
}
package.loaded.mp=mock
package.loaded['mp.utils']={split_path=utils.split_path,file_info=function(path)
    return {is_file=path:sub(1,7)=='/media/'}
end}
local function key(group,name,event)
    assert(groups[group].enabled,group..' should be enabled')
    for _,entry in ipairs(groups[group]) do
        if entry[1]==name then
            local cb=event and event.event=='down' and entry[3] or event and event.event=='up' and entry[4] or entry[2]
            cb(); return
        end
    end
    error('unknown key '..name)
end
local function pointer(x,y)
    mouse_x,mouse_y=x,y; bindings.mouse_move()
end
local function click(x,y)
    pointer(x,y); key('jwm-panel-mouse','MBTN_LEFT',{event='down'}); key('jwm-panel-mouse','MBTN_LEFT',{event='up'})
end
local ok,err=pcall(function()
    local source=debug.getinfo(1,'S').source:gsub('^@','')
    dofile(utils.split_path(source)..'ui.lua')
    messages['jwm-queue']()
    assert(overlay.data:find('Track 1.flac',1,true))
    assert(not overlay.data:find('Track 8.flac',1,true),'only one page is rendered')
    key('jwm-panel-keys','DOWN'); key('jwm-panel-keys','ENTER'); assert(props['playlist-pos']==1)
    click(911,356)
    assert(overlay.data:find('Track 8.flac',1,true))
    assert(not overlay.data:find('Track 1.flac',1,true))
    key('jwm-panel-keys','ENTER'); assert(props['playlist-pos']==8,'next page preserves relative row')
    click(873,356); key('jwm-panel-keys','ENTER'); assert(props['playlist-pos']==1)
    local full_playlist=props.playlist
    props.playlist={}
    for i=1,10 do props.playlist[i]=full_playlist[i] end
    observers.playlist()
    click(911,356)
    assert(overlay.data:find('Track 4.flac',1,true))
    assert(overlay.data:find('Track 10.flac',1,true))
    key('jwm-panel-keys','ENTER'); assert(props['playlist-pos']==4,'partial page preserves relative row')
    click(911,356); key('jwm-panel-keys','ENTER'); assert(props['playlist-pos']==4)
    click(873,356); key('jwm-panel-keys','ENTER'); assert(props['playlist-pos']==1)
    props.playlist=full_playlist; observers.playlist()
    for _=1,10 do key('jwm-panel-keys','DOWN') end
    key('jwm-panel-keys','ENTER'); assert(props['playlist-pos']==11)
    key('jwm-panel-keys','ESC'); assert(not groups['jwm-panel-keys'].enabled)
    click(682,28); click(700,128); assert(props['playlist-pos']==0)
    props['track-list']={{type='sub',id=7,title='中文 {\\an9}\n字幕'}}
    click(750,28)
    assert(not overlay.data:find('{\\an9}',1,true),'ASS injection escaped')
    click(700,160); assert(props.sid==7)
    click(700,128); assert(props.sid=='no')
    click(828,28); click(700,128); assert(props.hwdec=='no')
    click(700,128); assert(props.hwdec=='auto-safe')
    key('jwm-panel-keys','ESC')
    -- Custom transport controls must apply real playback commands.
    click(34,568); assert(props.pause)
    click(76,568); assert(props.mute)
    click(135,568); assert(math.abs(props.volume-50)<.01)
    pointer(135,568); key('jwm-panel-mouse','MBTN_LEFT',{event='down'})
    pointer(600,400); key('jwm-panel-mouse','MBTN_LEFT',{event='up'})
    assert(props.volume==100,'drag stays captured outside the original volume region')
    click(487,564); assert(math.abs(props['time-pos']-60)<.1)
    pointer(487,564); key('jwm-panel-mouse','MBTN_LEFT',{event='down'})
    pointer(20,400); key('jwm-panel-mouse','MBTN_LEFT',{event='up'})
    assert(props['time-pos']==0,'seek drag clamps to start')
    click(886,568); key('jwm-panel-keys','DOWN'); key('jwm-panel-keys','ENTER')
    assert(props.speed==.75); key('jwm-panel-keys','ESC')
    click(928,568); assert(props.fullscreen)
    props.pause=false; pointer(100,200); now=5; timers[1]()
    assert(overlay.data=='','timer hides both bars without further mouse movement')
    props.fullscreen=false; observers.fullscreen()
    assert(overlay.data=='','automatic hiding also works in windowed playback')
    pointer(100,200); assert(overlay.data=='','movement in the picture does not reveal bars')
    pointer(100,20)
    assert(overlay.data:find('JWM Player',1,true),'top edge reveals header')
    assert(not overlay.data:find(' / 02:00',1,true),'top edge does not reveal transport')
    now=15; timers[1]()
    assert(overlay.data:find('JWM Player',1,true),'stationary header hover pins it')
    pointer(100,200); now=19; timers[1](); assert(overlay.data=='')
    pointer(135,568)
    assert(not overlay.data:find('JWM Player',1,true),'bottom edge does not reveal header')
    assert(overlay.data:find(' / 02:00',1,true),'bottom edge reveals transport')
    key('jwm-panel-mouse','MBTN_LEFT',{event='down'})
    pointer(500,400); now=24; timers[1]()
    assert(overlay.data:find(' / 02:00',1,true),'drag pins transport beyond its hide deadline')
    key('jwm-panel-mouse','MBTN_LEFT',{event='up'})
    assert(overlay.data=='','release restores automatic hiding')
    set('pause',true)
    assert(overlay.data:find('JWM Player',1,true),'pause keeps automatic controls available')
    bindings.h(); pointer(928,568)
    assert(overlay.data=='','manual hiding overrides pause and edge hover')
    assert(not groups['jwm-panel-mouse'].enabled,'hidden controls have no invisible click targets')
    bindings.h(); assert(overlay.data:find('JWM Player',1,true),'H restores hidden bars')
    set('pause',false); pointer(100,200)
    bindings['Ctrl+h'](); now=30; timers[1]()
    assert(overlay.data:find('JWM Player',1,true),'Always mode ignores idle deadlines')
    bindings['Ctrl+h'](); assert(overlay.data=='','mode cycle supports Hidden')
    bindings['Ctrl+h'](); now=34; timers[1](); assert(overlay.data=='','mode cycle returns to Auto')
    click(828,28)
    assert(overlay.data:find('Top bar  ·  Auto',1,true))
    click(700,288) -- Header: Auto -> Always
    click(700,320) -- Transport: Auto -> Always
    click(700,320) -- Transport: Always -> Hidden
    key('jwm-panel-keys','ESC'); pointer(100,200); now=40; timers[1]()
    assert(overlay.data:find('JWM Player',1,true),'header mode is independent')
    assert(not overlay.data:find(' / 02:00',1,true),'transport stays manually hidden')
    bindings.h(); assert(overlay.data==''); bindings.h()
    assert(overlay.data:find(' / 02:00',1,true),'H restores the last visible policies')
    bindings['Ctrl+h'](); bindings['Ctrl+h']() -- Always -> Hidden -> Auto
    pointer(100,400); assert(not groups['jwm-panel-mouse'].enabled,'picture retains native mouse handling')
    props.playlist={}; messages['jwm-queue']()
    key('jwm-panel-keys','PGDWN'); key('jwm-panel-keys','PGUP'); key('jwm-panel-keys','ENTER')
    props.playlist={{filename='/x/歌曲😀.mp3'}}; observers.playlist()
    assert(overlay.data:find('歌曲😀.mp3',1,true))
    key('jwm-panel-keys','ESC')
    -- Scale both rendering and hit testing using the same canvas transform.
    osd_w,osd_h=480,300; observers['osd-dimensions']()
    click(341,14); assert(groups['jwm-panel-keys'].enabled)
    key('jwm-panel-keys','ESC')
    osd_w,osd_h=560,420; observers['osd-dimensions']()
    click(365,20); assert(groups['jwm-panel-keys'].enabled,'compact music window keeps scaled controls reachable')
    key('jwm-panel-keys','ESC'); osd_w,osd_h=960,600
    props['idle-active']=true; observers['idle-active']()
    assert(overlay.data:find('Drop local files to play here.',1,true))
    bindings.h(); assert(not overlay.data:find('JWM Player',1,true),'idle screen respects manual hiding')
    assert(overlay.data:find('Drop local files to play here.',1,true),'idle open target remains available')
    bindings.h()
    click(480,255)
    assert(pending[1].args.args[1]=='zenity')
    bindings['Ctrl+o'](); assert(#pending==1,'only one chooser at a time')
    pending[1].cb(true,{status=1}); assert(#pending==1,'cancel does not launch a fallback')
    bindings['Ctrl+o'](); pending[2].cb(false,nil)
    assert(pending[3].args.args[1]=='kdialog','missing zenity falls back to kdialog')
    pending[3].cb(true,{status=0,stdout='/media/a ${literal}.mp4\n/media/中文.mkv\n'})
    assert(commands[#commands-1][2]=='/media/a ${literal}.mp4','filenames remain literal arguments')
    assert(commands[#commands][3]=='append')
    local count=#commands
    bindings['Ctrl+o'](); pending[4].cb(true,{status=0,stdout='https://example.com/video.mp4'})
    assert(#commands==count+1 and commands[#commands][1]=='osd','chooser rejects protocols')
end)
package.loaded.mp=real
package.loaded['mp.utils']=utils
if not ok then real.msg.error(tostring(err)); real.commandv('quit',1)
else real.msg.info('JWM UI logic tests passed'); real.commandv('quit',0) end
