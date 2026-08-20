Implement the complete `show interfaces` command family for Nightshade OS, with full feature parity with the latest Arista EOS release (currently EOS 4.36.1F) and output formatting that matches EOS byte-for-byte in structure, alignment, casing, and phrasing — adapted to Nightshade's conventions defined below. If Arista's current documentation (https://www.arista.com/en/um-eos) shows a `show interfaces` subcommand or output field added since 4.36.1F that is not in the reference outputs below, include it, following the same formatting rules and Nightshade conventions.

## Project Context

- Nightshade OS is a custom Debian 13-based firewall OS built as a **Rust monorepo**.
- The CLI uses a VyOS/JunOS-style operational mode; `show interfaces ...` commands are operational-mode commands.
- Interface counters, link state history, and rate calculations must be tracked by a long-running daemon (netlink monitor + counter sampler); the CLI queries the daemon (use the monorepo's existing IPC pattern — inspect the repo and follow whatever gRPC/unix-socket convention already exists).
- Target platform data sources are the Linux kernel: rtnetlink, ethtool ioctls/netlink, sysfs. No vendor SDKs.

## Nightshade Conventions (deviations from EOS — these override EOS look where they conflict)

1. **Interface naming: native Linux names.** `eth0`–`eth7`, `lo`, `vlan<id>` (e.g. `vlan10`), `bond<id>`, `tun<id>`, `wg<id>`. Do NOT rename to Arista-style `Ethernet1`/`Et1`. Tables use the same names (no abbreviation column mapping). Sorting is natural sort: `eth0, eth1, ... eth9, eth10`.
2. **MAC address format: colon-separated lowercase** `xx:xx:xx:xx:xx:xx` (e.g. `2c:dd:e9:12:00:a1`), NOT Cisco/Arista dotted `xxxx.xxxx.xxxx`. This applies everywhere a MAC appears, including the `(bia ...)` field.
3. Everything else — column layouts, field names, phrasing, indentation, capitalization, counter names, section ordering — matches Arista EOS as specified in the reference outputs below. Column widths must expand gracefully for longer Linux names but keep EOS's visual rhythm (recompute pad widths from the longest name in the output set, minimum widths as shown).

## Command Tree to Implement

```
show interfaces
show interfaces <name>
show interfaces description
show interfaces status [connected | notconnect | errdisabled | inactive]
show interfaces counters
show interfaces counters errors
show interfaces counters discards
show interfaces counters rates
show interfaces counters queue
show interfaces counters bins
show interfaces transceiver
show interfaces transceiver detail
show interfaces transceiver properties
show interfaces transceiver eeprom
show interfaces capabilities
show interfaces flowcontrol
show interfaces negotiation [detail]
show interfaces phy [detail]
show interfaces mac [detail]
```

All commands accept an optional interface argument (`show interfaces eth0 counters errors`) and ranges (`show interfaces eth0-3 status`).

---

## Reference Outputs (authoritative formatting spec)

### 1. `show interfaces` / `show interfaces <name>`

```
eth0 is up, line protocol is up (connected)
  Hardware is Ethernet, address is 2c:dd:e9:12:00:a1 (bia 2c:dd:e9:12:00:a1)
  Description: WAN uplink to ISP - Circuit ID 4471-A
  Internet address is 203.0.113.2/30
  Broadcast address is 255.255.255.255
  IP MTU 1500 bytes, BW 10000000 kbit
  Full-duplex, 10Gb/s, auto negotiation: off, uni-link: n/a
  Up 12 days, 4 hours, 33 minutes, 12 seconds
  Loopback Mode : None
  2 link status changes since last clear
  Last clearing of "show interface" counters 12 days, 4:33:12 ago
  5 minutes input rate 24.7 Mbps (0.2% with framing overhead), 4123 packets/sec
  5 minutes output rate 96.3 Mbps (1.0% with framing overhead), 9877 packets/sec
     4294811034 packets input, 4816030792344 bytes
     Received 15234 broadcasts, 89127 multicast
     0 runts, 0 giants
     0 input errors, 0 CRC, 0 alignment, 0 symbol, 0 input discards
     0 PAUSE input
     8812734120 packets output, 11278449021837 bytes
     Sent 1287 broadcasts, 44506 multicast
     0 output errors, 0 collisions
     0 late collision, 0 deferred, 2 output discards
     0 PAUSE output

eth1 is up, line protocol is up (connected)
  Hardware is Ethernet, address is 2c:dd:e9:12:00:a2 (bia 2c:dd:e9:12:00:a2)
  Description: LAN trunk to qs-hq-access1
  Ethernet MTU 9214 bytes, BW 1000000 kbit
  Full-duplex, 1Gb/s, auto negotiation: on, uni-link: n/a
  Up 12 days, 4 hours, 31 minutes, 2 seconds
  Loopback Mode : None
  1 link status changes since last clear
  Last clearing of "show interface" counters never
  5 minutes input rate 3.11 Mbps (0.3% with framing overhead), 1204 packets/sec
  5 minutes output rate 1.02 Mbps (0.1% with framing overhead), 655 packets/sec
     102981234 packets input, 90238471234 bytes
     Received 88123 broadcasts, 412987 multicast
     0 runts, 0 giants
     0 input errors, 0 CRC, 0 alignment, 0 symbol, 0 input discards
     0 PAUSE input
     88123911 packets output, 71234098123 bytes
     Sent 9812 broadcasts, 128730 multicast
     0 output errors, 0 collisions
     0 late collision, 0 deferred, 0 output discards
     0 PAUSE output

eth2 is administratively down, line protocol is down (disabled)
  Hardware is Ethernet, address is 2c:dd:e9:12:00:a3 (bia 2c:dd:e9:12:00:a3)
  Ethernet MTU 9214 bytes, BW 1000000 kbit
  Full-duplex, Unconfigured, auto negotiation: off, uni-link: n/a
  Down 12 days, 4 hours, 40 minutes, 51 seconds
  Loopback Mode : None
  0 link status changes since last clear
  Last clearing of "show interface" counters never
  5 minutes input rate 0 bps (0.0% with framing overhead), 0 packets/sec
  5 minutes output rate 0 bps (0.0% with framing overhead), 0 packets/sec
     0 packets input, 0 bytes
     Received 0 broadcasts, 0 multicast
     0 runts, 0 giants
     0 input errors, 0 CRC, 0 alignment, 0 symbol, 0 input discards
     0 PAUSE input
     0 packets output, 0 bytes
     Sent 0 broadcasts, 0 multicast
     0 output errors, 0 collisions
     0 late collision, 0 deferred, 0 output discards
     0 PAUSE output

lo is up, line protocol is up (connected)
  Hardware is Loopback
  Description: Router-ID
  Internet address is 10.255.0.1/32
  Broadcast address is 255.255.255.255
  IP MTU 65535 bytes
  Up 12 days, 4 hours, 41 minutes, 10 seconds

vlan10 is up, line protocol is up (connected)
  Hardware is Vlan, address is 2c:dd:e9:12:00:a2 (bia 2c:dd:e9:12:00:a2)
  Description: LAN-USERS
  Internet address is 10.20.10.1/24
  Broadcast address is 255.255.255.255
  IP MTU 1500 bytes, BW 1000000 kbit
  Up 12 days, 4 hours, 31 minutes, 0 seconds

bond0 is up, line protocol is up (connected)
  Hardware is Port-Channel, address is 2c:dd:e9:12:00:a4 (bia 2c:dd:e9:12:00:a4)
  Description: LAG to qs-hq-core
  Ethernet MTU 9214 bytes, BW 20000000 kbit
  Up 12 days, 3 hours, 58 minutes, 44 seconds
  Active members in this channel: 2
  ... eth3 , Full-duplex, 10Gb/s
  ... eth4 , Full-duplex, 10Gb/s
  Fallback mode is: off
  5 minutes input rate 210 Mbps (1.1% with framing overhead), 24123 packets/sec
  5 minutes output rate 189 Mbps (1.0% with framing overhead), 21877 packets/sec
     84812734120 packets input, 101278449021837 bytes
     Received 812 broadcasts, 991234 multicast
     0 input errors, 0 input discards
     78123911223 packets output, 91234098123441 bytes
     Sent 44 broadcasts, 812734 multicast
     0 output errors, 0 output discards
```

**Grammar rules:**
- State line: `<name> is {up | down | administratively down}, line protocol is {up | down | lowerlayerdown | notpresent} ({connected | notconnect | disabled | errdisabled | inactive})`
- `bia` = burned-in (hardware) address. If a MAC is overridden in config, print the configured MAC first and the hardware MAC as bia.
- Rate window follows configured load-interval (default 300s → "5 minutes"; a 30s interval prints "30 seconds input rate").
- `(x.x% with framing overhead)` = utilization vs line speed including preamble + IFG (20 bytes/frame).
- Speed prints `Unconfigured` when link is down with no forced speed.
- `Last clearing ... never` if never cleared; otherwise `d days, h:mm:ss ago`.
- `lo`, `vlan*`, `tun*`, `wg*` omit the hardware counter block unless real counters exist (rtnetlink stats exist for these — include the counter block for them using the same layout, minus runts/giants/CRC lines which don't apply; use the reduced block shown for bond0).

### 2. `show interfaces description`

```
Interface                      Status         Protocol           Description
eth0                           up             up                 WAN uplink to ISP - Circuit ID 4471-A
eth1                           up             up                 LAN trunk to qs-hq-access1
eth2                           admin down     down
eth3                           up             up                 LAG member bond0
eth4                           up             up                 LAG member bond0
bond0                          up             up                 LAG to qs-hq-core
lo                             up             up                 Router-ID
vlan10                         up             up                 LAN-USERS
```

- Column widths: Interface 31, Status 15, Protocol 19, Description = rest of line.
- Status: `up` / `down` / `admin down`. Protocol: `up` / `down` / `lowerlayerdown`.
- Natural sort within type groups: physical (ethN), bonds, vlans, tunnels, lo last? No — pure natural alphanumeric sort across all names (bond0, eth0..ethN, lo, tun0, vlan10, wg0).

### 3. `show interfaces status`

```
Port       Name                          Status       Vlan     Duplex Speed  Type            Flags Encapsulation
eth0       WAN uplink to ISP - Circui    connected    routed   full   10G    10GBASE-SR
eth1       LAN trunk to qs-hq-access1    connected    trunk    full   1G     1000BASE-T
eth2                                     disabled     1        full   unconf 1000BASE-T
eth3       LAG member bond0              connected    in bond0 full   10G    10GBASE-CR
eth4       LAG member bond0              connected    in bond0 full   10G    10GBASE-CR
bond0      LAG to qs-hq-core             connected    trunk    full   20G    N/A
```

- `Name` truncates description at 26 chars, hard cut, no ellipsis.
- `Vlan` column: `routed`, access VLAN id, `trunk`, or `in bond<N>`.
- `a-` prefix for auto-negotiated values (`a-full`, `a-1G`).
- Filters (`connected` etc.) restrict rows, header unchanged.

`show interfaces status errdisabled`:

```
Port       Name                          Status       Reason
eth6                                     errdisabled  link-flap
```

### 4. `show interfaces counters`

```
Port                 InOctets     InUcastPkts       InMcastPkts     InBcastPkts
eth0            4816030792344      4294811034             89127           15234
eth1              90238471234       102981234            412987           88123
eth2                        0               0                 0               0
eth3            5123098123441     48123911223            412334              12
eth4            5012098123441     47123911223            409877              10

Port                OutOctets    OutUcastPkts      OutMcastPkts    OutBcastPkts
eth0           11278449021837      8812734120             44506            1287
eth1              71234098123        88123911            128730            9812
eth2                        0               0                 0               0
eth3           45012098123441     39123911223            406877              22
eth4           46222049021837     39000000997            405877              22
```

- Two stacked tables, right-aligned u64 counters, no thousands separators.
- Bonds excluded (members carry counters).

### 5. `show interfaces counters errors`

```
Port          FCSErr    AlignErr   SymbolErr        RxErr       Runts      Giants       TxErr
eth0               0           0           0            0           0           0           0
eth1               0           0           0            0           0           0           0
eth2               0           0           0            0           0           0           0
eth3              12           0           3           15           0           0           0
eth4               0           0           0            0           0           0           0
```

- `RxErr` = aggregate input errors (≥ sum of specific columns).

### 6. `show interfaces counters discards`

```
Port         InDiscards       OutDiscards
eth0                  0                 2
eth1                  0                 0
eth2                  0                 0
eth3                  0             41234
eth4                  0             40997
```

### 7. `show interfaces counters rates`

```
Port      Intvl   InMbps      InKpps  InPct   OutMbps     OutKpps  OutPct
eth0       0:05     24.7         4.1   0.2%      96.3         9.9    1.0%
eth1       0:05      3.1         1.2   0.3%       1.0         0.7    0.1%
eth2       0:05      0.0         0.0   0.0%       0.0         0.0    0.0%
eth3       0:05    105.2        12.1   1.1%      94.4        10.9    0.9%
eth4       0:05    104.8        12.0   1.0%      94.6        11.0    0.9%
```

- `Intvl` prints load-interval as `m:ss` where `0:05` means 5 minutes (EOS quirk — preserve it).
- Percentages relative to link speed.

### 8. `show interfaces counters queue`

```
Port      TxQ    Counter/pkts        Counter/bytes      Dropped/pkts     Dropped/bytes
eth0      UC0        84123441       81234981234412                 0                 0
eth0      UC1               0                    0                 0                 0
eth0      UC2               0                    0                 0                 0
eth0      UC3        12341123        9812734412334                 0                 0
eth0      UC4               0                    0                 0                 0
eth0      UC5               0                    0                 0                 0
eth0      UC6          441233           4412334981                 0                 0
eth0      UC7         8812344          88123449812                 2              3028
```

- Map queues to the tc/qdisc or NIC hardware TX queue stats (`ethtool -S` per-queue counters). If the NIC exposes N queues, print UC0..UC(N-1).

### 9. `show interfaces counters bins`

```
eth0
  Received frame size distribution:
    64 bytes:                412334981
    65-127 bytes:           1123441233
    128-255 bytes:           441233441
    256-511 bytes:           123441233
    512-1023 bytes:           88123441
    1024-1522 bytes:        2105745912
    1523-max bytes:                  0
  Transmitted frame size distribution:
    64 bytes:                212334981
    65-127 bytes:            923441233
    128-255 bytes:           341233441
    256-511 bytes:           223441233
    512-1023 bytes:          188123441
    1024-1522 bytes:        6923158791
    1523-max bytes:                  0
```

- Source: RMON stats from `ethtool -S` (driver-dependent names, e.g. `rx_64_byte_packets`). If the driver lacks a bin, print 0 and note support per-driver in code comments.

### 10. `show interfaces transceiver`

```
If system temperature is too high, transceiver temperature will rise 5 C
per 1 C rise in system temperature
                                                    Rx Power   Tx Power
Port       Temp (C)  Voltage (V)  Bias (mA)         (dBm)      (dBm)     Last Update
---------- --------- ------------ ----------------- ---------- --------- -------------------
eth0           33.45         3.28              6.42      -2.35      -1.87 0:00:04 ago
eth3           29.11         3.30              7.01      -1.02      -0.95 0:00:04 ago
eth4           29.87         3.29              6.88      -1.11      -0.98 0:00:04 ago
```

- Copper ports omitted; `N/A` for unreported fields. Source: `ethtool -m` (SFF-8472/SFF-8636).

`show interfaces transceiver detail`:

```
eth0
  Transceiver Type: 10GBASE-SR
  Vendor Name: FINISAR CORP.
  Vendor Part Number: FTLX8574D3BCL
  Vendor Serial Number: UWM01B7
  Vendor Date Code: 210412
  Temperature: 33.45 C
    High alarm threshold:  75.00 C   High warn threshold:  70.00 C
    Low alarm threshold:   -5.00 C   Low warn threshold:    0.00 C
  Voltage: 3.28 V
    High alarm threshold:   3.63 V   High warn threshold:   3.46 V
    Low alarm threshold:    2.97 V   Low warn threshold:    3.13 V
  Tx Bias: 6.42 mA
    High alarm threshold:  11.80 mA  High warn threshold:  10.80 mA
    Low alarm threshold:    4.00 mA  Low warn threshold:    5.00 mA
  Tx Power: -1.87 dBm
    High alarm threshold:   1.70 dBm High warn threshold:  -1.30 dBm
    Low alarm threshold:   -9.50 dBm Low warn threshold:   -8.30 dBm
  Rx Power: -2.35 dBm
    High alarm threshold:   2.00 dBm High warn threshold:  -1.00 dBm
    Low alarm threshold:  -13.10 dBm Low warn threshold:  -12.10 dBm
```

`show interfaces transceiver properties`:

```
Name : eth0
Administrative Speed: 10G
Administrative Duplex: full
Operational Speed: 10G
Operational Duplex: full
Media Type: 10GBASE-SR
```

`show interfaces transceiver eeprom` (raw SFF page hex dump):

```
eth0:
  A0 page:
    0000: 03 04 07 10 00 00 00 00  00 00 00 06 67 00 0a 64
    0010: 00 00 00 00 46 49 4e 49  53 41 52 20 43 4f 52 50
    ...
```

### 11. `show interfaces capabilities`

```
eth0
  Model:          NS-FW-1U-8X10G
  Type:           10GBASE-SR
  Speed/Duplex:   1G/full,10G/full,auto
  Flowcontrol:    rx-(off,on,desired),tx-(off,on,desired)

eth1
  Model:          NS-FW-1U-8X10G
  Type:           1000BASE-T
  Speed/Duplex:   10M/half,10M/full,100M/half,100M/full,1G/full,auto
  Flowcontrol:    rx-(off,on,desired),tx-(off,on,desired)
```

- `Model` comes from the platform/DMI identity (make it a platform config value).
- Speed/Duplex list derived from ethtool supported-modes bitmap.

### 12. `show interfaces flowcontrol`

```
Port       Send FlowControl   Receive FlowControl    RxPause   TxPause
           admin   oper       admin   oper
---------  -----   -----      -----   -----          -------   -------
eth0       off     off        off     off                  0         0
eth1       off     off        off     off                  0         0
eth3       desired on         desired on                  12         0
eth4       desired on         desired on                   9         0
```

- Source: `ethtool -a` + `rx_pause`/`tx_pause` from `ethtool -S`.

### 13. `show interfaces negotiation`

```
Port    Auto-Negotiation                              Local Advertisement
        Mode         Status                           Speed/Duplex        Pause
eth0    off          n/a                              n/a                 n/a
eth1    802.3        success                          10M/half 10M/full   None
                                                      100M/half 100M/full
                                                      1G/full
```

`show interfaces negotiation detail`:

```
eth1
  Auto-Negotiation Mode: IEEE 802.3
  Auto-Negotiation Status: Success
  Local Advertisement
    Speed/Duplex: 10M/half 10M/full 100M/half 100M/full 1G/full
    Pause: None
  Link Partner Advertisement
    Speed/Duplex: 10M/half 10M/full 100M/half 100M/full 1G/full
    Pause: Symmetric
  Resolution
    Speed/Duplex: 1G/full
    Pause: rx off, tx off
```

- Source: `ETHTOOL_GLINKSETTINGS` advertisement + link partner advertisement bitmaps.

### 14. `show interfaces phy detail`

```
Current System Time: Thu Aug 20 14:02:11 2026
eth0
  Current State
    PHY state                                linkUp
    Interface state                          up
    HW resets                                1
    Transceiver                              10GBASE-SR
    Transceiver SN                           UWM01B7
    Oper speed                               10Gbps
    Interrupt count                          4
    Diags mode                               normalOperation
    Model                                    NS-PHY-BCM84891
    Reset count                              1
    PHY state changes                        2
      Last change                            12 days, 4:33:12 ago
  Speed Configuration
    Configured speed                         10Gfull
    Auto-negotiation                         off
```

- Field availability is driver-dependent; keep the two-column `label<pad>value` layout (labels left, values starting at column 44) and the section headers. Populate what the driver exposes; omit unavailable rows.

### 15. `show interfaces mac`

```
Port    MAC Address          State
eth0    2c:dd:e9:12:00:a1    linkUp
eth1    2c:dd:e9:12:00:a2    linkUp
eth2    2c:dd:e9:12:00:a3    phyOff
eth3    2c:dd:e9:12:00:a4    linkUp
eth4    2c:dd:e9:12:00:a5    linkUp
```

`show interfaces mac detail`:

```
eth0
  MAC address:            2c:dd:e9:12:00:a1
  MAC state:              linkUp
  Local fault:            False
  Remote fault:           False
  FEC mode:               Disabled
  FEC corrected codewords:   0
  FEC uncorrected codewords: 0
```

- FEC via `ethtool --show-fec` and FEC stat counters where supported.

---

## Data Source Mapping (Debian 13)

| Output element | Source |
|---|---|
| Link/admin state, MAC, MTU, speed, duplex | rtnetlink `RTM_GETLINK`; ethtool `ETHTOOL_GLINKSETTINGS` |
| Byte/packet/error/discard counters | `rtnl_link_stats64` via netlink |
| Per-queue counters, pause frames, RMON bins | `ethtool -S` (driver stat names vary — build a per-driver alias map) |
| Rate calculations | daemon: ring buffer of counter samples, EWMA over load-interval |
| DOM/transceiver + EEPROM | `ethtool -m` (SFF-8472/SFF-8636 page decode) |
| Auto-neg detail | `ETHTOOL_GLINKSETTINGS` local + link-partner advertisement bitmaps |
| Flow control admin/oper | `ethtool -a` |
| FEC | `ethtool --show-fec` + FEC stats |
| Link change count, uptime, last-clear | daemon-tracked (netlink monitor), persisted |
| Description, load-interval, MAC override | Nightshade config tree |

## Architecture Requirements

1. **Daemon responsibilities:** subscribe to netlink link events; sample counters every 5s into a ring buffer; compute load-interval rates; track link-state change counts and timestamps; hold "last clear" baselines per interface (a `clear counters` command subtracts baselines — implement the daemon side now, CLI clear command can be a follow-up).
2. **CLI responsibilities:** parse `show interfaces ...` subcommands including interface names and ranges (`eth0-3`); query daemon; render.
3. **Rendering:** implement a shared table/column-layout module. All formatting rules (pad widths, right-alignment, truncation, natural sort) live there and are unit-tested. Detail-block rendering uses a template-like builder, not ad-hoc `format!` scattering.
4. **Structured output:** every command also supports `| json` (or the repo's existing JSON output convention) emitting the underlying data model via serde. The text renderer and JSON serializer consume the same structs.
5. **Graceful degradation:** virtual interfaces (lo, vlan, tun, wg) and drivers lacking specific stats must render cleanly (omit sections or print 0/N/A per the rules above), never error.

## Testing Requirements

- Golden-file tests for every command's text output using a fixture data model (the exact reference outputs above are the golden files — copy them in).
- Unit tests for: natural sort, 26-char hard truncation, `a-` prefix logic, rate/percentage math including framing overhead, duration formatting (`12 days, 4:33:12 ago`, `never`), MAC formatting, load-interval label ("5 minutes" vs "30 seconds").
- Property test: text renderer never panics for any combination of optional fields.
- Integration test (behind a feature flag) that runs against real netlink on a veth pair.

## Process

- Inspect the monorepo first: follow existing crate structure, IPC conventions, CLI parser framework, and error-handling patterns. Add new crates only if no suitable home exists (suggested: `ns-ifstate` daemon lib + CLI command module in the existing shell crate).
- Work incrementally: data model + daemon collection first, then `show interfaces` and `status`/`description`, then counters family, then transceiver/phy/mac/negotiation.
- Every reference output above is normative. When EOS behavior and Linux reality conflict, keep the EOS visual format and adapt the data source, documenting the mapping in code comments.
