-- JWM's companion panel; mpv's OSC remains responsible for seek/volume/transport.
-- No subprocesses, shell commands, filesystem discovery or network requests.
local mp = require 'mp'
local assdraw = require 'mp.assdraw'
local utils = require 'mp.utils'
local overlay = mp.create_osd_overlay('ass-events')
local PAGE_ROWS = 7
local panel, offset, selected = nil, 0, 1
local regions, width, height = {}, 960, 540
local last_motion = mp.get_time()
local active = false
local draw
local function safe(text)
    -- Media metadata is untrusted ASS text (including backslashes and newlines).
    return tostring(text or ''):gsub('[%c]', ' '):gsub('\\', '＼'):gsub('{', '｛'):gsub('}', '｝')
end
local function title(text, limit)
    text = safe(text)
    -- utf8 is not available in every mpv Lua runtime; truncate on codepoints.
    local out, n = {}, 0
    for char in text:gmatch('[%z\1-\127\194-\244][\128-\191]*') do
        n = n + 1
        if n > limit then return table.concat(out) .. '…' end
        out[#out + 1] = char
    end
    return table.concat(out)
end
local function items()
    if panel == 'Queue' then
        local list = {}
        for i, entry in ipairs(mp.get_property_native('playlist', {})) do
            local _, name = utils.split_path(entry.filename or '')
            list[#list + 1] = {label = entry.title or name, current = entry.current,
                action = function() mp.set_property_number('playlist-pos', i - 1) end}
        end
        return list
    elseif panel == 'Subtitles' then
        local list = {{label = 'Off', current = mp.get_property('sid') == 'no',
            action = function() mp.set_property('sid', 'no') end}}
        for _, track in ipairs(mp.get_property_native('track-list', {})) do
            if track.type == 'sub' then
                local id = track.id
                list[#list + 1] = {label = track.title or track.lang or ('Subtitle ' .. id),
                    current = track.selected,
                    action = function() mp.set_property_number('sid', id) end}
            end
        end
        return list
    end
    return {}
end
local function toggle(name)
    panel = panel ~= name and name or nil
    offset, selected = 0, 1
    draw()
end
local function page(direction)
    local count = #items()
    local previous = offset
    offset = math.max(0, math.min(math.max(0, count - PAGE_ROWS), offset + direction * PAGE_ROWS))
    -- Keep the selected row in the same position on the visible page,
    -- including when the last page scrolls by fewer than PAGE_ROWS items.
    selected = math.max(1, math.min(count, selected + offset - previous))
    draw()
end
local function pointer()
    local x, y = mp.get_mouse_pos()
    local w, h = mp.get_osd_size()
    if not w or w <= 0 or not h or h <= 0 then return -1, -1 end
    return x * width / w, y * height / h
end
local function hit()
    local x, y = pointer()
    for _, r in ipairs(regions) do
        if x >= r.x and x <= r.x + r.w and y >= r.y and y <= r.y + r.h then return r end
    end
end
local function keyboard(enable)
    if enable then mp.enable_key_bindings('jwm-panel-keys')
    else mp.disable_key_bindings('jwm-panel-keys') end
end
draw = function()
    local w, h = mp.get_osd_size()
    if not w or w <= 0 or not h or h <= 0 then return end
    height = 540
    width = math.max(360, height * w / h)
    regions = {}
    local visible = panel or not mp.get_property_native('fullscreen', false)
        or mp.get_property_native('pause', false) or mp.get_time() - last_motion < 3
    local ass = assdraw.ass_new()
    local function rect(x, y, rw, rh, color)
        ass:new_event(); ass:append('{\\bord0\\shad0\\1c&H' .. color .. '&\\alpha&H18&}')
        ass:draw_start(); ass:rect_cw(x, y, x + rw, y + rh); ass:draw_stop()
    end
    local function text(x, y, value, size, color)
        ass:new_event(); ass:pos(x, y)
        ass:append('{\\an7\\fnsans-serif\\fs' .. size .. '\\bord0\\shad0\\1c&H' .. color .. '&}' .. safe(value))
    end
    local function button(x, y, rw, rh, label, callback, chosen)
        rect(x, y, rw, rh, chosen and '725435' or '292521')
        text(x + 10, y + 7, label, 16, chosen and 'FFFFFF' or 'EBE6DF')
        regions[#regions + 1] = {x=x, y=y, w=rw, h=rh, action=callback}
    end
    if visible then
        local x = width - 346
        button(x, 14, 100, 36, 'Queue', function() toggle('Queue') end, panel == 'Queue')
        button(x + 106, 14, 110, 36, 'Subtitles', function() toggle('Subtitles') end, panel == 'Subtitles')
        local hw = mp.get_property('hwdec', 'no') ~= 'no'
        button(x + 222, 14, 110, 36, hw and 'HW: Auto' or 'HW: Off', function()
            mp.set_property('hwdec', hw and 'no' or 'auto-safe')
        end, hw)
    end
    if panel then
        local x, y, rw = width - 346, 60, 332
        local list = items()
        local rows = PAGE_ROWS
        selected = math.max(1, math.min(selected, #list))
        offset = math.max(0, math.min(offset, math.max(0, #list - rows)))
        rect(x, y, rw, 326, '201D19')
        text(x + 12, y + 10, panel .. '  ·  ' .. #list, 18, 'FFFFFF')
        button(x + rw - 42, y + 2, 38, 32, '×', function() panel = nil; draw() end)
        for row = 1, rows do
            local index = offset + row
            local item = list[index]
            if item then
                button(x + 8, y + 40 + (row - 1) * 32, rw - 16, 30,
                    (item.current and '• ' or '  ') .. title(item.label, 30),
                    function() selected = index; item.action(); draw() end, index == selected)
            end
        end
        if #list == 0 then text(x + 16, y + 64, 'Open files from JWM Music / Videos', 14, 'BDB5AC') end
        button(x + 8, y + 270, 72, 30, '↑', function() page(-1) end)
        button(x + 86, y + 270, 72, 30, '↓', function() page(1) end)
        text(x + 12, y + 306, panel == 'Subtitles' and 'J: cycle   V: visibility   Z/X: delay' or 'Enter: play   Esc: close   L: queue', 12, 'BDB5AC')
    end
    overlay.res_x, overlay.res_y, overlay.data = width, height, ass.text
    overlay:update()
    keyboard(panel ~= nil)
    mp.set_mouse_area(0, 0, w, h, 'jwm-panel-mouse')
    local hover = hit() ~= nil
    if hover ~= active then
        active = hover
        if hover then mp.enable_key_bindings('jwm-panel-mouse')
        else mp.disable_key_bindings('jwm-panel-mouse') end
    end
end
mp.set_key_bindings({{'MBTN_LEFT', function() local r = hit(); if r then r.action() end end}}, 'jwm-panel-mouse', 'force')
local function move(delta)
    selected = math.max(1, math.min(#items(), selected + delta))
    if selected <= offset then offset = selected - 1 end
    if selected > offset + PAGE_ROWS then offset = selected - PAGE_ROWS end
    draw()
end
mp.set_key_bindings({
    {'UP', function() move(-1) end}, {'DOWN', function() move(1) end},
    {'ENTER', function() local item = items()[selected]; if item then item.action() end end},
    {'ESC', function() panel = nil; draw() end},
}, 'jwm-panel-keys', 'force')
mp.add_key_binding('l', 'jwm-queue', function() toggle('Queue') end)
mp.add_key_binding('s', 'jwm-subtitles', function() toggle('Subtitles') end)
mp.add_key_binding('mouse_move', 'jwm-pointer', function() last_motion = mp.get_time(); draw() end)
for _, property in ipairs({'osd-dimensions', 'playlist', 'track-list', 'sid', 'hwdec', 'pause', 'fullscreen'}) do
    mp.observe_property(property, 'native', draw)
end
mp.add_periodic_timer(0.5, function()
    if mp.get_property_native('fullscreen', false) and not panel then draw() end
end)
mp.register_script_message('jwm-queue', function() toggle('Queue') end)
mp.register_script_message('jwm-subtitles', function() toggle('Subtitles') end)
mp.register_event('shutdown', function() overlay:remove() end)
draw()
