# usbnet-doctor

A small command-line tool for **Windows and Linux** that finds out why a USB
network adapter (or anything else on a USB hub) is slow or unstable.

It checks, in one run:

- **USB topology** – every controller, hub and port, merged into physical
  connectors (a USB 3 port and its USB 2.0 companion), with the speed each
  device actually runs at (1.5/12/480 Mbps, 5, 10 or 20 Gbps), USB-C ports,
  port errors (not enough power, overcurrent, failed enumeration) and USB 3
  devices stuck at USB 2.0 speed.
- **Network adapters** – link state and speed, traffic, errors and discards,
  IPv4 and gateway, driver version, where the adapter sits in the USB tree,
  when it was last plugged in, and the driver settings that matter
  (Energy-Efficient Ethernet, USB selective suspend / autosuspend, speed &
  duplex, flow control).
- **System log** – network-driver and USB-stack messages (folded by message),
  plus network connects/disconnects. On Windows this is the event log; on
  Linux the kernel log.
- **Ping** – loss, min/avg/max and jitter to the router and to 1.1.1.1 / 8.8.8.8.
- **Speed test** – download and upload against speed.cloudflare.com, with
  latency measured during the download to catch bufferbloat.
- **Diagnosis** – plain-language findings built from all of the above.

No admin or root rights are needed.

On Windows, double-clicking the `.exe` opens its own console window. That
window stays open after the checks finish: press Enter to run them again, or
close it with X. Run from an existing terminal, the tool exits normally.

## Usage

```
usbnet-doctor [COMMAND] [OPTIONS]

Commands:
  all      Everything below, then a diagnosis (default)
  usb      USB tree with the speed of every port and device
  net      Network adapters, their USB location and driver settings
  events   Driver errors and network disconnects from the system log
  ping     Packet loss and jitter to the router and the internet
  speed    Download/upload speed and latency under load

Options:
  --days <N>           Days of system log history to scan [default: 3]
  --pings <N>          Pings per target [default: 30]
  --mb <N>             Megabytes per download run in the speed test [default: 50]
  --rounds <N>         Download runs in the speed test [default: 2]
  --no-speed           Skip the internet speed test in `all`
  --provider <NAME>    Extra log source (driver or provider name) to watch; can repeat
  --events-shown <N>   Events to list per section [default: 12]
  -v, --verbose        Show every advanced driver setting
  --no-color           Plain output without colors
```

## Building

```
cargo build --release
```

The binary is `target/release/usbnet-doctor` (`usbnet-doctor.exe` on
Windows). Every push is also built for both systems by GitHub Actions; the
binaries are attached to each run as artifacts.

- **Windows**: the only dependencies are `windows-sys` 0.59 and `winreg`,
  which link through import libraries, so the project builds with the plain
  `x86_64-pc-windows-gnu` toolchain from rustup (no Visual Studio or MinGW
  install needed) as well as with MSVC.
- **Linux**: the only dependency is `libc`. At run time the tool uses
  `journalctl` (or `dmesg`) for the kernel log and `curl` for the speed test;
  both are present on nearly every distribution.

## Linux notes

- The kernel log is read from the systemd journal. Without permission to read
  it, the tool falls back to `dmesg` (current boot only) and says so. To see
  everything without `sudo`, add yourself to the `systemd-journal` (or `adm`)
  group.
- Ping uses unprivileged ICMP sockets (allowed by `net.ipv4.ping_group_range`
  on most distributions), a raw socket when run as root, or the `ping`
  command as a last resort.
- Driver settings come from read-only `ethtool` queries, so they need no root.
  To change them, use `ethtool` (for example
  `sudo ethtool --set-eee enx00e04c680001 eee off`) and the USB device's
  `power/control` file in sysfs (`on` turns autosuspend off).
- Port types (USB-A, USB-C, internal) depend on what the firmware tells the
  kernel; when it says nothing the column stays empty.

## How it works

Windows:

- USB: the hub IOCTLs that Microsoft's USBView uses
  (`IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX(_V2)`,
  `IOCTL_USB_GET_PORT_CONNECTOR_PROPERTIES`, string descriptors).
- Network: `GetIfTable2`, `GetIpForwardTable2`, `GetUnicastIpAddressTable`
  and the adapter's registry class key (`Ndi\Params`) for driver settings.
- Events: `EvtQuery` / `EvtFormatMessage` on the System and
  NetworkProfile/Operational logs.
- Ping: `IcmpSendEcho`. Speed test: WinHTTP (uses the system proxy).

Linux:

- USB: `/sys/bus/usb/devices` – device speed and version, hub ports with
  their `peer` links (which pair each USB 3 port with its USB 2.0 twin),
  `connect_type`, `over_current_count` and `power/connected_duration`.
- Network: `/sys/class/net`, `getifaddrs`, `/proc/net/route`, and the
  `ETHTOOL_GDRVINFO` / `GSET` / `GEEE` / `GPAUSEPARAM` ioctls.
- Events: `journalctl _TRANSPORT=kernel -o json` (or `dmesg`), filtered to
  network-driver and USB-stack messages; "Link is Up/Down" and Wi-Fi
  (de)association lines count as connects and disconnects.
- Ping: ICMP sockets. Speed test: `curl` (honours `https_proxy`).

The code is split the same way: `src/win/` and `src/linux/` talk to the
operating system, and everything else (the USB tree, diagnosis, ping
statistics, output) is shared. `tests/linux_sysfs.rs` runs the Linux build
against a fake `/sys` (set with `USBNET_DOCTOR_SYSFS`).

## License

MIT – see [LICENSE](LICENSE).
