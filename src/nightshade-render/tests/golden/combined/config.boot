# A whole box. This is where the interactions live: eth1 and eth2 are enslaved
# to a bond, a VLAN rides on the bond, that VLAN is bridged, and a VXLAN is
# sourced from the bond. Every reference crosses an interface type.
system {
    host-name fw-01
    name-server 1.1.1.1
    time-zone UTC
}
interfaces {
    bonding bond0 {
        address 10.0.0.1/24
        member eth1
        member eth2
        mode 802.3ad
    }
    bridge br0 {
        address 172.16.1.1/24
        member vlan200
        stp
    }
    ethernet eth0 {
        address 192.168.1.1/24
        description "the uplink"
        mtu 9000
    }
    ethernet eth1 {
    }
    ethernet eth2 {
    }
    loopback lo {
        address 127.0.0.1/8
    }
    vlan vlan100 {
        address 172.16.0.1/24
        id 100
        parent bond0
    }
    vlan vlan200 {
        id 200
        parent bond0
    }
    vxlan vxlan1 {
        remote 10.0.0.2
        source-interface bond0
        vni 4242
    }
}
