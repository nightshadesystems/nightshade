# active-backup, so `primary` is meaningful. The members carry no addresses:
# the address belongs on the bond, and a constraint enforces that.
interfaces {
    bonding bond0 {
        address 10.0.0.1/24
        hash-policy layer2+3
        lacp-rate fast
        member eth1
        member eth2
        min-links 1
        mode active-backup
        primary eth1
    }
    ethernet eth1 {
    }
    ethernet eth2 {
    }
}
