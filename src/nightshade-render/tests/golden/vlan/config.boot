# Two VLANs on one parent, so the parent's .network carries both VLAN= lines.
interfaces {
    ethernet eth0 {
        address 10.0.0.1/24
    }
    vlan vlan100 {
        address 172.16.0.1/24
        id 100
        mtu 1400
        parent eth0
    }
    vlan vlan200 {
        description "guest"
        id 200
        parent eth0
    }
}
