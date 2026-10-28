-- JWM Player's embedded chrome. Playback stays in mpv. The optional file
-- chooser runs asynchronously with argument arrays, never through a shell.
local mp = require 'mp'
local assdraw = require 'mp.assdraw'
local utils = require 'mp.utils'
local overlay = mp.create_osd_overlay('ass-events')
overlay.z = 1000
local PAGE_ROWS = 7
local panel, offset, selected = nil, 0, 1
local regions, width, height = {}, 960, 600
local active, dragging, chooser = false, nil, nil
local pointer_present = false
local bar_modes = {top='Auto', bottom='Auto'}
local restore_modes = {top='Auto', bottom='Auto'}
local revealed = {top=mp.get_time(), bottom=mp.get_time()}
local near_edge = {top=false, bottom=false}
local top_shown, bottom_shown = false, false
local draw
-- ASS colors are BGR.
local C = {white='F2F0F1', muted='969296', line='35312F', panel='171411',
    hover='302924', blue='F4AE88'}
local function safe(value)
    return tostring(value or ''):gsub('[%c]', ' '):gsub('\\', '＼'):gsub('{', '｛'):gsub('}', '｝')
end
local function title(value, limit)
    local out, n = {}, 0
    for char in safe(value):gmatch('[%z\1-\127\194-\244][\128-\191]*') do
        n = n + 1
        if n > limit then return table.concat(out) .. '…' end
        out[#out + 1] = char
    end
    return table.concat(out)
end
local function number(name, default)
    -- Unavailable properties return nil plus an error string during track
    -- changes. Do not forward that second return value as tonumber's base.
    local value = mp.get_property_native(name)
    return tonumber(value) or default or 0
end
local function clamp(value, low, high) return math.max(low, math.min(high, value)) end
local function clock(value)
    value = math.max(0, math.floor(value or 0))
    if value >= 3600 then return string.format('%d:%02d:%02d', math.floor(value/3600), math.floor(value/60)%60, value%60) end
    return string.format('%02d:%02d', math.floor(value/60), value%60)
end
local function pointer()
    if not pointer_present then return -1,-1 end
    local x, y = mp.get_mouse_pos()
    local w, h = mp.get_osd_size()
    if not w or w <= 0 or not h or h <= 0 then return -1, -1 end
    return x * width / w, y * height / h
end
local function hit()
    local x, y = pointer()
    for i = #regions, 1, -1 do
        local r = regions[i]
        if x >= r.x and x <= r.x+r.w and y >= r.y and y <= r.y+r.h then return r end
    end
end
local function idle() return mp.get_property_native('idle-active', true) end
local function bar_visible(bar)
    if bar_modes[bar]=='Hidden' then return false end
    if bar_modes[bar]=='Always' or idle() or mp.get_property_native('pause',false) or chooser then return true end
    if panel and (bar=='bottom' and panel=='Speed' or bar=='top' and panel~='Speed') then return true end
    if dragging and bar=='bottom' then return true end
    local x,y=pointer()
    if x>=0 and x<=width and y>=0 and y<=height
        and (bar=='top' and y<=72 or bar=='bottom' and y>=height-84) then return true end
    return mp.get_time()-revealed[bar]<3
end
local function set_bar_mode(bar,mode)
    if mode~='Hidden' then restore_modes[bar]=mode
    elseif bar_modes[bar]~='Hidden' then restore_modes[bar]=bar_modes[bar] end
    bar_modes[bar]=mode
    revealed[bar]=mp.get_time()
    if mode=='Hidden' then
        if bar=='bottom' then dragging=nil end
        if panel and (bar=='top' and panel~='Speed' or bar=='bottom' and panel=='Speed') then panel=nil end
    end
end
local function toggle_bars()
    local hidden=bar_modes.top=='Hidden' and bar_modes.bottom=='Hidden'
    set_bar_mode('top',hidden and restore_modes.top or 'Hidden')
    set_bar_mode('bottom',hidden and restore_modes.bottom or 'Hidden')
    panel=nil
    mp.osd_message(hidden and 'Bars restored' or 'Bars hidden · H to restore',2)
    draw()
end
local next_mode={Auto='Always', Always='Hidden', Hidden='Auto'}
local function cycle_bar_mode(bar)
    local mode=next_mode[bar_modes[bar]]
    set_bar_mode(bar,mode)
    mp.osd_message((bar=='top' and 'Top' or 'Bottom')..' bar: '..mode..' · H hides / restores both',2)
    draw()
end
local function cycle_bars()
    local mode=next_mode[bar_modes.top]
    set_bar_mode('top',mode); set_bar_mode('bottom',mode)
    mp.osd_message('Bars: '..mode..' · H hides / restores',2)
    draw()
end
local function toggle(name)
    panel = panel ~= name and name or nil
    offset, selected = 0, 1
    draw()
end
local function items()
    if panel == 'Queue' then
        local list = {}
        for i, entry in ipairs(mp.get_property_native('playlist', {})) do
            local index = i - 1
            local _, name = utils.split_path(entry.filename or '')
            list[#list+1] = {label=entry.title or name, current=entry.current,
                action=function()
                    mp.set_property_number('playlist-pos', index)
                    mp.set_property('pause', 'no')
                end}
        end
        return list
    elseif panel == 'Subtitles' then
        local list = {{label='Off', current=mp.get_property('sid') == 'no',
            action=function() mp.set_property('sid', 'no') end}}
        for _, track in ipairs(mp.get_property_native('track-list', {})) do
            if track.type == 'sub' then
                local id = track.id
                list[#list+1] = {label=track.title or track.lang or ('Subtitle '..id),
                    current=track.selected, action=function() mp.set_property_number('sid', id) end}
            end
        end
        return list
    elseif panel == 'Speed' then
        local list = {}
        for _, speed in ipairs({0.5, 0.75, 1, 1.25, 1.5, 1.75, 2}) do
            local value = speed
            list[#list+1] = {label=string.format('%g×', value)..(value==1 and '  ·  Normal' or ''),
                current=math.abs(number('speed', 1)-value)<0.01,
                action=function() mp.set_property_number('speed', value) end}
        end
        return list
    elseif panel == 'Settings' then
        local hw = mp.get_property('hwdec', 'no') ~= 'no'
        return {
            {label='Hardware decoding  ·  '..(hw and 'Auto' or 'Off'), current=hw,
                action=function() mp.set_property('hwdec', hw and 'no' or 'auto-safe') end},
            {label='Always on top', current=mp.get_property_native('ontop',false),
                action=function() mp.commandv('cycle','ontop') end},
            {label='Show subtitles', current=mp.get_property_native('sub-visibility',true),
                action=function() mp.commandv('cycle','sub-visibility') end},
            {label='Reset subtitle delay', action=function() mp.set_property_number('sub-delay',0) end},
            {label='Playback statistics', action=function() panel=nil; mp.commandv('script-binding','stats/display-stats-toggle') end},
            {label='Top bar  ·  '..bar_modes.top, action=function() cycle_bar_mode('top') end},
            {label='Bottom bar  ·  '..bar_modes.bottom, action=function() cycle_bar_mode('bottom') end},
        }
    end
    return {}
end
local function page(direction)
    local count, previous = #items(), offset
    offset = clamp(offset+direction*PAGE_ROWS,0,math.max(0,count-PAGE_ROWS))
    selected = clamp(selected+offset-previous,1,math.max(1,count))
    draw()
end
local function open_files()
    if chooser then return end
    local function launch(args, fallback)
        chooser = mp.command_native_async({name='subprocess', args=args, playback_only=false,
            capture_stdout=true, capture_stderr=true}, function(success, result)
            chooser = nil
            if not success or not result or result.status < 0 or result.status == 127 then
                if fallback then launch({'kdialog','--getopenfilename','.','*','--multiple','--separate-output'})
                else mp.osd_message('File chooser unavailable. Drop local files here, or open with JWM Video.',4) end
                return
            end
            if result.status ~= 0 then return end -- Cancel is not an error.
            local files, bytes = {}, 0
            for path in (result.stdout or ''):gmatch('[^\r\n]+') do
                local info = path:sub(1,1)=='/' and utils.file_info(path)
                if not info or not info.is_file then mp.osd_message('Choose regular local media files.',3); return end
                bytes = bytes+#path
                files[#files+1] = path
            end
            if #files>512 or bytes>128*1024 then
                mp.osd_message('Choose at most 512 files (128 KiB of file paths).',3); return
            end
            for i, path in ipairs(files) do mp.commandv('loadfile',path,i==1 and 'replace' or 'append') end
            if #files>0 then mp.set_property('pause','no'); panel=nil; draw() end
        end)
    end
    launch({'zenity','--file-selection','--multiple','--separator=\n','--title=Open media — JWM Player'},true)
    draw()
end

draw = function()
    local w, h = mp.get_osd_size()
    if not w or w<=0 or not h or h<=0 then return end
    -- Preserve aligned physical hit boxes even in portrait or tiny windows.
    height = math.max(600,600*h/w)
    width = height*w/h
    regions = {}
    local px, py = pointer()
    local is_idle = idle()
    top_shown,bottom_shown = bar_visible('top'),bar_visible('bottom')
    local ass = assdraw.ass_new()
    local function shape(path, color, alpha, blur, clip)
        ass:new_event()
        ass:pos(0,0)
        ass:append(string.format('{\\an7\\bord0\\shad0\\1c&H%s&\\alpha&H%02X&\\blur%g}',color,alpha or 0,blur or 0))
        if clip then ass:append(clip) end
        -- assdraw defaults to p4 (1/8-pixel coordinates). Match its coord()
        -- convention for these compound paths as well as individual shapes.
        ass:draw_start()
        ass:append((path:gsub('%S+',function(token)
            local value=tonumber(token)
            return value and tostring(math.ceil(value*8)) or token
        end)))
        ass:draw_stop()
    end
    local function rect(x,y,rw,rh,color,alpha,radius)
        local r = math.min(radius or 0,rw/2,rh/2)
        if rw<=0 or rh<=0 then return end
        if r==0 then
            shape(string.format('m %g %g l %g %g %g %g %g %g',x,y,x+rw,y,x+rw,y+rh,x,y+rh),color,alpha)
        else
            local k = r*.5523
            shape(string.format('m %g %g l %g %g b %g %g %g %g %g %g l %g %g b %g %g %g %g %g %g l %g %g b %g %g %g %g %g %g l %g %g b %g %g %g %g %g %g',
                x+r,y,x+rw-r,y,x+rw-r+k,y,x+rw,y+r-k,x+rw,y+r,
                x+rw,y+rh-r,x+rw,y+rh-r+k,x+rw-r+k,y+rh,x+rw-r,y+rh,
                x+r,y+rh,x+r-k,y+rh,x,y+rh-r+k,x,y+rh-r,
                x,y+r,x,y+r-k,x+r-k,y,x+r,y),color,alpha)
        end
    end
    local function circle(x,y,r,color,alpha,blur)
        local k = r*.5523
        shape(string.format('m %g %g b %g %g %g %g %g %g b %g %g %g %g %g %g b %g %g %g %g %g %g b %g %g %g %g %g %g',
            x+r,y,x+r,y+k,x+k,y+r,x,y+r,x-k,y+r,x-r,y+k,x-r,y,
            x-r,y-k,x-k,y-r,x,y-r,x+k,y-r,x+r,y-k,x+r,y),color,alpha,blur)
    end
    local function text(x,y,value,size,color,align)
        ass:new_event(); ass:pos(x,y)
        ass:append(string.format('{\\an%d\\fnsans-serif\\fs%g\\bord0\\shad0\\1c&H%s&}%s',
            align or 7,size,color or C.white,safe(value)))
    end
    local function line(x1,y1,x2,y2,color,thickness)
        local dx,dy = x2-x1,y2-y1
        local length = math.sqrt(dx*dx+dy*dy)
        if length==0 then return end
        local a,b = -dy/length*(thickness or 1.3)/2,dx/length*(thickness or 1.3)/2
        shape(string.format('m %g %g l %g %g %g %g %g %g',x1+a,y1+b,x2+a,y2+b,x2-a,y2-b,x1-a,y1-b),color)
    end
    local function icon(name,x,y,color,size)
        local s = (size or 20)/24
        local function seg(a,b,c,d) line(x+a*s,y+b*s,x+c*s,y+d*s,color,1.5*s) end
        local function box(a,b,c,d) seg(a,b,c,b); seg(c,b,c,d); seg(c,d,a,d); seg(a,d,a,b) end
        local function poly(points)
            local path = ''
            for i,p in ipairs(points) do path=path..string.format('%s %g %g ',i==1 and 'm' or 'l',x+p[1]*s,y+p[2]*s) end
            shape(path,color)
        end
        if name=='play' then poly({{7,3},{21,12},{7,21}})
        elseif name=='pause' then poly({{6,3},{10,3},{10,21},{6,21}}); poly({{15,3},{19,3},{19,21},{15,21}})
        elseif name=='folder' then
            seg(3,6,10,6); seg(10,6,12,9); seg(12,9,21,9); seg(21,9,21,20); seg(21,20,3,20); seg(3,20,3,6)
        elseif name=='queue' then
            for i=0,2 do seg(4,6+i*6,6,6+i*6); seg(10,6+i*6,21,6+i*6) end
        elseif name=='subtitles' then box(3,4,21,17); seg(7,17,7,21); seg(7,21,12,17); seg(7,9,17,9); seg(7,13,13,13)
        elseif name=='volume' or name=='mute' then
            poly({{3,9},{7,9},{12,4},{12,20},{7,15},{3,15}})
            if name=='mute' then seg(16,8,22,16); seg(22,8,16,16)
            else seg(16,8,18,10); seg(18,10,18,14); seg(18,14,16,16); seg(20,4,23,8); seg(23,8,23,16); seg(23,16,20,20) end
        elseif name=='fullscreen' then
            seg(3,9,3,3); seg(3,3,9,3); seg(15,3,21,3); seg(21,3,21,9)
            seg(21,15,21,21); seg(21,21,15,21); seg(9,21,3,21); seg(3,21,3,15)
        elseif name=='restore' then box(5,7,17,19); seg(9,3,21,3); seg(21,3,21,15)
        elseif name=='screen' then box(2,3,22,17); seg(12,17,12,22); seg(7,22,17,22)
        elseif name=='close' then seg(6,6,18,18); seg(18,6,6,18)
        elseif name=='minimize' then seg(5,12,19,12)
        elseif name=='maximize' then box(5,5,19,19)
        elseif name=='up' then seg(6,15,12,9); seg(12,9,18,15)
        elseif name=='down' then seg(6,9,12,15); seg(12,15,18,9)
        elseif name=='check' then seg(5,12,10,17); seg(10,17,20,7)
        elseif name=='hide' then
            seg(2,12,7,6); seg(7,6,17,6); seg(17,6,22,12)
            seg(2,12,7,18); seg(7,18,17,18); seg(17,18,22,12)
            seg(3,3,21,21)
        elseif name=='settings' then
            for i=0,7 do
                local a=i*math.pi/4
                seg(12+8*math.cos(a),12+8*math.sin(a),12+11*math.cos(a),12+11*math.sin(a))
            end
            circle(x+12*s,y+12*s,8*s,color); circle(x+12*s,y+12*s,6.4*s,C.panel)
            circle(x+12*s,y+12*s,3*s,color); circle(x+12*s,y+12*s,1.5*s,C.panel)
        end
    end
    local function region(x,y,rw,rh,callback,tip,drag)
        regions[#regions+1] = {x=x,y=y,w=rw,h=rh,action=callback,tip=tip,drag=drag}
    end
    local function button(x,y,rw,rh,name,callback,tip,chosen)
        local over = px>=x and px<=x+rw and py>=y and py<=y+rh
        if chosen or over then rect(x,y,rw,rh,C.hover,chosen and 25 or 65,6) end
        if name then icon(name,x+(rw-18)/2,y+(rh-18)/2,chosen and C.blue or C.white,18) end
        region(x,y,rw,rh,callback,tip)
    end

    if is_idle then
        rect(0,0,width,height,'050403')
        -- Vector light ribbon: no bitmap/network dependency, sharp at any DPI.
        local function arc(color,thickness,alpha,blur,clip)
            local x,y = width,height
            shape(string.format('m %g %g b %g %g %g %g %g %g b %g %g %g %g %g %g l %g %g b %g %g %g %g %g %g b %g %g %g %g %g %g',
                -40,y*.83,x*.2,y*.83,x*.34,y*.74,x*.55,y*.75,
                x*.78,y*.75,x*.9,y*.57,x+20,y*.31,
                x+20,y*.31+thickness,x*.9,y*.57+thickness,x*.78,y*.75+thickness,x*.55,y*.75+thickness,
                x*.34,y*.74+thickness,x*.2,y*.83+thickness,-40,y*.83+thickness),color,alpha,blur,clip)
        end
        arc('C76565',32,185,42); arc('D88691',16,155,24)
        arc('E9A095',6,120,12); arc('FFC7A8',2,75,4)
        for i=0,9 do
            local color=string.format('FF%02X%02X',math.floor(185-i*2.5),math.floor(146+i*4))
            local clip=string.format('{\\clip(%d,0,%d,%d)}',math.floor(width*i/10),math.ceil(width*(i+1)/10),math.ceil(height))
            arc(color,5,math.floor(130+math.abs(i-5)*13),14,clip)
        end
        local cx,cy = width/2,height*.425
        circle(cx,cy,49,'CB8C91',205,14)
        circle(cx,cy,47,'D8A49A',45)
        circle(cx,cy,45.7,'30211F')
        circle(cx,cy+10,30,'72432F',200,15)
        circle(cx+15,cy+15,24,'AD6B77',220,17)
        icon('play',cx-18,cy-19,'F5DEDD',40)
        region(cx-49,cy-49,98,98,open_files,'Open local files')
        text(cx,cy+64,'Drop local files to play here.',27,C.white,8)
        text(cx,cy+104,'Music, videos and your JWM recordings.',13.5,C.muted,8)
    end
    if top_shown then
        if not is_idle then
            for i=0,11 do
                rect(0,i*5,width,5,'080706',35+i*18)
            end
        end
        rect(20,20,21,18,'B69784',0,3); rect(21.2,21.2,18.6,15.6,'080706',0,2)
        icon('play',24,23,C.white,12)
        text(51,20,'JWM Player',15,C.white)
        button(151,12,32,32,'hide',toggle_bars,'Hide bars · H')
        local header_y,bh = 12,32
        local windows_x = width-106
        rect(windows_x,header_y,92,bh,C.panel,75,7)
        button(windows_x,header_y,30,bh,'minimize',function() mp.set_property('window-minimized','yes') end,'Minimize')
        button(windows_x+31,header_y,30,bh,mp.get_property_native('window-maximized',false) and 'restore' or 'maximize',
            function() mp.commandv('cycle','window-maximized') end,'Maximize / restore')
        button(windows_x+62,header_y,30,bh,'close',function() mp.commandv('quit') end,'Close player')
        local tools_x = windows_x-140
        rect(tools_x,header_y,130,bh,C.panel,65,7)
        button(tools_x,header_y,94,bh,nil,function() toggle('Subtitles') end,'Subtitles · S',panel=='Subtitles')
        icon('subtitles',tools_x+11,header_y+8,C.white,16)
        text(tools_x+35,header_y+9,'Subtitles',11,C.white)
        button(tools_x+96,header_y,34,bh,'settings',function() toggle('Settings') end,'Playback settings',panel=='Settings')
        local files_x = tools_x-94
        rect(files_x,header_y,82,bh,C.panel,65,7)
        button(files_x,header_y,40,bh,'folder',open_files,'Open files · Ctrl+O')
        button(files_x+42,header_y,40,bh,'queue',function() toggle('Queue') end,'Play queue · L',panel=='Queue')
        line(files_x+90,20,files_x+90,36,C.line,1)
    end
    if bottom_shown then
        if not is_idle then
            for i=0,11 do rect(0,height-72+i*6,width,6,'080706',230-i*17) end
        end
        local bottom = height-49
        line(12,height-57,width-12,height-57,'24211D',.8)
        button(17,bottom,34,34,not is_idle and not mp.get_property_native('pause',false) and 'pause' or 'play',
            function() if is_idle then open_files() else mp.commandv('cycle','pause') end end,'Play / pause · Space')
        local muted = mp.get_property_native('mute',false)
        button(61,bottom,30,34,muted and 'mute' or 'volume',function() mp.commandv('cycle','mute') end,'Mute · M')
        local volume,vx,vw = clamp(number('volume',100),0,100),99,72
        rect(vx,bottom+16,vw,2.4,C.line,0,1)
        rect(vx,bottom+16,vw*volume/100,2.4,C.blue,0,1)
        circle(vx+vw*volume/100,bottom+17.2,3.5,muted and C.muted or 'EACED8')
        local function set_volume(x) mp.set_property_number('volume',clamp((x-vx)/vw*100,0,100)) end
        region(vx-6,bottom+6,vw+12,23,set_volume,'Volume',true)
        local fsx = width-50
        local speedx,screenx,subx = fsx-42,fsx-82,fsx-122
        button(fsx,bottom,34,34,mp.get_property_native('fullscreen',false) and 'restore' or 'fullscreen',
            function() mp.commandv('cycle','fullscreen') end,'Fullscreen · F')
        button(screenx,bottom,34,34,'screen',function() mp.commandv('cycle','ontop') end,'Always on top',mp.get_property_native('ontop',false))
        button(subx,bottom,34,34,'subtitles',function() toggle('Subtitles') end,'Subtitles · S',panel=='Subtitles')
        button(speedx,bottom,36,34,nil,function() toggle('Speed') end,'Playback speed',panel=='Speed')
        text(speedx+18,bottom+17,string.format('%g×',number('speed',1)),13,C.white,5)
        local sx,sw = 206,subx-226
        local duration,position = number('duration'),number('time-pos')
        local fraction = duration>0 and clamp(position/duration,0,1) or 0
        rect(sx,bottom+12,sw,2.6,C.line,0,1)
        rect(sx,bottom+12,sw*fraction,2.6,C.blue,0,1)
        circle(sx+sw*fraction,bottom+13.3,3.3,'EBD1DF')
        text(sx-3,bottom+22,clock(position)..' / '..clock(duration),10.5,C.muted)
        local function seek(x)
            if duration>0 then mp.commandv('seek',clamp((x-sx)/sw,0,1)*duration,'absolute+exact') end
        end
        region(sx-6,bottom+3,sw+12,20,seek,'Seek',true)
        if px>=sx and px<=sx+sw and py>=bottom+3 and py<=bottom+23 and duration>0 then
            local tx = clamp(px,sx+22,sx+sw-22)
            rect(tx-27,bottom-25,54,24,C.panel,0,5)
            text(tx,bottom-13,clock(clamp((px-sx)/sw,0,1)*duration),11,C.white,5)
        end
    end
    if panel then
        local rw,rh = 316,338
        local x = width-rw-18
        local y = panel=='Speed' and height-rh-66 or 58
        local list = items()
        selected = clamp(selected,1,math.max(1,#list))
        offset = clamp(offset,0,math.max(0,#list-PAGE_ROWS))
        rect(x-1,y-1,rw+2,rh+2,'514236',50,11)
        rect(x,y,rw,rh,C.panel,5,10)
        text(x+17,y+15,panel..(panel=='Queue' and '  ·  '..#list or ''),17,C.white)
        button(x+rw-40,y+7,30,30,'close',function() panel=nil; draw() end,'Close panel · Esc')
        line(x+12,y+48,x+rw-12,y+48,C.line,1)
        for row=1,PAGE_ROWS do
            local index,ry = offset+row,y+56+(row-1)*32
            local item = list[index]
            if item then
                button(x+8,ry,rw-16,30,nil,function() selected=index; item.action(); draw() end,nil,index==selected)
                if item.current then icon('check',x+17,ry+7,C.blue,15) end
                text(x+43,ry+7,title(item.label,30),13,item.current and C.blue or C.white)
            end
        end
        if #list==0 then
            text(x+rw/2,y+126,'Your queue is empty',16,C.white,8)
            text(x+rw/2,y+156,'Open a file to start watching.',12,C.muted,8)
            button(x+rw/2-25,y+172,50,36,'folder',open_files,'Open files')
        end
        if #list>PAGE_ROWS then
            button(x+rw-85,y+286,32,24,'up',function() page(-1) end,'Previous page')
            button(x+rw-47,y+286,32,24,'down',function() page(1) end,'Next page')
            text(x+16,y+291,string.format('%d–%d / %d',offset+1,math.min(offset+PAGE_ROWS,#list),#list),11,C.muted)
        end
        local hint = panel=='Subtitles' and 'J: cycle   V: visibility   Z/X: delay'
            or panel=='Settings' and 'Bars: Auto / Always / Hidden · H restores'
            or '↑ / ↓: select    Enter: apply    Esc: close'
        text(x+16,y+318,hint,10,C.muted)
    end
    local over = hit()
    if over and over.tip and not dragging then
        local tx = clamp(over.x+over.w/2,90,width-90)
        local ty = over.y<height/2 and over.y+over.h+9 or over.y-33
        local tw = math.max(58,#over.tip*6+18)
        rect(tx-tw/2,ty,tw,24,C.panel,0,5)
        text(tx,ty+12,over.tip,11,C.white,5)
    end
    overlay.res_x,overlay.res_y,overlay.data = width,height,ass.text
    overlay:update()
    if panel then mp.enable_key_bindings('jwm-panel-keys') else mp.disable_key_bindings('jwm-panel-keys') end
    mp.set_mouse_area(0,0,w,h,'jwm-panel-mouse')
    local hover = over~=nil or dragging~=nil
    if hover~=active then
        active=hover
        if hover then mp.enable_key_bindings('jwm-panel-mouse') else mp.disable_key_bindings('jwm-panel-mouse') end
    end
end
local function mouse_down()
    local r = hit()
    if r then
        local x=pointer()
        if r.drag then dragging=r; r.action(x) else r.action() end
    end
    draw()
end
local function mouse_up()
    if dragging then local x=pointer(); dragging.action(x); dragging=nil end
    draw()
end
-- The section API takes separate press/down/up callbacks (unlike the newer
-- add_key_binding API's complex flag). Keep capture local to our hit regions.
mp.set_key_bindings({{'MBTN_LEFT',function() mouse_down(); mouse_up() end,mouse_down,mouse_up}},
    'jwm-panel-mouse','force')
local function move(delta)
    selected=clamp(selected+delta,1,math.max(1,#items()))
    if selected<=offset then offset=selected-1 end
    if selected>offset+PAGE_ROWS then offset=selected-PAGE_ROWS end
    draw()
end
mp.set_key_bindings({
    {'UP',function() move(-1) end}, {'DOWN',function() move(1) end},
    {'PGUP',function() page(-1) end}, {'PGDWN',function() page(1) end},
    {'ENTER',function() local item=items()[selected]; if item then item.action(); draw() end end},
    {'ESC',function() panel=nil; draw() end},
},'jwm-panel-keys','force')
mp.add_key_binding('l','jwm-queue',function() toggle('Queue') end)
mp.add_key_binding('s','jwm-subtitles',function() toggle('Subtitles') end)
mp.add_key_binding('Ctrl+o','jwm-open',open_files)
mp.add_key_binding('h','jwm-toggle-bars',toggle_bars)
mp.add_key_binding('Ctrl+h','jwm-cycle-bars',cycle_bars)
mp.add_key_binding('mouse_move','jwm-pointer',function()
    pointer_present=true
    local x,y=pointer()
    local edges={top=x>=0 and x<=width and y>=0 and y<=72,
        bottom=x>=0 and x<=width and y>=height-84 and y<=height}
    for _,bar in ipairs({'top','bottom'}) do
        if edges[bar] or near_edge[bar] then revealed[bar]=mp.get_time() end
        near_edge[bar]=edges[bar]
    end
    if dragging then local x=pointer(); dragging.action(x) end
    draw()
end)
mp.add_key_binding('mouse_leave','jwm-pointer-leave',function()
    pointer_present=false; dragging=nil
    for _,bar in ipairs({'top','bottom'}) do near_edge[bar]=false; revealed[bar]=-math.huge end
    draw()
end)
for _,property in ipairs({'osd-dimensions','playlist','track-list','sid','hwdec','pause','fullscreen',
    'idle-active','duration','volume','mute','speed','ontop','sub-visibility','window-maximized'}) do
    mp.observe_property(property,'native',draw)
end
-- Decoder position updates can arrive at video frame rate. Ten UI updates per
-- second keep the timeline moving without rebuilding chrome for every frame.
local last_progress=-math.huge
mp.observe_property('time-pos','native',function()
    local now=mp.get_time()
    if now-last_progress>=.1 then last_progress=now; draw() end
end)
mp.add_periodic_timer(.25,function()
    if bar_visible('top')~=top_shown or bar_visible('bottom')~=bottom_shown then draw() end
end)
mp.register_script_message('jwm-queue',function() toggle('Queue') end)
mp.register_script_message('jwm-subtitles',function() toggle('Subtitles') end)
mp.register_event('shutdown',function()
    if chooser then mp.abort_async_command(chooser) end
    overlay:remove()
end)
draw()
