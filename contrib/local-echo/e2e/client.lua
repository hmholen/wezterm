local wezterm = require 'wezterm'
local act = wezterm.action
local E = os.getenv
local config = wezterm.config_builder()

config.unix_domains = {
  {
    name = 'lag',
    socket_path = E('TEST_SOCK'),
    no_serve_automatically = true,
    proxy_command = { 'python3', '-I', E('LAGPROXY'), E('LAG_MS'), E('TEST_SOCK') },
    local_echo_threshold_ms = 10,
  },
}
config.default_gui_startup_args = { 'connect', 'lag' }
config.check_for_updates = false
config.automatically_reload_config = false
config.front_end = 'Software'
config.window_close_confirmation = 'NeverPrompt'
config.initial_cols = 100
config.initial_rows = 20

local LOG = E('TEST_LOG')
local function log(s)
  local f = io.open(LOG, 'a')
  f:write(s .. '\n')
  f:close()
end
local function now() return tonumber(wezterm.time.now():format('%s%.3f')) end

local t0, phase, last = nil, 'init', nil
local function snap(pane)
  local c = pane:get_cursor_position()
  local dims = pane:get_dimensions()
  local text = pane:get_text_from_region(0, c.y, dims.cols, c.y)
  local rec = string.format('x=%d y=%d |%s|', c.x, c.y, text)
  if rec ~= last then
    last = rec
    log(string.format('%7.0f %s %s', (now() - t0) * 1000, phase, rec))
  end
end

local sampling = false
local function sample(pane)
  if not sampling then return end
  snap(pane)
  wezterm.time.call_after(0.01, function() sample(pane) end)
end

-- steps: { delay_seconds, function(window, pane) }
local function run(window, pane, steps, i)
  i = i or 1
  local step = steps[i]
  if not step then return end
  wezterm.time.call_after(step[1], function()
    step[2](window, pane)
    run(window, pane, steps, i + 1)
  end)
end

local function key(k, mods)
  return function(window, pane)
    log(string.format('%7.0f %s KEY %s', (now() - t0) * 1000, phase, k))
    window:perform_action(act.SendKey { key = k, mods = mods }, pane)
  end
end
local function set_phase(p) return function() phase = p end end

local GAP = tonumber(E('KEY_GAP') or '0.04')
local function type_str(steps, s)
  for ch in s:gmatch('.') do
    table.insert(steps, { GAP, key(ch == ' ' and 'Space' or ch) })
  end
end

local started = false
log('config loaded')
local function start()
  if started then return end
  local window = wezterm.gui.gui_windows()[1]
  if not window then log('start: no window yet'); return end
  started = true
  local pane = window:active_pane()
  t0 = now()
  local steps = {
    -- warm up: lets the client measure the round trip, then clear screen
    { 2.0, key('x') }, { 1.5, key('Backspace') }, { 1.5, key('l', 'CTRL') },
    { 2.0, function() sampling = true; sample(pane) end },
    { 0.3, set_phase('type') },
  }
  if E('SCENARIO') == 'shiftspace' then
    type_str(steps, 'lets')
    table.insert(steps, { GAP, key('Space', 'SHIFT') })
    type_str(steps, 'goo')
    table.insert(steps, { 3, set_phase('done') })
    table.insert(steps, { 0.3, key('u', 'CTRL') })
    table.insert(steps, { 1, function(w, p) log('END'); w:perform_action(act.QuitApplication, p) end })
    run(window, pane, steps)
    return
  end
  if E('SCENARIO') == 'ctrlb' then
    -- an unpredictable key (Ctrl+B: cursor left in zsh) while nothing is
    -- being predicted, then typing straight away
    type_str(steps, 'lets ')
    table.insert(steps, { 2.5, set_phase('ctrlb') })
    table.insert(steps, { GAP, key('b', 'CTRL') })
    type_str(steps, 'goooo')
    table.insert(steps, { 3, set_phase('done') })
    table.insert(steps, { 0.3, key('u', 'CTRL') })
    table.insert(steps, { 1, function(w, p) log('END'); w:perform_action(act.QuitApplication, p) end })
    run(window, pane, steps)
    return
  end
  if E('SCENARIO') == 'hold' then
    -- held-down Backspace that overshoots the start of the input, then
    -- typing straight away
    type_str(steps, 'is this working')
    table.insert(steps, { 2.5, set_phase('hold') })
    for _ = 1, 20 do table.insert(steps, { 0.03, key('Backspace') }) end
    type_str(steps, 'here we go')
    table.insert(steps, { 3, set_phase('done') })
    table.insert(steps, { 0.3, key('u', 'CTRL') })
    table.insert(steps, { 1, function(w, p) log('END'); w:perform_action(act.QuitApplication, p) end })
    run(window, pane, steps)
    return
  end
  if E('SCENARIO') == 'secret' then
    type_str(steps, "read -s '?Password: ' x")
    table.insert(steps, { GAP, key('Enter') })
    table.insert(steps, { 1.5, set_phase('secret') })
    type_str(steps, 'hunter2')
    table.insert(steps, { 2.0, key('Enter') })
    table.insert(steps, { 1.5, set_phase('done') })
    table.insert(steps, { 0.3, function(w, p) log('END'); w:perform_action(act.QuitApplication, p) end })
    run(window, pane, steps)
    return
  end
  if E('SCENARIO') == 'claude' then
    type_str(steps, 'claude')
    table.insert(steps, { GAP, key('Enter') })
    table.insert(steps, { 10, key('DownArrow') })     -- trust dialog: "Yes"
    table.insert(steps, { 0.5, key('Enter') })
    table.insert(steps, { 10, set_phase('cctype') })
    type_str(steps, 'hello there world')
    table.insert(steps, { 3, set_phase('ccerase') })
    for _ = 1, 6 do table.insert(steps, { GAP, key('Backspace') }) end
    table.insert(steps, { 3, set_phase('ccmid') })
    for _ = 1, 5 do table.insert(steps, { GAP, key('LeftArrow') }) end
    type_str(steps, 'XY')
    table.insert(steps, { 3, set_phase('done') })
    table.insert(steps, { 0.3, key('c', 'CTRL') })
    table.insert(steps, { 0.5, key('c', 'CTRL') })
    table.insert(steps, { 3, function(w, p) log('END'); w:perform_action(act.QuitApplication, p) end })
    run(window, pane, steps)
    return
  end
  type_str(steps, 'git status')
  table.insert(steps, { 2.5, set_phase('erase') })
  for _ = 1, 10 do table.insert(steps, { GAP, key('Backspace') }) end
  -- edit in the middle: "gt status" -> Left x8 -> "i"
  table.insert(steps, { 2.5, set_phase('mid') })
  type_str(steps, 'gt status')
  table.insert(steps, { 2.5, function() end })
  for _ = 1, 8 do table.insert(steps, { GAP, key('LeftArrow') }) end
  table.insert(steps, { GAP, key('i') })
  table.insert(steps, { 2.5, key('u', 'CTRL') })
  -- run a command, then type the next one before the output arrives
  table.insert(steps, { 1.5, set_phase('enter') })
  type_str(steps, 'echo hi')
  table.insert(steps, { GAP, key('Enter') })
  type_str(steps, 'ls')
  table.insert(steps, { 2.5, key('u', 'CTRL') })
  table.insert(steps, { 2.5, set_phase('done') })
  table.insert(steps, { 0.2, function(w, p) log('END'); w:perform_action(act.QuitApplication, p) end })
  run(window, pane, steps)
end

wezterm.on('gui-attached', function() log('gui-attached'); wezterm.time.call_after(4, start) end)
wezterm.on('update-status', function() if not started then wezterm.time.call_after(4, start) end end)
wezterm.on('window-focus-changed', function() log('focus-changed') end)

return config
