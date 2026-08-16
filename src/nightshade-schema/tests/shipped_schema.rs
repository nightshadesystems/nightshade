//! The schema that actually ships.
//!
//! Everything here loads `schema/` from the source tree and exercises it as an
//! operator would. A unit test with a hand-written three-node schema proves
//! the engine works; these prove the schema does.

use nightshade_schema::config::ConfigTree;
use nightshade_schema::model::{Location, NodeKind, Schema, SchemaNode};
use nightshade_schema::path::Path;
use nightshade_schema::validate::SetError;
use nightshade_schema::{curly, loader};

fn schema() -> Schema {
    loader::load_dir(&loader::source_dir()).expect("the shipped schema must load")
}

fn p(s: &str) -> Path {
    Path::parse(s).unwrap()
}

/// Build a config by `set`ting paths, the way an operator would, and check as
/// we go that each one is legal. A typo in a test fixture that the schema
/// would have rejected is a test that proves nothing.
fn config(schema: &Schema, entries: &[(&str, &str)]) -> ConfigTree {
    let mut tree = ConfigTree::new();
    for (path, value) in entries {
        let path = p(path);
        let value = (!value.is_empty()).then_some(*value);
        schema
            .validate_set(&path, value)
            .unwrap_or_else(|e| panic!("fixture is not valid: {e}"));
        match value {
            Some(v) => tree.add(&path, v).unwrap(),
            // A `set` with no value is either a flag or a bare tag instance
            // -- `disable` against `interfaces ethernet eth1` -- and only the
            // schema can tell them apart. Getting it wrong makes an interface
            // a leaf, at which point nothing can reference it and nothing can
            // be configured under it, so this is exactly the decision configd
            // has to make too.
            None => match schema.resolve(&path) {
                Some(Location::Instance(_)) => {
                    tree.ensure_interior(&path).unwrap();
                }
                _ => tree.declare_leaf(&path).unwrap(),
            },
        }
    }
    tree
}

/// The messages of every cross-node violation, joined, for substring
/// assertions.
fn violations(schema: &Schema, tree: &ConfigTree) -> String {
    schema
        .check_constraints(tree)
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// the schema itself
// ---------------------------------------------------------------------------

#[test]
fn every_node_has_help_and_a_reachable_type() {
    fn walk(node: &SchemaNode, at: &str, seen: &mut usize) {
        assert!(!node.help.trim().is_empty(), "{at} has no help text");
        *seen += 1;
        for (name, child) in &node.children {
            walk(child, &format!("{at} {name}"), seen);
        }
    }
    let schema = schema();
    let mut seen = 0;
    for (name, node) in &schema.root.children {
        walk(node, name, &mut seen);
    }
    // A guard against the schema directory silently failing to merge: if a
    // file stops being read, this number collapses.
    assert!(seen > 50, "only {seen} schema nodes loaded");
}

#[test]
fn all_six_interface_types_are_present_with_the_common_leaves() {
    let schema = schema();
    let interfaces = &schema.root.children["interfaces"];
    let mut types: Vec<&str> = interfaces.children.keys().map(String::as_str).collect();
    types.sort();
    assert_eq!(
        types,
        ["bonding", "bridge", "ethernet", "loopback", "vlan", "vxlan"]
    );

    for name in ["ethernet", "bonding", "bridge", "vxlan"] {
        let node = &interfaces.children[name];
        for leaf in ["address", "description", "mtu", "mac", "disable"] {
            assert!(
                node.children.contains_key(leaf),
                "{name} is missing the common leaf {leaf}"
            );
        }
    }

    // vlan takes its MAC from its parent, so the override is excluded.
    assert!(!interfaces.children["vlan"].children.contains_key("mac"));

    // loopback keeps only what means something on it.
    let loopback = &interfaces.children["loopback"];
    let mut leaves: Vec<&str> = loopback.children.keys().map(String::as_str).collect();
    leaves.sort();
    assert_eq!(leaves, ["address", "description"]);
    assert!(
        loopback.children["address"].value_spec().unwrap().accepts.is_empty(),
        "there is nothing for loopback to DHCP from"
    );
}

#[test]
fn priorities_order_masters_before_what_rides_on_them() {
    let schema = schema();
    let interfaces = &schema.root.children["interfaces"];
    let at = |name: &str| interfaces.children[name].priority;

    assert!(schema.root.children["system"].priority < at("ethernet"));
    assert!(at("ethernet") < at("bonding"));
    assert!(at("bonding") < at("bridge"));
    assert!(at("bridge") < at("vlan"));
    assert!(at("vlan") < at("vxlan"));
}

#[test]
fn defaults_are_the_ones_that_need_no_interface() {
    let schema = schema();
    let defaults = schema.defaults();
    assert_eq!(
        defaults.get(&p("system host-name")).unwrap().value(),
        Some("nightshade")
    );
    assert_eq!(defaults.get(&p("system time-zone")).unwrap().value(), Some("UTC"));
    // Per-interface defaults have nowhere to live until an interface does.
    assert!(!defaults.contains(&p("interfaces")));
    assert!(schema.validate_tree(&defaults).is_empty());
}

// ---------------------------------------------------------------------------
// validate_set
// ---------------------------------------------------------------------------

#[test]
fn set_accepts_what_the_schema_describes() {
    let schema = schema();
    let ok: &[(&str, Option<&str>)] = &[
        ("system host-name", Some("fw-01")),
        ("system name-server", Some("1.1.1.1")),
        ("system time-zone", Some("UTC")),
        ("interfaces ethernet eth0", None),
        ("interfaces ethernet eth0 address", Some("192.168.1.1/24")),
        ("interfaces ethernet eth0 address", Some("dhcp")),
        ("interfaces ethernet eth0 mtu", Some("9000")),
        ("interfaces ethernet eth0 disable", None),
        ("interfaces ethernet enp1s0 speed", Some("10000")),
        ("interfaces loopback lo address", Some("127.0.0.1/8")),
        ("interfaces vlan vlan100 id", Some("100")),
        ("interfaces bonding bond0 mode", Some("802.3ad")),
        ("interfaces bridge br0 priority", Some("4096")),
        ("interfaces vxlan vxlan1 vni", Some("4242")),
    ];
    for (path, value) in ok {
        schema
            .validate_set(&p(path), *value)
            .unwrap_or_else(|e| panic!("`{path} {}` should be accepted: {e}", value.unwrap_or("")));
    }
}

#[test]
fn set_rejects_with_a_message_about_the_right_thing() {
    let schema = schema();

    // Unknown path.
    assert!(matches!(
        schema.validate_set(&p("system hostname"), Some("fw")),
        Err(SetError::UnknownPath { .. })
    ));

    // Bad value, named as a value.
    let err = schema
        .validate_set(&p("interfaces ethernet eth0 mtu"), Some("100000"))
        .unwrap_err();
    assert!(err.to_string().contains("between 68 and 9216"), "{err}");

    // A bad tag key is reported as the interface name, not as whatever came
    // after it.
    let err = schema
        .validate_set(&p("interfaces ethernet \"eth 0\" mtu"), Some("1500"))
        .unwrap_err();
    assert!(matches!(err, SetError::BadName { .. }), "{err}");
    assert!(err.to_string().contains("interface name"), "{err}");

    // Conventional names are enforced on the types that have them.
    assert!(schema.validate_set(&p("interfaces vlan eth0 id"), Some("1")).is_err());
    assert!(schema.validate_set(&p("interfaces bonding br0"), None).is_err());
    assert!(schema.validate_set(&p("interfaces loopback lo1"), None).is_err());

    // A leaf with no value, and a flag with one.
    let err = schema.validate_set(&p("system host-name"), None).unwrap_err();
    assert!(matches!(err, SetError::ValueRequired { .. }), "{err}");
    let err = schema
        .validate_set(&p("interfaces ethernet eth0 disable"), Some("yes"))
        .unwrap_err();
    assert!(matches!(err, SetError::UnexpectedValue { .. }), "{err}");

    // A container is not a thing you set.
    assert!(matches!(
        schema.validate_set(&p("system"), None),
        Err(SetError::NotSettable { .. })
    ));

    // Stepped ranges.
    assert!(schema
        .validate_set(&p("interfaces bridge br0 priority"), Some("5000"))
        .is_err());
}

// ---------------------------------------------------------------------------
// cross-node constraints -- the reference cases
// ---------------------------------------------------------------------------

#[test]
fn a_vlan_parent_must_be_a_configured_interface() {
    let schema = schema();

    let good = config(
        &schema,
        &[
            ("interfaces ethernet eth0", ""),
            ("interfaces vlan vlan100 parent", "eth0"),
            ("interfaces vlan vlan100 id", "100"),
        ],
    );
    assert_eq!(schema.check_constraints(&good), []);

    // eth7 was never configured.
    let bad = config(
        &schema,
        &[
            ("interfaces ethernet eth0", ""),
            ("interfaces vlan vlan100 parent", "eth7"),
            ("interfaces vlan vlan100 id", "100"),
        ],
    );
    let found = schema.check_constraints(&bad);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].path, p("interfaces vlan vlan100 parent"));
    assert!(found[0].message.contains("eth7"), "{}", found[0].message);
}

#[test]
fn deleting_an_interface_others_reference_names_every_referrer() {
    let schema = schema();
    let mut tree = config(
        &schema,
        &[
            ("interfaces ethernet eth0", ""),
            ("interfaces vlan vlan100 parent", "eth0"),
            ("interfaces vlan vlan100 id", "100"),
            ("interfaces vlan vlan200 parent", "eth0"),
            ("interfaces vlan vlan200 id", "200"),
            ("interfaces bridge br0 member", "eth0"),
        ],
    );
    assert_eq!(schema.check_constraints(&tree), []);

    tree.remove(&p("interfaces ethernet eth0"));
    let found = schema.check_constraints(&tree);
    let paths: Vec<String> = found.iter().map(|v| v.path.to_string()).collect();
    assert!(paths.contains(&"interfaces vlan vlan100 parent".to_string()), "{paths:?}");
    assert!(paths.contains(&"interfaces vlan vlan200 parent".to_string()), "{paths:?}");
    assert!(paths.contains(&"interfaces bridge br0 member".to_string()), "{paths:?}");
}

#[test]
fn a_bond_or_bridge_member_cannot_have_its_own_address() {
    let schema = schema();

    for master in ["bonding bond0", "bridge br0"] {
        let tree = config(
            &schema,
            &[
                ("interfaces ethernet eth1", ""),
                ("interfaces ethernet eth1 address", "10.0.0.1/24"),
                (&format!("interfaces {master} member"), "eth1"),
            ],
        );
        let found = schema.check_constraints(&tree);
        assert_eq!(found.len(), 1, "{master}: {found:?}");
        assert_eq!(found[0].path, p("interfaces ethernet eth1 address"));
        assert!(found[0].message.contains("eth1"), "{}", found[0].message);
        // The message names the master, so the operator knows where the
        // address belongs instead.
        let name = master.split_whitespace().nth(1).unwrap();
        assert!(found[0].message.contains(name), "{}", found[0].message);
    }

    // The address on the master itself is the point of the arrangement.
    let ok = config(
        &schema,
        &[
            ("interfaces ethernet eth1", ""),
            ("interfaces bonding bond0 member", "eth1"),
            ("interfaces bonding bond0 address", "10.0.0.1/24"),
        ],
    );
    assert_eq!(schema.check_constraints(&ok), []);
}

#[test]
fn an_interface_cannot_be_enslaved_twice() {
    let schema = schema();

    let tree = config(
        &schema,
        &[
            ("interfaces ethernet eth1", ""),
            ("interfaces bonding bond0 member", "eth1"),
            ("interfaces bridge br0 member", "eth1"),
        ],
    );
    let messages = violations(&schema, &tree);
    assert!(messages.contains("more than one"), "{messages}");
    assert!(messages.contains("eth1"), "{messages}");

    // Two bridges is the same mistake.
    let tree = config(
        &schema,
        &[
            ("interfaces ethernet eth1", ""),
            ("interfaces bridge br0 member", "eth1"),
            ("interfaces bridge br1 member", "eth1"),
        ],
    );
    assert!(violations(&schema, &tree).contains("more than one"));
}

#[test]
fn a_bond_primary_must_be_one_of_its_own_members() {
    let schema = schema();

    let good = config(
        &schema,
        &[
            ("interfaces ethernet eth1", ""),
            ("interfaces ethernet eth2", ""),
            ("interfaces bonding bond0 mode", "active-backup"),
            ("interfaces bonding bond0 member", "eth1"),
            ("interfaces bonding bond0 member", "eth2"),
            ("interfaces bonding bond0 primary", "eth1"),
        ],
    );
    assert_eq!(schema.check_constraints(&good), []);

    // A real interface, but a member of the other bond.
    let bad = config(
        &schema,
        &[
            ("interfaces ethernet eth1", ""),
            ("interfaces ethernet eth3", ""),
            ("interfaces bonding bond0 mode", "active-backup"),
            ("interfaces bonding bond0 member", "eth1"),
            ("interfaces bonding bond1 mode", "active-backup"),
            ("interfaces bonding bond1 member", "eth3"),
            ("interfaces bonding bond0 primary", "eth3"),
        ],
    );
    let messages = violations(&schema, &bad);
    assert!(messages.contains("not a member of this bond"), "{messages}");
}

#[test]
fn primary_only_applies_in_active_backup_mode() {
    let schema = schema();
    let tree = config(
        &schema,
        &[
            ("interfaces ethernet eth1", ""),
            ("interfaces bonding bond0 mode", "802.3ad"),
            ("interfaces bonding bond0 member", "eth1"),
            ("interfaces bonding bond0 primary", "eth1"),
        ],
    );
    assert!(violations(&schema, &tree).contains("active-backup"));
}

#[test]
fn a_vlan_id_is_unique_per_parent() {
    let schema = schema();

    // The same ID on two different parents is ordinary.
    let ok = config(
        &schema,
        &[
            ("interfaces ethernet eth0", ""),
            ("interfaces ethernet eth1", ""),
            ("interfaces vlan vlan100 parent", "eth0"),
            ("interfaces vlan vlan100 id", "100"),
            ("interfaces vlan vlan101 parent", "eth1"),
            ("interfaces vlan vlan101 id", "100"),
        ],
    );
    assert_eq!(schema.check_constraints(&ok), []);

    // The same ID twice on one parent is two interfaces the kernel will not
    // both create.
    let bad = config(
        &schema,
        &[
            ("interfaces ethernet eth0", ""),
            ("interfaces vlan vlan100 parent", "eth0"),
            ("interfaces vlan vlan100 id", "100"),
            ("interfaces vlan vlan101 parent", "eth0"),
            ("interfaces vlan vlan101 id", "100"),
        ],
    );
    let messages = violations(&schema, &bad);
    assert!(messages.contains("same parent and id"), "{messages}");
}

#[test]
fn a_vni_is_unique_across_vxlans() {
    let schema = schema();
    let tree = config(
        &schema,
        &[
            ("interfaces vxlan vxlan1 vni", "4242"),
            ("interfaces vxlan vxlan1 remote", "10.0.0.2"),
            ("interfaces vxlan vxlan2 vni", "4242"),
            ("interfaces vxlan vxlan2 remote", "10.0.0.3"),
        ],
    );
    assert!(violations(&schema, &tree).contains("same vni"));
}

#[test]
fn a_vxlan_is_unicast_or_multicast_but_not_both() {
    let schema = schema();

    let both = config(
        &schema,
        &[
            ("interfaces ethernet eth0", ""),
            ("interfaces vxlan vxlan1 vni", "1"),
            ("interfaces vxlan vxlan1 remote", "10.0.0.2"),
            ("interfaces vxlan vxlan1 group", "239.1.1.1"),
            ("interfaces vxlan vxlan1 source-interface", "eth0"),
        ],
    );
    assert!(violations(&schema, &both).contains("cannot both be set"));

    // A group needs an interface to join it on.
    let orphan_group = config(
        &schema,
        &[
            ("interfaces vxlan vxlan1 vni", "1"),
            ("interfaces vxlan vxlan1 group", "239.1.1.1"),
        ],
    );
    assert!(violations(&schema, &orphan_group).contains("source-interface"));

    let ok = config(
        &schema,
        &[
            ("interfaces ethernet eth0", ""),
            ("interfaces vxlan vxlan1 vni", "1"),
            ("interfaces vxlan vxlan1 group", "239.1.1.1"),
            ("interfaces vxlan vxlan1 source-interface", "eth0"),
        ],
    );
    assert_eq!(schema.check_constraints(&ok), []);
}

#[test]
fn required_leaves_are_required() {
    let schema = schema();

    let mut tree = ConfigTree::new();
    tree.ensure_interior(&p("interfaces vlan vlan100")).unwrap();
    let messages = violations(&schema, &tree);
    assert!(messages.contains("interfaces vlan vlan100 parent"), "{messages}");
    assert!(messages.contains("interfaces vlan vlan100 id"), "{messages}");
    assert!(messages.contains("required"), "{messages}");

    let mut tree = ConfigTree::new();
    tree.ensure_interior(&p("interfaces vxlan vxlan1")).unwrap();
    assert!(violations(&schema, &tree).contains("interfaces vxlan vxlan1 vni"));
}

// ---------------------------------------------------------------------------
// structure and types over a whole config
// ---------------------------------------------------------------------------

#[test]
fn a_hand_edited_file_is_diagnosed_rather_than_accepted() {
    let schema = schema();
    let tree = curly::parse(
        r#"
system {
    host-name "not a host name"
    nameserver 1.1.1.1
}
interfaces {
    ethernet eth0 {
        mtu 100000
        mtu 1500
        disable yes
    }
    ethernet "eth 1" {
        address 10.0.0.1/24
    }
}
"#,
    )
    .unwrap();

    let messages: Vec<String> = schema
        .validate_tree(&tree)
        .iter()
        .map(|v| v.to_string())
        .collect();
    let all = messages.join("\n");

    assert!(all.contains("not a host name"), "{all}");
    assert!(all.contains("nameserver"), "{all}");
    assert!(all.contains("is not a configuration path"), "{all}");
    assert!(all.contains("takes one value but has 2"), "{all}");
    assert!(all.contains("takes no value"), "{all}");
    assert!(all.contains("eth 1"), "{all}");
}

#[test]
fn a_valid_config_survives_a_save_and_a_load() {
    let schema = schema();
    let tree = config(
        &schema,
        &[
            ("system host-name", "fw-01"),
            ("system name-server", "1.1.1.1"),
            ("system name-server", "9.9.9.9"),
            ("system time-zone", "UTC"),
            ("interfaces loopback lo address", "127.0.0.1/8"),
            ("interfaces ethernet eth0 address", "192.168.1.1/24"),
            ("interfaces ethernet eth0 description", "the uplink"),
            ("interfaces ethernet eth1", ""),
            ("interfaces ethernet eth2", ""),
            ("interfaces bonding bond0 member", "eth1"),
            ("interfaces bonding bond0 member", "eth2"),
            ("interfaces bonding bond0 address", "10.0.0.1/24"),
            // Routed: carries an address of its own.
            ("interfaces vlan vlan100 parent", "eth0"),
            ("interfaces vlan vlan100 id", "100"),
            ("interfaces vlan vlan100 address", "172.16.0.1/24"),
            // Bridged: no address, because the address belongs on br0.
            ("interfaces vlan vlan200 parent", "eth0"),
            ("interfaces vlan vlan200 id", "200"),
            ("interfaces bridge br0 member", "vlan200"),
            ("interfaces bridge br0 address", "172.16.1.1/24"),
            ("interfaces bridge br0 stp", ""),
            ("interfaces vxlan vxlan1 vni", "4242"),
            ("interfaces vxlan vxlan1 remote", "10.0.0.2"),
            ("interfaces vxlan vxlan1 source-interface", "bond0"),
        ],
    );

    assert_eq!(schema.validate_tree(&tree), []);
    assert_eq!(schema.check_constraints(&tree), []);

    let text = curly::render(&tree, &schema);
    // The schema-aware renderer writes the compact tag form.
    assert!(text.contains("ethernet eth0 {"), "{text}");
    assert!(text.contains("bonding bond0 {"), "{text}");
    assert_eq!(curly::parse(&text).unwrap(), tree, "round trip changed the tree");
    // And saving it again is byte-identical.
    assert_eq!(curly::render(&curly::parse(&text).unwrap(), &schema), text);
}

// ---------------------------------------------------------------------------
// completion metadata
// ---------------------------------------------------------------------------

#[test]
fn children_of_drives_completion_at_every_position() {
    let schema = schema();

    let top: Vec<String> = schema
        .children_of(&Path::root())
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(top, ["interfaces", "system"]);

    let system = schema.children_of(&p("system"));
    let names: Vec<&str> = system.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["host-name", "name-server", "time-zone"]);
    let name_server = system.iter().find(|c| c.name == "name-server").unwrap();
    assert!(name_server.multi);
    assert_eq!(name_server.value.as_deref(), Some("<ip-address>"));
    assert!(!name_server.help.is_empty());

    // At a tag node the operator invents the next word, so what comes back is
    // a placeholder to show, not a list to complete from.
    let ethernet = schema.children_of(&p("interfaces ethernet"));
    assert_eq!(ethernet.len(), 1);
    assert!(ethernet[0].placeholder);
    assert_eq!(ethernet[0].name, "<interface>");

    // Inside an instance, the tag node's children are back.
    let inside: Vec<String> = schema
        .children_of(&p("interfaces ethernet eth0"))
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(
        inside,
        ["address", "description", "disable", "duplex", "mac", "mtu", "speed"]
    );

    // At a leaf, what it takes.
    let mtu = schema.children_of(&p("interfaces ethernet eth0 mtu"));
    assert_eq!(mtu.len(), 1);
    assert!(mtu[0].placeholder);
    assert_eq!(mtu[0].name, "<68-9216>");
    assert_eq!(mtu[0].default.as_deref(), Some("1500"));

    // The `dhcp` keyword shows up where it is accepted and not where it is not.
    let address = schema.children_of(&p("interfaces ethernet eth0 address"));
    assert!(address[0].name.contains("dhcp"), "{}", address[0].name);
    let loopback = schema.children_of(&p("interfaces loopback lo address"));
    assert!(!loopback[0].name.contains("dhcp"), "{}", loopback[0].name);

    // A flag takes nothing, and an unknown path offers nothing.
    assert!(schema.children_of(&p("interfaces ethernet eth0 disable")).is_empty());
    assert!(schema.children_of(&p("nonsense")).is_empty());
}

#[test]
fn the_renderer_knows_which_nodes_are_tag_nodes() {
    let schema = schema();
    for tag in ["ethernet", "loopback", "vlan", "bonding", "bridge", "vxlan"] {
        assert!(schema.is_tag_node(&p(&format!("interfaces {tag}"))), "{tag}");
    }
    assert!(!schema.is_tag_node(&p("interfaces")));
    assert!(!schema.is_tag_node(&p("system")));
    assert!(!schema.is_tag_node(&p("interfaces ethernet eth0")));
    assert!(matches!(
        schema.root.children["interfaces"].children["ethernet"].kind,
        NodeKind::Tag(_)
    ));
}
