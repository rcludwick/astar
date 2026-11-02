# CLI fixtures

`live_*.txt` were captured from the rig on 2026-10-11 (`podman exec
nodebench-asl3 asterisk -rx '<command>'`, Asterisk 22.10.1+asl3-3.10.5,
ASL3 node 1999 linked to astar-server 1998). They showed two things the
source-rendered files did not: the `lstats` peer is a bare address and its
millisecond field is not zero-padded, and an unregistered dynamic peer's port
is `(null)`.

Every other `.txt` file here is **hand-made**: rendered from the exact `printf` format strings in
the source, not captured from a running node. They cover cases a
single live link cannot produce (line wrapping at 8 or 4 entries, a lagged
or trunked peer, an inbound connecting link).

| File | Command | Source of the format |
|---|---|---|
| `rpt_nodes_*.txt` | `rpt nodes 1999` | `rpt_do_nodes` and `__mklinklist`, `apps/app_rpt/rpt_cli.c` / `rpt_link.c`, AllStarLink/app_rpt master |
| `rpt_stats.txt` | `rpt stats 1999` | `rpt_do_stats`, same file |
| `rpt_lstats*.txt` | `rpt lstats 1999` | `rpt_do_lstats`, same file |
| `iax2_channels*.txt` | `iax2 show channels` | `handle_cli_iax2_show_channels`, `channels/chan_iax2.c`, Asterisk 22 |
| `iax2_peers.txt` | `iax2 show peers` | `__iax2_show_peers` / `PEERS_FORMAT`, same file |
