// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! M17 must be in the DEFAULT build of every client, not something a packager
//! has to remember to switch on.
//!
//! Why this is a test and not a comment: the `m17` feature reaches the macOS
//! app, the Iced client and the node daemon by *inheritance* — each of them
//! depends on `astar-station` and simply does not disable default features.
//! That is four separate `Cargo.toml` files any one of which could quietly
//! break the chain, and the failure is silent: everything still compiles, the
//! `m17_*` methods still exist (they are written to exist either way and
//! return `M17("m17 support not compiled")`), and `m17_available()` just
//! starts reporting `false`. A client whose network picker has silently lost
//! M17 looks exactly like a machine with no `libcodec2` installed.
//!
//! These assertions read the manifests as TEXT rather than checking
//! `cfg!(feature = "m17")`, so they hold no matter which features the test
//! binary itself was compiled with — `--no-default-features` must not be able
//! to make this file pass vacuously.

/// Strip `#` comments so a feature named inside prose cannot satisfy a check.
/// The manifests here carry long explanatory comments that mention `m17` and
/// `default-features` repeatedly.
fn uncommented(manifest: &str) -> String {
    manifest
        .lines()
        .map(|l| l.split_once('#').map_or(l, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `default = [...]` array of a manifest, comments removed.
fn default_features(manifest: &str) -> String {
    let src = uncommented(manifest);
    let (_, after) = src
        .split_once("\ndefault = [")
        .expect("manifest declares a `default` feature list");
    let (list, _) = after.split_once(']').expect("`default` list is closed");
    list.to_string()
}

/// The dependency line naming `dep` in `manifest`, comments removed.
fn dep_line(manifest: &str, dep: &str) -> String {
    uncommented(manifest)
        .lines()
        .find(|l| l.trim_start().starts_with(&format!("{dep} = ")))
        .unwrap_or_else(|| panic!("manifest depends on {dep}"))
        .to_string()
}

#[test]
fn station_and_console_ship_m17_by_default() {
    for (who, manifest) in [
        ("astar-station", include_str!("../Cargo.toml")),
        (
            "astar-console",
            include_str!("../../astar-console/Cargo.toml"),
        ),
    ] {
        assert!(
            default_features(manifest).contains("\"m17\""),
            "{who} dropped `m17` from its default feature list — every client \
             inherits M17 from here, and losing it is silent at compile time"
        );
    }
}

#[test]
fn clients_do_not_disable_the_defaults_they_inherit_m17_from() {
    // Each of these builds a shipping artifact: astar-sys is what the Swift
    // binding and therefore the macOS app link, astar-gui is the Iced client,
    // astar-server is the node daemon.
    for (who, manifest) in [
        ("astar-sys", include_str!("../../astar-sys/Cargo.toml")),
        ("astar-gui", include_str!("../../../apps/gui/Cargo.toml")),
        (
            "astar-server",
            include_str!("../../astar-server/Cargo.toml"),
        ),
    ] {
        let line = dep_line(manifest, "astar-station");
        assert!(
            !line.contains("default-features = false"),
            "{who} disables astar-station's default features, which drops M17 \
             from a shipping artifact. If that is deliberate, add \
             `astar-station/m17` explicitly and update this test.\n  {line}"
        );
    }
}

/// `astar-cli` is deliberately NOT in the list above: it is built on
/// `astar-iax` directly and has no M17 subcommand, so it takes
/// `astar-station` as an optional, `default-features = false` dependency
/// pulled in only by its `dstar` feature. This test pins that asymmetry so the
/// two lists above cannot be read as "every crate in the workspace".
#[test]
fn astar_cli_is_the_documented_exception() {
    let line = dep_line(include_str!("../../astar-cli/Cargo.toml"), "astar-station");
    assert!(
        line.contains("optional = true") && line.contains("default-features = false"),
        "astar-cli's astar-station dependency changed shape. If the CLI gained \
         M17, move it into the clients test above and fix the README's \
         what-works table, which currently claims the CLI has no M17.\n  {line}"
    );
}
