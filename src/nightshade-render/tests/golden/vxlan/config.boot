# vxlan1 is a unicast tunnel attached to eth0; vxlan2 is multicast, which is
# why it also needs a source interface; vxlan3 has neither and so has to be
# told to stand alone.
interfaces {
    ethernet eth0 {
        address 10.0.0.1/24
    }
    vxlan vxlan1 {
        address 172.16.0.1/24
        port 4789
        remote 10.0.0.2
        source-address 10.0.0.1
        source-interface eth0
        ttl 64
        vni 4242
    }
    vxlan vxlan2 {
        group 239.1.1.1
        source-interface eth0
        vni 4243
    }
    vxlan vxlan3 {
        remote 10.0.0.3
        vni 4244
    }
}
