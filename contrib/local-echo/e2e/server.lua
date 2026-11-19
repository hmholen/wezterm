local wezterm = require 'wezterm'
local config = wezterm.config_builder()
config.unix_domains = { { name = 'testmux', socket_path = os.getenv('TEST_SOCK'), no_serve_automatically = true } }
config.default_prog = { '/usr/bin/zsh', '-l' }
return config
