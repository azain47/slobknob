-- hotkey enable/disable logging is noise every time the wheel opens
hs.hotkey.setLogLevel("warning")

knobwheel = require("knobwheel").start()

hs.alert.show("Hammerspoon loaded")
