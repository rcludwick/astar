// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The ASL3 node driver: `podman exec <ctr> asterisk -rx '<cmd>'` for CLI
//! commands, and [`ami::AmiClient`] for the manager interface and its event
//! stream. The typed tables are in [`parse`].

pub mod ami;
pub mod parse;

use std::fmt;
use std::time::Duration;

use crate::rig::{Podman, RigConfig, RigError, Secret};

pub use parse::{
    IaxChannels, IaxPeers, ParseError, RptLinkStat, RptNodes, RptStats, parse_iax2_channels,
    parse_iax2_peers, parse_rpt_lstats, parse_rpt_nodes, parse_rpt_stats,
};

/// A node-side failure: running the command, or reading its output.
#[derive(Debug)]
pub enum NodeError {
    Rig(RigError),
    Parse {
        command: String,
        err: ParseError,
    },
    Ami(ami::AmiError),
    /// The CLI answered, but refused the command.
    Refused {
        command: String,
        reply: String,
    },
}

/// The refusal line in an `rpt cmd` reply, if it is one.
#[must_use]
pub fn rpt_cmd_refusal(out: &str) -> Option<&str> {
    out.lines().map(str::trim).find(|l| {
        l.starts_with("Unknown node number")
            || l.starts_with("Unknown action name")
            || (l.starts_with("Node ") && l.ends_with("is not ready."))
            || l.starts_with("Usage:")
            || l.starts_with("No such command")
    })
}

impl fmt::Display for NodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rig(e) => write!(f, "{e}"),
            Self::Parse { command, err } => write!(f, "`{command}`: {err}"),
            Self::Ami(e) => write!(f, "{e}"),
            Self::Refused { command, reply } => write!(f, "`{command}` refused: {reply}"),
        }
    }
}

impl std::error::Error for NodeError {}

impl From<RigError> for NodeError {
    fn from(e: RigError) -> Self {
        Self::Rig(e)
    }
}

impl From<ami::AmiError> for NodeError {
    fn from(e: ami::AmiError) -> Self {
        Self::Ami(e)
    }
}

/// `ilink` sub-commands used by the bench (`app_rpt` function numbers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ilink {
    /// `ilink 1` — disconnect.
    Disconnect,
    /// `ilink 2` — monitor.
    Monitor,
    /// `ilink 3` — transceive.
    Transceive,
    /// `ilink 6` — disconnect all links.
    DisconnectAll,
}

impl Ilink {
    #[must_use]
    pub fn code(self) -> u8 {
        match self {
            Self::Disconnect => 1,
            Self::Monitor => 2,
            Self::Transceive => 3,
            Self::DisconnectAll => 6,
        }
    }
}

/// Drives the ASL3 node container.
#[derive(Debug, Clone)]
pub struct NodeDriver {
    podman: Podman,
    pub container: String,
    pub node: String,
    ami_addr: std::net::SocketAddr,
    ami_user: String,
    ami_secret: Secret,
}

impl NodeDriver {
    #[must_use]
    pub fn new(cfg: &RigConfig) -> Self {
        Self {
            podman: cfg.podman(),
            container: cfg.node_container.clone(),
            node: cfg.node_number.clone(),
            ami_addr: cfg.ami,
            ami_user: cfg.ami_user.clone(),
            ami_secret: cfg.ami_secret.clone(),
        }
    }

    /// `asterisk -rx '<cli>'` inside the node container → raw text.
    pub fn rx(&self, cli: &str) -> Result<String, NodeError> {
        Ok(self
            .podman
            .exec(&self.container, &["asterisk", "-rx", cli])?)
    }

    fn parsed<T>(
        &self,
        cli: &str,
        f: impl FnOnce(&str) -> Result<T, ParseError>,
    ) -> Result<T, NodeError> {
        let text = self.rx(cli)?;
        f(&text).map_err(|err| NodeError::Parse {
            command: cli.to_string(),
            err,
        })
    }

    /// `rpt nodes <this node>`.
    pub fn rpt_nodes(&self) -> Result<RptNodes, NodeError> {
        self.parsed(&format!("rpt nodes {}", self.node), parse_rpt_nodes)
    }

    /// `rpt stats <this node>`.
    pub fn rpt_stats(&self) -> Result<RptStats, NodeError> {
        self.parsed(&format!("rpt stats {}", self.node), parse_rpt_stats)
    }

    /// `rpt lstats <this node>` — direct links only.
    pub fn rpt_lstats(&self) -> Result<Vec<RptLinkStat>, NodeError> {
        self.parsed(&format!("rpt lstats {}", self.node), parse_rpt_lstats)
    }

    pub fn iax2_channels(&self) -> Result<IaxChannels, NodeError> {
        self.parsed("iax2 show channels", parse_iax2_channels)
    }

    pub fn iax2_peers(&self) -> Result<IaxPeers, NodeError> {
        self.parsed("iax2 show peers", parse_iax2_peers)
    }

    /// `rpt localnodes` lists this node — `app_rpt` loaded it.
    pub fn node_loaded(&self) -> Result<bool, NodeError> {
        let text = self.rx("rpt localnodes")?;
        Ok(text.split_whitespace().any(|w| w == self.node))
    }

    /// `rpt cmd <this node> ilink <n> <target>`.
    ///
    /// `asterisk -rx` exits 0 even when `app_rpt` refuses, so a reply naming an
    /// unknown node/action, a not-ready node, or a usage message is an error.
    pub fn ilink(&self, op: Ilink, target: &str) -> Result<String, NodeError> {
        let cli = format!("rpt cmd {} ilink {} {target}", self.node, op.code());
        let cli = cli.trim_end();
        let out = self.rx(cli)?;
        if let Some(why) = rpt_cmd_refusal(&out) {
            return Err(NodeError::Refused {
                command: cli.to_string(),
                reply: why.to_string(),
            });
        }
        Ok(out)
    }

    /// Turn on the debug the report wants on failure:
    /// `iax2 set debug on` and `rpt debug level 7`.
    pub fn enable_debug(&self) -> Result<(), NodeError> {
        self.rx("iax2 set debug on")?;
        self.rx("rpt debug level 7")?;
        Ok(())
    }

    /// The direct link to `peer`, from `rpt lstats`, if present.
    pub fn link_to(&self, peer: &str) -> Result<Option<RptLinkStat>, NodeError> {
        Ok(self.rpt_lstats()?.into_iter().find(|l| l.node == peer))
    }

    /// Open an AMI session and log in.
    pub fn ami(&self, events: bool, timeout: Duration) -> Result<ami::AmiClient, NodeError> {
        let mut c = ami::AmiClient::connect(self.ami_addr, timeout)?;
        c.login(&self.ami_user, self.ami_secret.expose(), events)?;
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ilink_codes() {
        assert_eq!(Ilink::Disconnect.code(), 1);
        assert_eq!(Ilink::Monitor.code(), 2);
        assert_eq!(Ilink::Transceive.code(), 3);
        assert_eq!(Ilink::DisconnectAll.code(), 6);
    }

    #[test]
    fn rpt_cmd_refusals() {
        assert_eq!(rpt_cmd_refusal(""), None);
        assert_eq!(
            rpt_cmd_refusal("Node 1999 is not ready.\n"),
            Some("Node 1999 is not ready.")
        );
        assert!(rpt_cmd_refusal("Unknown node number 1997.\n").is_some());
        assert!(rpt_cmd_refusal("Usage: rpt cmd <nodename> ...\n").is_some());
    }

    #[test]
    fn driver_does_not_debug_print_secret() {
        let mut cfg = RigConfig::new("/repo".into());
        cfg.ami_secret = Secret::new("swordfish");
        let d = NodeDriver::new(&cfg);
        // Debug is derived; make sure the secret is not in it.
        let s = format!("{d:?}");
        assert!(!s.contains("swordfish"), "{s}");
    }
}
