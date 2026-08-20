//! `show interfaces capabilities`.
//!
//! What the port could be asked to do, which is a different question from what
//! it is doing and is the one that gets answered before somebody writes a
//! change window. The speed list comes from the driver's supported-modes
//! bitmap rather than from a table of part numbers, so a card nobody has seen
//! before still answers correctly.

use crate::block::Block;
use crate::model::Snapshot;

use super::stanzas;

/// Label field width. Values start at column 18.
const VALUE: usize = 18;

pub fn render(snapshot: &Snapshot) -> String {
    let model = snapshot.system.model.clone();
    let blocks: Vec<String> = super::physical(snapshot)
        .into_iter()
        .filter_map(|interface| {
            let capabilities = interface.capabilities.as_ref()?;
            let mut block = Block::new();
            block.heading(&interface.name);
            block.maybe_aligned(2, "Model:", model.clone(), VALUE);
            block.maybe_aligned(2, "Type:", interface.media_type.clone(), VALUE);
            if !capabilities.speed_duplex.is_empty() {
                block.aligned(
                    2,
                    "Speed/Duplex:",
                    &capabilities.speed_duplex.join(","),
                    VALUE,
                );
            }
            if !capabilities.flowcontrol.is_empty() {
                block.aligned(2, "Flowcontrol:", &capabilities.flowcontrol.join(","), VALUE);
            }
            Some(block.take())
        })
        .collect();
    stanzas(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Capabilities, Interface, Kind, System};

    #[test]
    fn a_port_whose_driver_reports_nothing_is_not_a_block() {
        let snapshot = Snapshot {
            interfaces: vec![Interface::new("eth0", Kind::Ethernet)],
            system: System {
                model: Some("NS-FW-1U-8X10G".into()),
                ..System::default()
            },
        };
        assert_eq!(render(&snapshot), "");
    }

    #[test]
    fn the_platform_model_is_the_same_on_every_port() {
        let mut eth0 = Interface::new("eth0", Kind::Ethernet);
        eth0.capabilities = Some(Capabilities {
            speed_duplex: vec!["1G/full".into(), "auto".into()],
            flowcontrol: vec!["rx-(off,on)".into()],
        });
        let mut eth1 = eth0.clone();
        eth1.name = "eth1".into();
        let snapshot = Snapshot {
            interfaces: vec![eth0, eth1],
            system: System {
                model: Some("NS-FW-1U-8X10G".into()),
                ..System::default()
            },
        };
        let text = render(&snapshot);
        assert_eq!(text.matches("NS-FW-1U-8X10G").count(), 2, "{text}");
        assert!(text.contains("  Speed/Duplex:   1G/full,auto\n"), "{text}");
    }
}
