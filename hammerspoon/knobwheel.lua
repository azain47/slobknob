-- KnobWheel: radial mode picker for a keyboard volume knob (built for the AULA F75).
--
-- The knob sends media keys: turn = SOUND_UP / SOUND_DOWN, press = MUTE.
--   tap knob            -> current mode's press action (mute in Volume mode)
--   double-tap knob     -> wheel opens; turn to highlight, it picks 0.6s after the last turn
--                          (or tap to pick right away)
--   Cmd + turn          -> wheel opens; keep turning, let go of Cmd to pick
--   (trigger = "hold")  -> hold to open; turn while held and release, or release, turn, tap
--   Esc or 5s idle      -> close the wheel without picking
--   turn knob           -> current mode's action
--
-- F75 note: holding the knob ~3s flips it into lighting mode, so release before that.

local M = {}

-- load extensions now; lazy loading logs to the console from inside the event tap
local _ = hs.canvas, hs.mouse, hs.hotkey, hs.menubar, hs.settings, hs.timer, hs.audiodevice,
  hs.spotify, hs.osascript, hs.application, hs.screen

M.config = {
  trigger         = "doubletap", -- "doubletap" or "hold"
  doubleTapSeconds = 0.28, -- max gap between the two taps (single taps wait this long)
  holdSeconds     = 0.35, -- trigger = "hold": hold this long to open the wheel
  wheelTimeout    = 5,    -- close an untouched wheel after this many seconds
  autoConfirmSeconds = 0.6, -- after a turn in the wheel, pick the highlight this long later (0 = tap only)
  modifierTrigger = true, -- hold Cmd and turn to open the wheel; let go of Cmd to pick
  revertAfter     = 0,    -- seconds idle before falling back to Volume (0 = never)
  scrollLines     = 3,    -- lines per knob click in Scroll mode
  seekSeconds     = 5,    -- seconds per knob click in Seek mode
  appSwitchCommit = 0.8,  -- Apps mode: let go of Cmd this long after the last turn
  -- order of slices, clockwise from the top
  dragPixels      = 4,    -- pointer travel per knob click in Drag mode
  debugLog        = false, -- append every knob event to ~/.hammerspoon/knobwheel.log
  dimScreen       = true, -- darken the screen behind the wheel
  pastel          = true, -- a soft color per mode instead of the macOS accent color
  wheel = { "volume", "brightness", "scroll", "zoom", "apps", "tabs", "spaces", "media", "seek" },
  -- also available: "windows", "undo", "drag", "jog", "edits"

  -- Per-app wheels, keyed by app name. While that app is frontmost, holding the knob
  -- opens its wheel instead, and the app keeps its own selected mode.
  appWheels = {
    ["DaVinci Resolve"] = { "volume", "jog", "edits", "zoom", "drag", "scroll", "apps", "media" },
  },
}

local cfg = M.config
local ev = hs.eventtap.event

local function sysKey(name)
  ev.newSystemKeyEvent(name, true):post()
  ev.newSystemKeyEvent(name, false):post()
end

local function stroke(mods, key)
  hs.eventtap.keyStroke(mods, key, 5000)
end

-- Mute we re-post ourselves must get past our own tap.
local synthMute = 0
local function postMute()
  synthMute = 2
  sysKey("MUTE")
  hs.timer.doAfter(0.3, function() synthMute = 0 end)
end

-- Apps mode holds Cmd down across several turns so the native Cmd-Tab switcher stays up.
local cmdHeld, cmdTimer = false, nil
local function releaseCmd()
  if cmdTimer then cmdTimer:stop(); cmdTimer = nil end
  if cmdHeld then
    ev.newKeyEvent("cmd", false):post()
    cmdHeld = false
  end
end

local function cmdTabStep(dir, key)
  if not cmdHeld then
    ev.newKeyEvent("cmd", true):post()
    cmdHeld = true
  end
  local mods = dir > 0 and { "cmd" } or { "cmd", "shift" }
  ev.newKeyEvent(mods, key, true):post()
  ev.newKeyEvent(mods, key, false):post()
  if cmdTimer then cmdTimer:stop() end
  cmdTimer = hs.timer.doAfter(cfg.appSwitchCommit, releaseCmd)
end

-- Seek: a playing Spotify / Music wins, otherwise arrow keys (YouTube, most video players).
local function appleScriptNumber(script)
  local ok, result = hs.osascript.applescript(script)
  return ok and tonumber(result) or nil
end

local function seekBy(sec)
  if hs.application.get("Spotify") and hs.spotify.isPlaying() then
    hs.spotify.setPosition(math.max(0, hs.spotify.getPosition() + sec))
    return
  end
  if hs.application.get("Music") then
    local pos = appleScriptNumber([[tell application "Music"
      if player state is playing then return player position
      return -1
    end tell]])
    if pos and pos >= 0 then
      hs.osascript.applescript(string.format(
        'tell application "Music" to set player position to %f', math.max(0, pos + sec)))
      return
    end
  end
  stroke({}, sec > 0 and "right" or "left")
end

-- Drag: hold the left button at the pointer and slide it, so the knob scrubs whatever
-- value field or slider is under the mouse (Resolve wheels and number boxes, etc).
local drag = { active = false, axis = "horizontal", origin = nil, pos = nil, timer = nil }

local function endDrag()
  if drag.timer then drag.timer:stop(); drag.timer = nil end
  if not drag.active then return end
  ev.newMouseEvent(ev.types.leftMouseUp, drag.pos):post()
  hs.mouse.absolutePosition(drag.origin)
  drag.active = false
end

local function dragStep(dir)
  if not drag.active then
    drag.origin = hs.mouse.absolutePosition()
    drag.pos = { x = drag.origin.x, y = drag.origin.y }
    ev.newMouseEvent(ev.types.leftMouseDown, drag.pos):post()
    drag.active = true
  end
  local d = dir * cfg.dragPixels
  if drag.axis == "horizontal" then drag.pos.x = drag.pos.x + d else drag.pos.y = drag.pos.y - d end
  ev.newMouseEvent(ev.types.leftMouseDragged, drag.pos):post()
  if drag.timer then drag.timer:stop() end
  drag.timer = hs.timer.doAfter(0.35, endDrag)
end

local scrollAxis = "vertical"
local showHud -- forward declaration

-- rotate(dir): dir is +1 clockwise, -1 counter-clockwise. Return true to let the
-- original key through to macOS untouched.
M.modes = {
  volume = {
    name = "Volume", icon = "🔊", hint = "turn: volume\ntap: mute",
    rotate = function() return true end,
    press = postMute,
  },
  brightness = {
    name = "Brightness", icon = "☀️", hint = "turn: screen brightness",
    rotate = function(dir) sysKey(dir > 0 and "BRIGHTNESS_UP" or "BRIGHTNESS_DOWN") end,
  },
  scroll = {
    name = "Scroll", icon = "🖱️", hint = "turn: scroll\ntap: switch axis",
    rotate = function(dir)
      local n = cfg.scrollLines * dir
      local delta = scrollAxis == "vertical" and { 0, -n } or { -n, 0 }
      ev.newScrollEvent(delta, {}, "line"):post()
    end,
    press = function()
      scrollAxis = scrollAxis == "vertical" and "horizontal" or "vertical"
      showHud("scroll", "Scroll: " .. scrollAxis)
    end,
  },
  zoom = {
    name = "Zoom", icon = "🔍", hint = "turn: zoom in / out\ntap: reset",
    rotate = function(dir) stroke({ "cmd" }, dir > 0 and "=" or "-") end,
    press = function() stroke({ "cmd" }, "0") end,
  },
  apps = {
    name = "Apps", icon = "🗂️", hint = "turn: switch app\ntap: pick",
    rotate = function(dir) cmdTabStep(dir, "tab") end,
    press = releaseCmd,
  },
  windows = {
    name = "Windows", icon = "🪟", hint = "turn: cycle app windows",
    rotate = function(dir) stroke(dir > 0 and { "cmd" } or { "cmd", "shift" }, "`") end,
  },
  tabs = {
    name = "Tabs", icon = "📑", hint = "turn: switch tab",
    rotate = function(dir) stroke({ "cmd", "shift" }, dir > 0 and "]" or "[") end,
  },
  spaces = {
    name = "Spaces", icon = "🖥️", hint = "turn: switch desktop\ntap: Mission Control",
    rotate = function(dir) stroke({ "ctrl" }, dir > 0 and "right" or "left") end,
    press = function() stroke({ "ctrl" }, "up") end, -- Mission Control
  },
  media = {
    name = "Media", icon = "🎵", hint = "turn: skip track\ntap: play / pause",
    rotate = function(dir) sysKey(dir > 0 and "NEXT" or "PREVIOUS") end,
    press = function() sysKey("PLAY") end,
  },
  seek = {
    name = "Seek", icon = "⏩", hint = "turn: seek\ntap: play / pause",
    rotate = function(dir) seekBy(dir * cfg.seekSeconds) end,
    press = function() sysKey("PLAY") end,
  },
  drag = {
    name = "Drag", icon = "🎚️", hint = "turn: drag value under mouse\ntap: switch axis",
    rotate = dragStep,
    press = function()
      endDrag()
      drag.axis = drag.axis == "horizontal" and "vertical" or "horizontal"
      showHud("drag", "Drag: " .. drag.axis)
    end,
  },
  jog = {
    name = "Jog", icon = "🎞️", hint = "turn: step frames\ntap: play / pause", -- frame step
    rotate = function(dir) stroke({}, dir > 0 and "right" or "left") end,
    press = function() stroke({}, "space") end,
  },
  edits = {
    name = "Edits", icon = "✂️", hint = "turn: jump edit points\ntap: play / pause", -- previous / next edit point
    rotate = function(dir) stroke({}, dir > 0 and "down" or "up") end,
    press = function() stroke({}, "space") end,
  },
  undo = {
    name = "Undo", icon = "↩️", hint = "turn: undo / redo",
    rotate = function(dir)
      if dir > 0 then stroke({ "cmd", "shift" }, "z") else stroke({ "cmd" }, "z") end
    end,
  },
}

-- Each wheel (the default one, plus one per app in cfg.appWheels) remembers its own mode.
local modeByWheel = hs.settings.get("knobwheel.modes") or {}

local function wheelKey()
  local app = hs.application.frontmostApplication()
  local name = app and app:name()
  if name and cfg.appWheels[name] then return name end
  return "default"
end

local function wheelList(key)
  return key == "default" and cfg.wheel or cfg.appWheels[key]
end

local function currentMode(key)
  key = key or wheelKey()
  local id = modeByWheel[key]
  if M.modes[id] then return id end
  return wheelList(key)[1]
end

---------------------------------------------------------------------------
-- Drawing
---------------------------------------------------------------------------

-- White SF Symbol PNGs made by knobwheel-icons/make-icons.swift; emoji is the fallback.
local iconDir = os.getenv("HOME") .. "/.hammerspoon/knobwheel-icons/"
local iconCache = {}
local function iconImage(id)
  if iconCache[id] == nil then
    iconCache[id] = hs.image.imageFromPath(iconDir .. id .. ".png") or false
  end
  return iconCache[id] or nil
end

local function accentColor()
  local ok, c = pcall(function()
    return hs.drawing.color.asRGB(hs.drawing.color.colorsFor("System").controlAccentColor)
  end)
  if ok and c and c.red then return { red = c.red, green = c.green, blue = c.blue } end
  return { red = 1, green = 0.58, blue = 0.18 }
end

local function withAlpha(c, a) return { red = c.red, green = c.green, blue = c.blue, alpha = a } end

local accent = accentColor()

-- A soft color per mode for the selected slice and the HUD (set pastel = false for the OS accent).
local function hex(h)
  return { red = tonumber(h:sub(2, 3), 16) / 255, green = tonumber(h:sub(4, 5), 16) / 255,
           blue = tonumber(h:sub(6, 7), 16) / 255 }
end
local pastel = {
  volume = hex("#f5adb9"), brightness = hex("#f5e3ad"), scroll = hex("#adf5dd"), zoom = hex("#b9adf5"),
  apps = hex("#f5cbad"), tabs = hex("#ade9f5"), spaces = hex("#ddadf5"), media = hex("#f5addd"),
  seek = hex("#c5f5ad"), windows = hex("#adcbf5"), drag = hex("#adf5b9"), jog = hex("#f5b7ad"),
  edits = hex("#f5adf5"), undo = hex("#f3f5ad"),
}

local function colorFor(id)
  if cfg.pastel then return pastel[id] or hex("#e2d6f5") end
  return accent
end

-- dark icon on pastel, white on the OS accent
local function iconOnColor(id)
  return cfg.pastel and (iconImage(id .. "-dark") or iconImage(id)) or iconImage(id)
end
local theme = {
  ring     = { white = 0.07, alpha = 0.9 },
  slice    = { white = 1, alpha = 0.055 },
  edge     = { white = 1, alpha = 0.09 },
  hub      = { white = 0.035, alpha = 0.94 },
  text     = { white = 1, alpha = 0.96 },
  muted    = { white = 1, alpha = 0.5 },
  shadow   = { white = 0, alpha = 0.55 },
  onColor  = { white = 0.08, alpha = 1 },
}

-- bold = semibold system font; plain = medium
local function styled(str, size, color, bold)
  local font = { name = bold and ".AppleSystemUIFontDemi" or ".AppleSystemUIFontMedium", size = size }
  return hs.styledtext.new(str, { font = font, color = color,
                                  paragraphStyle = { alignment = "center" } })
end

local SIZE, R_OUT, R_IN, R_ICON, POP = 540, 228, 114, 168, 9
local C = SIZE / 2
local FOOTER = 44

-- angle in degrees, 0 = top, clockwise
local function polar(r, deg)
  local a = math.rad(deg)
  return { x = C + r * math.sin(a), y = C - r * math.cos(a) }
end

local function wedge(a1, a2, rIn, rOut)
  local pts, steps = {}, 24
  for s = 0, steps do pts[#pts + 1] = polar(rOut, a1 + (a2 - a1) * s / steps) end
  for s = steps, 0, -1 do pts[#pts + 1] = polar(rIn, a1 + (a2 - a1) * s / steps) end
  return pts
end

local wheel = { canvas = nil, dim = nil, sel = 1, sticky = false, timer = nil,
                key = "default", list = cfg.wheel }

local function currentScreenFrame()
  return (hs.mouse.getCurrentScreen() or hs.screen.mainScreen()):frame()
end

local function sliceAngles(i)
  local span = 360 / #wheel.list
  local mid = (i - 1) * span
  return mid - span / 2 + 0.9, mid + span / 2 - 0.9, mid
end

local function buildDim(f)
  local d = hs.canvas.new(f)
  d:level(hs.canvas.windowLevels.overlay)
  d:behavior({ "canJoinAllSpaces", "fullScreenAuxiliary", "transient" })
  d:appendElements({ type = "rectangle", action = "fill", fillGradient = "radial",
    fillGradientColors = { { white = 0, alpha = 0.12 }, { white = 0, alpha = 0.5 } } })
  return d
end

local function buildWheel(f)
  local c = hs.canvas.new({ x = f.x + (f.w - SIZE) / 2, y = f.y + (f.h - SIZE - FOOTER) / 2,
                            w = SIZE, h = SIZE + FOOTER })
  c:level(hs.canvas.windowLevels.overlay)
  c:behavior({ "canJoinAllSpaces", "fullScreenAuxiliary", "transient" })

  c:appendElements({ type = "circle", center = { x = C, y = C }, radius = R_OUT + 8,
    action = "strokeAndFill", fillColor = theme.ring, strokeColor = theme.edge, strokeWidth = 1,
    withShadow = true, shadow = { blurRadius = 40, color = theme.shadow, offset = { h = -10, w = 0 } } })

  local active = currentMode(wheel.key)
  for i, id in ipairs(wheel.list) do
    local m = M.modes[id]
    local a1, a2, mid = sliceAngles(i)
    local p = polar(R_ICON, mid)
    c:appendElements({ id = "w" .. i, type = "segments", closed = true, action = "fill",
                       coordinates = wedge(a1, a2, R_IN + 4, R_OUT), fillColor = theme.slice })
    local img = iconImage(id)
    if img then
      c:appendElements({ id = "i" .. i, type = "image", image = img, imageAlpha = 0.72,
                         frame = { x = p.x - 17, y = p.y - 24, w = 34, h = 34 } })
    else
      c:appendElements({ id = "i" .. i, type = "text", text = styled(m.icon, 24, theme.text),
                         frame = { x = p.x - 20, y = p.y - 26, w = 40, h = 34 } })
    end
    c:appendElements({ id = "t" .. i, type = "text", text = styled(m.name, 12, theme.muted),
                       frame = { x = p.x - 50, y = p.y + 14, w = 100, h = 18 } })
    if id == active then
      -- little dot on the rim marks the mode you are in now
      local d = polar(R_OUT - 11, mid)
      c:appendElements({ type = "circle", center = d, radius = 3, action = "fill",
                         fillColor = { white = 1, alpha = 0.85 } })
    end
  end

  c:appendElements(
    { type = "circle", center = { x = C, y = C }, radius = R_IN - 8, action = "strokeAndFill",
      fillColor = theme.hub, strokeColor = theme.edge, strokeWidth = 1 },
    { id = "hubIcon", type = "image", image = iconImage(wheel.list[1]) or hs.image.imageFromName("NSStatusNone"),
      frame = { x = C - 22, y = C - 66, w = 44, h = 44 } },
    { id = "hubTitle", type = "text", text = "",
      frame = { x = C - 90, y = C - 16, w = 180, h = 28 } },
    { id = "hubHint", type = "text", text = "",
      frame = { x = C - 90, y = C + 16, w = 180, h = 40 } },
    { type = "text", frame = { x = 0, y = SIZE + 8, w = SIZE, h = 22 },
      text = styled((wheel.via == "modifier" and "turn to choose   ·   release ⌘ to pick   ·   esc to cancel")
        or (cfg.autoConfirmSeconds > 0
          and string.format("turn to choose   ·   picks after %gs   ·   tap to pick now   ·   esc to cancel",
                            cfg.autoConfirmSeconds))
        or "turn to choose   ·   tap to select   ·   esc to cancel", 12, theme.muted) }
  )
  return c
end

local function paintWheel()
  local c = wheel.canvas
  if not c then return end
  for i = 1, #wheel.list do
    local a1, a2 = sliceAngles(i)
    local on = i == wheel.sel
    local w = c["w" .. i]
    w.coordinates = on and wedge(a1, a2, R_IN + 4, R_OUT + POP) or wedge(a1, a2, R_IN + 4, R_OUT)
    local id = wheel.list[i]
    local col = colorFor(id)
    w.fillColor = on and withAlpha(col, 0.95) or theme.slice
    w.withShadow = on
    if on then w.shadow = { blurRadius = 22, color = withAlpha(col, 0.55), offset = { h = 0, w = 0 } } end
    if c["i" .. i].type == "image" then
      c["i" .. i].image = on and iconOnColor(id) or iconImage(id)
      c["i" .. i].imageAlpha = on and 1 or 0.72
    end
    local labelColor = on and (cfg.pastel and theme.onColor or theme.text) or theme.muted
    c["t" .. i].text = styled(M.modes[id].name, 12, labelColor, on)
  end
  local id = wheel.list[wheel.sel]
  local m = M.modes[id]
  local img = iconImage(id)
  if img then c.hubIcon.image = img end
  c.hubTitle.text = styled(m.name, 20, theme.text, true)
  c.hubHint.text = styled(m.hint or "", 11, theme.muted)
end

local hud
showHud = function(id, text)
  if hud then hud:delete() end
  local f = currentScreenFrame()
  local title = styled(text, 16, theme.text, true)
  local tw = hs.drawing.getTextDrawingSize(title).w
  local w, h = math.ceil(tw) + 76, 52
  hud = hs.canvas.new({ x = f.x + (f.w - w) / 2 - 20, y = f.y + f.h - h - 124, w = w + 40, h = h + 40 })
  hud:level(hs.canvas.windowLevels.overlay)
  hud:behavior({ "canJoinAllSpaces", "fullScreenAuxiliary", "transient" })
  -- 20px margin around the pill so its shadow is not clipped
  hud:appendElements(
    { type = "rectangle", action = "strokeAndFill", frame = { x = 20, y = 14, w = w, h = h },
      roundedRectRadii = { xRadius = h / 2, yRadius = h / 2 },
      fillColor = { white = 0.06, alpha = 0.9 }, strokeColor = theme.edge, strokeWidth = 1,
      withShadow = true, shadow = { blurRadius = 24, color = theme.shadow, offset = { h = -6, w = 0 } } },
    { type = "circle", center = { x = 20 + h / 2, y = 14 + h / 2 }, radius = 17,
      action = "fill", fillColor = withAlpha(colorFor(id), 0.95) }
  )
  local img = iconOnColor(id)
  if img then
    hud:appendElements({ type = "image", image = img,
                         frame = { x = 20 + h / 2 - 10, y = 14 + h / 2 - 10, w = 20, h = 20 } })
  end
  hud:appendElements({ type = "text", text = title,
                       frame = { x = 20 + h - 4, y = 14 + (h - 21) / 2, w = tw + 20, h = 24 } })
  hud:show(0.12)
  local mine = hud
  hs.timer.doAfter(1.0, function()
    if hud == mine then hud:delete(0.3); hud = nil end
  end)
end

---------------------------------------------------------------------------
-- Mode + wheel state
---------------------------------------------------------------------------

local menubar
local revertTimer

local function menuIcon(id)
  local img = iconImage(id)
  return img and img:copy():setSize({ w = 16, h = 16 }):template(true)
end

local function updateMenubar()
  if not menubar then return end
  local id = currentMode()
  local img = menuIcon(id)
  if img then menubar:setIcon(img, true); menubar:setTitle(nil)
  else menubar:setTitle(M.modes[id].icon) end
end

local function setMode(id, key)
  releaseCmd()
  endDrag()
  modeByWheel[key or wheelKey()] = id
  hs.settings.set("knobwheel.modes", modeByWheel)
  updateMenubar()
  showHud(id, M.modes[id].name)
end

local escKey

-- Auto-confirm: picks the highlighted slice a moment after the last turn.
local confirmTimer

local function stopConfirm()
  if confirmTimer then confirmTimer:stop(); confirmTimer = nil end
end

-- Watches Cmd only while a Cmd-opened wheel is up, so typing never goes through it.
local modTap

local function closeWheel()
  stopConfirm()
  if modTap then modTap:stop(); modTap = nil end
  if wheel.timer then wheel.timer:stop(); wheel.timer = nil end
  if wheel.canvas then wheel.canvas:delete(0.15); wheel.canvas = nil end
  if wheel.dim then wheel.dim:delete(0.2); wheel.dim = nil end
  if escKey then escKey:disable() end
end

local function commitWheel()
  local id = wheel.list[wheel.sel]
  closeWheel()
  setMode(id, wheel.key)
end

local function armConfirm()
  stopConfirm()
  local secs = cfg.autoConfirmSeconds
  if secs <= 0 or not wheel.canvas then return end
  confirmTimer = hs.timer.doAfter(secs, function()
    confirmTimer = nil
    commitWheel()
  end)
end

local function touchWheel()
  if wheel.timer then wheel.timer:stop() end
  wheel.timer = hs.timer.doAfter(cfg.wheelTimeout, closeWheel)
end

local function openWheel(sticky, via)
  releaseCmd()
  endDrag()
  wheel.key = wheelKey()
  wheel.list = wheelList(wheel.key)
  wheel.sel = 1
  local active = currentMode(wheel.key)
  for i, id in ipairs(wheel.list) do if id == active then wheel.sel = i end end
  wheel.startSel = wheel.sel
  wheel.sticky = sticky == true
  wheel.via = via or "tap"
  if wheel.via == "modifier" then
    modTap = hs.eventtap.new({ ev.types.flagsChanged }, function(e)
      if not e:getFlags().cmd then
        -- let go of Cmd: pick, or just close if the highlight never moved
        hs.timer.doAfter(0, function()
          if not wheel.canvas then return end
          if wheel.sel ~= wheel.startSel then commitWheel() else closeWheel() end
        end)
      end
      return false
    end):start()
  end
  local f = currentScreenFrame()
  if cfg.dimScreen then
    wheel.dim = buildDim(f)
    wheel.dim:show(0.15)
  end
  wheel.canvas = buildWheel(f)
  paintWheel()
  wheel.canvas:show(0.1)
  escKey = escKey or hs.hotkey.new({}, "escape", closeWheel)
  escKey:enable()
  touchWheel()
end

local function moveWheel(dir)
  local n = #wheel.list
  wheel.sel = (wheel.sel - 1 + dir) % n + 1
  paintWheel()
  touchWheel()
  if wheel.via ~= "modifier" then armConfirm() end
end

local function armRevert()
  if revertTimer then revertTimer:stop(); revertTimer = nil end
  local key = wheelKey()
  if cfg.revertAfter > 0 and currentMode(key) ~= "volume" then
    revertTimer = hs.timer.doAfter(cfg.revertAfter, function() setMode("volume", key) end)
  end
end

---------------------------------------------------------------------------
-- Knob input
---------------------------------------------------------------------------

local pressing, rotatedWhileHeld, holdTimer, tapTimer, swallowNextUp = false, false, nil, nil, false

local function runPress()
  local press = M.modes[currentMode()].press
  if press then press() end
end

local function onPressDown()
  if wheel.canvas and wheel.sticky then
    commitWheel()
    swallowNextUp = true
    return
  end
  if cfg.trigger == "doubletap" then
    if tapTimer then
      -- second tap: open the wheel instead of running the single-tap action
      tapTimer:stop(); tapTimer = nil
      openWheel(true)
      swallowNextUp = true
    end
    return
  end
  pressing, rotatedWhileHeld = true, false
  if holdTimer then holdTimer:stop() end
  holdTimer = hs.timer.doAfter(cfg.holdSeconds, function()
    holdTimer = nil
    openWheel()
  end)
end

local function onPressUp()
  pressing = false
  if swallowNextUp then swallowNextUp = false; return end
  if cfg.trigger == "doubletap" then
    tapTimer = hs.timer.doAfter(cfg.doubleTapSeconds, function()
      tapTimer = nil
      runPress()
    end)
    return
  end
  if holdTimer then
    -- short tap
    holdTimer:stop(); holdTimer = nil
    runPress()
    return
  end
  if wheel.canvas then
    if rotatedWhileHeld then
      commitWheel()
    else
      wheel.sticky = true
      paintWheel()
      touchWheel()
    end
  end
end

local function onRotate(dir)
  if wheel.canvas then
    if pressing then rotatedWhileHeld = true end
    moveWheel(dir)
    return false
  end
  -- Cmd + turn opens the wheel on the current mode (not Apps mode's own Cmd)
  if cfg.modifierTrigger and not cmdHeld and hs.eventtap.checkKeyboardModifiers().cmd then
    openWheel(true, "modifier")
    return false
  end
  armRevert()
  return M.modes[currentMode()].rotate(dir) == true
end

local passing = {} -- key -> true while a passed-through key-down awaits its key-up

-- Written straight to a file: print() inside the tap can fail (hs.ipc) and a failing
-- callback lets the key through to macOS.
local logPath = os.getenv("HOME") .. "/.hammerspoon/knobwheel.log"
local function writeLog(fmt, ...)
  local f = io.open(logPath, "a")
  if not f then return end
  f:write(string.format("%.3f ", hs.timer.secondsSinceEpoch() % 1000), string.format(fmt, ...), "\n")
  f:close()
end

local function debugLog(...)
  if cfg.debugLog then writeLog(...) end
end

local function handleKey(k)
  local key = k.key
  if key == "MUTE" then
    if synthMute > 0 then synthMute = synthMute - 1; return false end
    if k["repeat"] then return true end
    if k.down then onPressDown() else onPressUp() end
    return true
  elseif key == "SOUND_UP" or key == "SOUND_DOWN" then
    if k.down then
      local pass = onRotate(key == "SOUND_UP" and 1 or -1)
      passing[key] = pass
      return not pass
    end
    local pass = passing[key]
    passing[key] = nil
    return not pass
  end
  return false
end

M.tap = hs.eventtap.new({ ev.types.systemDefined }, function(e)
  local k = e:systemKey()
  local key = k.key
  if key ~= "MUTE" and key ~= "SOUND_UP" and key ~= "SOUND_DOWN" then return false end
  local ok, swallow = xpcall(handleKey, debug.traceback, k)
  if not ok then
    writeLog("ERROR %s", tostring(swallow))
    return true -- never leak a knob event to macOS because our code broke
  end
  debugLog("%s %s%s -> %s (mode=%s, wheel=%s)", key, k.down and "down" or "up",
    k["repeat"] and " repeat" or "", swallow and "swallowed" or "passed",
    currentMode(), wheel.canvas and "open" or "closed")
  return swallow
end)

function M.start()
  M.tap:start()
  -- event taps can get disabled by the system after a slow callback; keep it alive
  M.watchdog = hs.timer.doEvery(5, function()
    if not M.paused and not M.tap:isEnabled() then M.tap:start() end
  end)

  menubar = hs.menubar.new()
  menubar:setMenu(function()
    local key = wheelKey()
    local list, active = wheelList(key), currentMode(key)
    local title = key == "default" and "KnobWheel" or ("KnobWheel: " .. key)
    local items = { { title = title, disabled = true } }
    local ids = {}
    for _, id in ipairs(list) do ids[#ids + 1] = id end
    for id in pairs(M.modes) do
      local listed = false
      for _, w in ipairs(list) do if w == id then listed = true end end
      if not listed then ids[#ids + 1] = id end
    end
    for _, id in ipairs(ids) do
      local m = M.modes[id]
      items[#items + 1] = { title = m.name, image = menuIcon(id), checked = id == active,
                            fn = function() setMode(id) end }
    end
    items[#items + 1] = { title = "-" }
    items[#items + 1] = { title = "Reload config", fn = hs.reload }
    items[#items + 1] = { title = "Open Hammerspoon console", fn = hs.openConsole }
    return items
  end)
  updateMenubar()

  -- open the wheel or set a mode from anywhere (Raycast, Shortcuts, a terminal):
  --   open -g "hammerspoon://knobwheel?action=open"   /   "hammerspoon://knobwheel?mode=scroll"
  hs.urlevent.bind("knobwheel", function(_, params)
    local ok, err = xpcall(function()
      if params.mode and M.modes[params.mode] then setMode(params.mode)
      elseif params.action == "open" then openWheel(true)
      elseif params.action == "close" then closeWheel()
      elseif params.action == "pause" then M.paused = true; M.tap:stop()
      elseif params.action == "resume" then M.paused = false; M.tap:start()
      elseif params.action == "reload" then hs.reload() end
    end, debug.traceback)
    if not ok then writeLog("URL ERROR %s", tostring(err)) end
  end)

  -- switching apps: drop any held Cmd / mouse button and show that app's mode
  M.appWatcher = hs.application.watcher.new(function(_, kind)
    if kind == hs.application.watcher.activated then
      releaseCmd()
      endDrag()
      updateMenubar()
    end
  end):start()
  return M
end

M.debug = { openWheel = openWheel, closeWheel = closeWheel, moveWheel = moveWheel,
            commitWheel = commitWheel, setMode = setMode, showHud = showHud,
            mode = function() return currentMode() end }

return M
