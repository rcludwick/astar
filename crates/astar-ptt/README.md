# astar-ptt

Pluggable hardware PTT: implement `PttBackend` (raw key-in / radio-key-out /
fail-safe) and `spawn` the 20 ms runner with your session callbacks (`PttIo`).
Ships `Uci150Serial` (AllScan UCI150: CTS handset key in, RTS radio key out by
default; macOS needs the WCH CH34xVCPDriver dext) and a documented `Cm108Hid`
stub. Those are the default lines: the key-input line (CTS/DCD/DSR/RI) and
radio-output line (RTS/DTR) are selectable via `Uci150Serial::open_with(path,
key, radio)` using the `KeyLine` and `RadioLine` enums.
Safety: the radio-key line is re-asserted every tick and released on every
exit path, including consumer-callback panics.
