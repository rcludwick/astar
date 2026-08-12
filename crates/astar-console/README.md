# astar-console

Front-end-agnostic operator-console engine over `astar-iax`: a `ConsoleSession`
you `connect`/`set_ptt`/`snapshot`/`disconnect`, with live TX/RX metering,
shared gain cells, an inspection timeline, and a local mic→speaker parrot for
audio checks. Designed for snapshot-polling UIs (Tauri, web): see the inspect
harness for a production consumer, and `examples/asl3_call.rs` for the
end-to-end ASL3 recipe (token mint + node resolution via `astar-asl3`,
hardware PTT via `astar-ptt`).
