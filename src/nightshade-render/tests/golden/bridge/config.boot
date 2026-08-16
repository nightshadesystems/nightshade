# A bridge with STP on, over two ports.
interfaces {
    bridge br0 {
        address 10.0.0.1/24
        aging-time 600
        member eth1
        member eth2
        priority 4096
        stp
    }
    ethernet eth1 {
    }
    ethernet eth2 {
    }
}
