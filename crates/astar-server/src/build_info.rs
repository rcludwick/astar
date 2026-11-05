// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! What this binary was built with, and the systemd unit it can print.

/// The cargo features compiled in, as `astar-server --features` prints them.
pub const FEATURES: &[&str] = &[
    #[cfg(feature = "stream")]
    "stream",
    #[cfg(feature = "stream-mp3")]
    "stream-mp3",
    #[cfg(feature = "codec2-static")]
    "codec2-static",
    #[cfg(feature = "dstar-soft-ambe")]
    "dstar-soft-ambe",
    #[cfg(feature = "public-tls")]
    "public-tls",
    #[cfg(feature = "discord")]
    "discord",
    #[cfg(feature = "device-audio")]
    "device-audio",
    #[cfg(feature = "auth")]
    "auth",
    #[cfg(feature = "dns-providers")]
    "dns-providers",
];

/// A hardened unit for the single static executable: the binary in
/// `/usr/local/bin`, its config in `/etc/astar/node.toml`, its state in
/// systemd's `StateDirectory`. `CAP_NET_BIND_SERVICE` is the one privilege
/// granted, so the public page can bind 80 and 443 without root or a port
/// sysctl.
pub const SYSTEMD_UNIT: &str = "\
[Unit]
Description=astar-server AllStarLink node
Documentation=https://github.com/rcludwick/astar
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=allstar
Group=allstar
DynamicUser=no
ExecStart=/usr/local/bin/astar-server serve --config /etc/astar/node.toml
Restart=always
RestartSec=2
# Certificates, the ACME account and the settings store live here;
# astar-server reads $STATE_DIRECTORY.
StateDirectory=astar
StateDirectoryMode=0700
# The one privilege: bind 80 and 443.
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/etc/astar
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
RestrictNamespaces=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallArchitectures=native

[Install]
WantedBy=multi-user.target
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_grants_only_the_port_capability_and_names_the_state_dir() {
        for line in [
            "User=allstar",
            "DynamicUser=no",
            "StateDirectory=astar",
            "AmbientCapabilities=CAP_NET_BIND_SERVICE",
            "ProtectSystem=strict",
            "ExecStart=/usr/local/bin/astar-server serve --config /etc/astar/node.toml",
        ] {
            assert!(SYSTEMD_UNIT.lines().any(|l| l == line), "{line}");
        }
        assert!(!SYSTEMD_UNIT.contains("CAP_SYS_ADMIN") && !SYSTEMD_UNIT.contains("User=root"));
    }

    #[test]
    fn features_list_what_is_compiled_in() {
        assert_eq!(FEATURES.contains(&"stream"), cfg!(feature = "stream"));
        assert_eq!(
            FEATURES.contains(&"stream-mp3"),
            cfg!(feature = "stream-mp3")
        );
        assert_eq!(FEATURES.contains(&"auth"), cfg!(feature = "auth"));
        assert_eq!(
            FEATURES.contains(&"dstar-soft-ambe"),
            cfg!(feature = "dstar-soft-ambe")
        );
        assert_eq!(
            FEATURES.contains(&"dns-providers"),
            cfg!(feature = "dns-providers")
        );
    }
}
