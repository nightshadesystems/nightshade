# eth0 exercises everything that produces a .link file; eth1 exercises DHCP
# and the disabled state, and produces no .link file at all.
interfaces {
    ethernet eth0 {
        address 192.168.1.1/24
        address 2001:db8::1/64
        description "the uplink"
        duplex full
        mac 02:00:5e:10:00:01
        mtu 9000
        speed 10000
    }
    ethernet eth1 {
        address dhcp
        disable
    }
}
