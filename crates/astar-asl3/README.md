# astar-asl3

AllStarLink (ASL3) service layer: `mint_wt_token` (native portal login →
Web Transceiver token, used as the IAX2 CALLING_NAME) and `resolve_node`
(DNS TXT node directory with A-record fallback). Config in, values out —
this crate never reads env/files and never logs credentials.
