# Stable local echo (local patch)

Branch `local-echo` carries a client-side change that makes WezTerm's
predictive local echo usable over slow links. How it works is described in
the module docs of `wezterm-client/src/pane/predict.rs` and in the commit
message. It is based on
upstream commit `e1eae81a` (nightly `20261114-121057-e1eae81a`). The mux
protocol is unchanged, so a patched GUI works against a stock mux server
with the same codec version.

Enable it per domain with `local_echo_threshold_ms`, e.g. on an SSH domain:

```lua
config.ssh_domains = {
  { name = 'hpc', remote_address = 'hpc', multiplexing = 'WezTerm',
    local_echo_threshold_ms = 50 },
}
```

## Debugging

`WEZTERM_LOG=wezterm_client::pane::predict=trace` logs each comparison
(server state, outcome, predicted history) and each expiry with its reason.
On Windows the GUI log is `%USERPROFILE%\.local\share\wezterm\wezterm-gui.exe-log-<pid>.txt`.
The trace contains what was typed (prompts that look like they ask for a
secret are never predicted, so never logged).

## Unit tests

```sh
cargo test -p wezterm-client --lib predict
```

## End-to-end test (Linux)

`e2e/` drives a real `wezterm-gui` on a private headless X server (`Xvnc`)
against a stock mux server, through a relay that adds latency, and logs the
cursor line every 10 ms while a Lua script types.

```sh
cargo build --release -p wezterm-gui
contrib/local-echo/e2e/run.sh target/release/wezterm-gui 400 patched   # shell scenario
contrib/local-echo/e2e/run.sh /usr/bin/wezterm-gui 400 stock           # baseline
SCENARIO=claude contrib/local-echo/e2e/run.sh target/release/wezterm-gui 400 cc
SCENARIO=secret contrib/local-echo/e2e/run.sh target/release/wezterm-gui 400 secret
python3 -I contrib/local-echo/e2e/score.py /dev/shm/wt-*/log.txt
```

- `run.sh <gui> <rtt-ms> <label>`: logs to `/dev/shm/wt-<label>/log.txt`.
  Uses `/usr/bin/wezterm-mux-server` and your login zsh (with its plugins).
  The `claude` scenario starts Claude Code in `$HOME` and accepts its
  one-session trust prompt; it never submits a prompt.
- `score.py`: counts display changes more than 20 ms after the last key
  press, i.e. changes the user did not cause. At 400 ms: stock 53 (shell) /
  29 (Claude Code typing phases); patched 6 (only Enter and Ctrl+U results)
  / 0.
- `lagproxy.py` connects straight to the server socket rather than going
  through `wezterm cli proxy`: the proxy reads one PDU for its own
  `SetClientId` handshake, so an unrelated PDU sent first leaves the
  handshake reply in the stream, where it gets matched to the GUI's version
  request ("Please install the same version of wezterm ...").

## Windows build (cross-compiled from Linux)

No MSVC needed: llvm-mingw (tested with release 20261020, ucrt, ubuntu-22.04
x86_64) and the `x86_64-pc-windows-gnullvm` Rust target.

```sh
rustup target add x86_64-pc-windows-gnullvm
TC=/path/to/llvm-mingw-20261020-ucrt-ubuntu-22.04-x86_64/bin
export PATH=$TC:$PATH \
  CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_LINKER=x86_64-w64-mingw32-clang \
  CC_x86_64_pc_windows_gnullvm=x86_64-w64-mingw32-clang \
  CXX_x86_64_pc_windows_gnullvm=x86_64-w64-mingw32-clang++ \
  AR_x86_64_pc_windows_gnullvm=llvm-ar \
  RC_x86_64_pc_windows_gnullvm=x86_64-w64-mingw32-windres
cargo build --release --target x86_64-pc-windows-gnullvm -p wezterm-gui
```

The first attempt fails to link: `libssh-rs-sys` asks for `libcrypto` and
`libssl` (MSVC library names). Copy `libcrypto.a`/`libssl.a` in
`target/x86_64-pc-windows-gnullvm/release/build/openssl-sys-*/out/openssl-build/install/lib`
to `liblibcrypto.a`/`liblibssl.a` and build again.

Ship `wezterm-gui.exe` (optionally `llvm-strip`ped) together with
`x86_64-w64-mingw32/bin/libunwind.dll` from llvm-mingw; every other import is
a Windows system DLL. Install side by side with the stock install:

```powershell
$dst = "$env:LOCALAPPDATA\WezTerm-local-echo"
Copy-Item "C:\Program Files\WezTerm" $dst -Recurse
# then copy wezterm-gui.exe and libunwind.dll into $dst
```

## Updating to a newer WezTerm

This checkout is a shallow clone, so deepen it first:

```sh
git fetch --unshallow origin main && git rebase origin/main local-echo
```

The server's codec version (`codec/src/lib.rs`, `CODEC_VERSION`) must match
the GUI's, so update hpc's mux server and the laptop GUI together.
