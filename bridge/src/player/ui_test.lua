-- Run with mpv --no-config --vo=null --ao=null --idle=yes --script=.../ui_test.lua
local real = require 'mp'
local utils = require 'mp.utils'
local bindings, messages, groups, observers = {}, {}, {}, {}
local mouse_x, mouse_y = 0, 0
local props = {playlist = {}, ['track-list'] = {}, sid = 'no', hwdec = 'auto-safe', fullscreen = false}
for i = 1, 300 do props.playlist[i] = {filename = '/music/Track ' .. i .. '.flac', current = i == 1} end
local overlay = {update=function() end, remove=function() end}
local mock = {
    create_osd_overlay=function() return overlay end,
    get_time=function() return 0 end,
    get_osd_size=function() return 960, 540 end,
    get_mouse_pos=function() return mouse_x, mouse_y end,
    get_property=function(name, default) local v=props[name]; if v == nil then return default end; return v end,
    get_property_native=function(name, default) local v=props[name]; if v == nil then return default end; return v end,
    set_property=function(name, value) props[name]=value; if observers[name] then observers[name]() end end,
    set_property_number=function(name, value) props[name]=value end,
    set_mouse_area=function() end,
    set_key_bindings=function(keys, group, flags) assert(flags == 'force'); groups[group]=keys end,
    enable_key_bindings=function(group, flags) assert(flags == nil); groups[group].enabled=true end,
    disable_key_bindings=function(group) if groups[group] then groups[group].enabled=false end end,
    add_key_binding=function(key, name, cb) bindings[key]=cb end,
    observe_property=function(name, kind, cb) observers[name]=cb end,
    add_periodic_timer=function() end,
    register_script_message=function(name, cb) messages[name]=cb end,
    register_event=function() end,
}
package.loaded.mp = mock
local function key(group, name)
    assert(groups[group].enabled, group .. ' should be enabled')
    for _, entry in ipairs(groups[group]) do if entry[1] == name then entry[2](); return end end
    error('unknown key ' .. name)
end
local function click(x, y)
    mouse_x, mouse_y = x, y
    bindings.mouse_move()
    key('jwm-panel-mouse', 'MBTN_LEFT')
end
local ok, err = pcall(function()
    -- Lua loaders may report either @/absolute/path or /absolute/path.
    local source = debug.getinfo(1, 'S').source:gsub('^@', '')
    local directory = utils.split_path(source)
    dofile(directory .. 'ui.lua')
    messages['jwm-queue']()
    assert(overlay.data:find('Track 1.flac', 1, true))
    assert(not overlay.data:find('Track 8.flac', 1, true), 'render must be bounded to one page')
    key('jwm-panel-keys', 'DOWN'); key('jwm-panel-keys', 'ENTER')
    assert(props['playlist-pos'] == 1)
    -- Paging keeps the selected relative row; Enter must never play a
    -- previously selected item that is now outside the visible page.
    click(720, 345)
    assert(overlay.data:find('Track 8.flac', 1, true))
    assert(not overlay.data:find('Track 1.flac', 1, true))
    key('jwm-panel-keys', 'ENTER')
    assert(props['playlist-pos'] == 8, 'next page must keep the second row selected')
    click(650, 345)
    key('jwm-panel-keys', 'ENTER')
    assert(props['playlist-pos'] == 1, 'previous page must restore the relative row')
    local full_playlist = props.playlist
    props.playlist = {}
    for i = 1, 10 do props.playlist[i] = full_playlist[i] end
    observers.playlist()
    click(720, 345)
    assert(overlay.data:find('Track 4.flac', 1, true))
    assert(overlay.data:find('Track 10.flac', 1, true))
    key('jwm-panel-keys', 'ENTER')
    assert(props['playlist-pos'] == 4, 'partial last page must preserve the relative row')
    click(720, 345)
    key('jwm-panel-keys', 'ENTER')
    assert(props['playlist-pos'] == 4, 'paging past the end must keep selection unchanged')
    click(650, 345)
    key('jwm-panel-keys', 'ENTER')
    assert(props['playlist-pos'] == 1)
    props.playlist = full_playlist
    observers.playlist()
    for _ = 1, 10 do key('jwm-panel-keys', 'DOWN') end
    key('jwm-panel-keys', 'ENTER'); assert(props['playlist-pos'] == 11)
    key('jwm-panel-keys', 'ESC'); assert(not groups['jwm-panel-keys'].enabled)
    click(640, 30); click(700, 115); assert(props['playlist-pos'] == 0)
    props['track-list'] = {{type='sub', id=7, title='中文 {\\an9}\n字幕'}}
    click(750, 30)
    assert(not overlay.data:find('{\\an9}', 1, true), 'ASS injection escaped')
    click(700, 145); assert(props.sid == 7)
    click(700, 115); assert(props.sid == 'no')
    click(880, 30); assert(props.hwdec == 'no')
    click(880, 30); assert(props.hwdec == 'auto-safe')
    mouse_x, mouse_y = 100, 500; bindings.mouse_move()
    assert(not groups['jwm-panel-mouse'].enabled, 'OSC must retain mouse outside panel')
    props.playlist = {}; messages['jwm-queue']()
    click(720, 345); click(650, 345) -- empty queue paging safe
    key('jwm-panel-keys', 'ENTER') -- empty queue safe
    props.playlist = {{filename='/x/歌曲😀.mp3'}}; observers.playlist()
    assert(overlay.data:find('歌曲😀.mp3', 1, true))
end)
package.loaded.mp = real
if not ok then real.msg.error(tostring(err)); real.commandv('quit', 1)
else real.msg.info('JWM UI logic tests passed'); real.commandv('quit', 0) end
