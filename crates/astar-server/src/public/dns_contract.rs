// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! One set of expectations every [`DnsProvider`] meets, run against the
//! fake Porkbun (127.0.0.1) and dns-update's in-memory provider. Nothing
//! here reaches a real provider or real DNS.

use std::net::IpAddr;
use std::sync::Arc;

use crate::public::acme::Dns01Solver;
use crate::public::dns::{AddressChange, Dns01, DnsProvider, TxtRecord};

/// What a test can see of the zone without the provider's help.
pub(crate) trait ZoneView {
    fn txt(&self, fqdn: &str) -> Vec<String>;
    fn a(&self, name: &str) -> Vec<String>;
}

const NAME: &str = "69586.example.net";
const CH: &str = "_acme-challenge.69586.example.net";

pub(crate) fn address_is_written_once_then_left(p: &dyn DnsProvider, z: &dyn ZoneView) {
    let ip: IpAddr = "203.0.113.9".parse().unwrap();
    let first = p.set_address(NAME, ip).unwrap();
    assert!(
        matches!(first, AddressChange::Created | AddressChange::Set),
        "{first:?}"
    );
    let second = p.set_address(NAME, ip).unwrap();
    assert!(
        matches!(second, AddressChange::Unchanged | AddressChange::Set),
        "{second:?}"
    );
    let moved: IpAddr = "203.0.113.10".parse().unwrap();
    p.set_address(NAME, moved).unwrap();
    assert_eq!(z.a(NAME), vec!["203.0.113.10".to_string()]);
}

pub(crate) fn our_txt_goes_and_the_operators_stays(p: &dyn DnsProvider, z: &dyn ZoneView) {
    let theirs = p.add_txt(CH, "operator-kept").unwrap();
    let ours = p.add_txt(CH, "acme-value").unwrap();
    let mut both = z.txt(CH);
    both.sort();
    assert_eq!(
        both,
        vec!["acme-value".to_string(), "operator-kept".to_string()]
    );
    p.remove_txt(&ours).unwrap();
    assert_eq!(z.txt(CH), vec!["operator-kept".to_string()]);
    p.remove_txt(&TxtRecord { id: None, ..theirs }).unwrap();
    assert_eq!(z.txt(CH), [] as [std::string::String; 0]);
}

pub(crate) fn removing_an_absent_txt_is_not_an_error(p: &dyn DnsProvider) {
    p.remove_txt(&TxtRecord {
        fqdn: CH.into(),
        value: "never-written".into(),
        id: None,
    })
    .unwrap();
}

pub(crate) fn dns01_leaves_nothing_behind(p: &Arc<dyn DnsProvider>, z: &dyn ZoneView) {
    let d =
        Dns01::new(Arc::clone(p)).with_nameservers(Vec::new(), std::time::Duration::from_secs(5));
    d.present(CH, "v1").unwrap();
    d.wait_visible(&[(CH.into(), "v1".into())]).unwrap();
    d.cleanup(CH, "v1");
    assert_eq!(z.txt(CH), [] as [std::string::String; 0]);
}

mod porkbun {
    use super::*;
    use crate::public::porkbun::tests::fake_porkbun;

    #[test]
    fn porkbun_meets_the_contract() {
        let (pb, c) = fake_porkbun("203.0.113.9");
        address_is_written_once_then_left(&c, &pb);
        our_txt_goes_and_the_operators_stays(&c, &pb);
        removing_an_absent_txt_is_not_an_error(&c);
        let (pb, c) = fake_porkbun("203.0.113.9");
        dns01_leaves_nothing_behind(&(Arc::new(c) as Arc<dyn DnsProvider>), &pb);
    }
}

#[cfg(feature = "dns-providers")]
mod via_dns_update {
    use super::*;
    use crate::public::dns::Scrub;
    use crate::public::dns_update::DnsUpdateProvider;
    use ::dns_update::{DnsRecord, NamedDnsRecord};
    use std::sync::Mutex;

    struct Store(Arc<Mutex<Vec<NamedDnsRecord>>>);
    impl ZoneView for Store {
        fn txt(&self, fqdn: &str) -> Vec<String> {
            let want = format!("{fqdn}.");
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.name == want)
                .filter_map(|r| match &r.record {
                    DnsRecord::TXT(t) => Some(t.clone()),
                    _ => None,
                })
                .collect()
        }
        fn a(&self, name: &str) -> Vec<String> {
            let want = format!("{name}.");
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.name == want)
                .filter_map(|r| match &r.record {
                    DnsRecord::A(a) => Some(a.to_string()),
                    _ => None,
                })
                .collect()
        }
    }

    fn provider() -> (DnsUpdateProvider, Store) {
        let store = Arc::new(Mutex::new(Vec::new()));
        let p = DnsUpdateProvider::with_updater(
            ::dns_update::DnsUpdater::new_in_memory(Arc::clone(&store)),
            "example.net",
            600,
            Scrub::new(Vec::<String>::new()),
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        (p, Store(store))
    }

    #[test]
    fn dns_update_meets_the_contract() {
        let (p, z) = provider();
        address_is_written_once_then_left(&p, &z);
        our_txt_goes_and_the_operators_stays(&p, &z);
        removing_an_absent_txt_is_not_an_error(&p);
        let (p, z) = provider();
        dns01_leaves_nothing_behind(&(Arc::new(p) as Arc<dyn DnsProvider>), &z);
    }
}
