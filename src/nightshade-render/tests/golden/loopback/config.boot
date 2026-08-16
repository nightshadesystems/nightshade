# lo takes addresses and a description and nothing else.
interfaces {
    loopback lo {
        address 127.0.0.1/8
        address ::1/128
        description "loopback"
    }
}
