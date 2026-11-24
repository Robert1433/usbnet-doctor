# usbnet-doctor

A small Windows command-line tool that finds out why a USB network adapter
(or anything else on a USB hub) is slow or unstable.

It checks, in one run:

- **USB topology** – every controller, hub and port, merged into physical
  connectors (a USB 3 port and its USB 2.0 companion), with the speed each
  device actually runs at (1.5/12/480 Mbps, 5 or 10 Gbps), USB-C ports,
  port errors (not enough power, overcurrent, failed enumeration) and USB 3
  devices stuck at USB 2.0 speed.
- **Network adapters** – link state and speed, traffic, errors and discards,
  IPv4 and gateway, driver version, where the adapter sits in the USB tree,
  when it was last plugged in, and the driver settings that matter
  (Energy-Efficient Ethernet, selective suspend, speed & duplex, flow control).
- **Event log** – network-driver and USB-stack events from the System log
  (folded by message), plus network connects/disconnects.
- **Ping** – loss, min/avg/max and jitter to the router and to 1.1.1.1 / 8.8.8.8.
- **Speed test** – download and upload against speed.cloudflare.com, with
  latency measured during the download to catch bufferbloat.
- **Diagnosis** – plain-language findings built from all of the above.

No admin rights are needed.

## Usage

```
usbnet-doctor [COMMAND] [OPTIONS]

Commands:
  all      Everything below, then a diagnosis (default)
  usb      USB tree with the speed of every port and device
  net      Network adapters, their USB location and driver settings
  events   Driver errors and network disconnects from the event log
  ping     Packet loss and jitter to the router and the internet
  speed    Download/upload speed and latency under load

Options:
  --days <N>           Days of event log history to scan [default: 3]
  --pings <N>          Pings per target [default: 30]
  --mb <N>             Megabytes per download run in the speed test [default: 50]
  --rounds <N>         Download runs in the speed test [default: 2]
  --no-speed           Skip the internet speed test in `all`
  --provider <NAME>    Extra event-log provider (or part of its name) to watch; can repeat
  --events-shown <N>   Events to list per section [default: 12]
  -v, --verbose        Show every advanced driver setting
  --no-color           Plain output without colors
```

## Building

```
cargo build --release
```

The binary is `target\release\usbnet-doctor.exe`. The only dependencies are
`windows-sys` 0.59 and `winreg`, which link through import libraries, so the
project builds with the plain `x86_64-pc-windows-gnu` toolchain from rustup
(no Visual Studio or MinGW install needed) as well as with MSVC.

## How it works

- USB: the hub IOCTLs that Microsoft's USBView uses
  (`IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX(_V2)`,
  `IOCTL_USB_GET_PORT_CONNECTOR_PROPERTIES`, string descriptors).
- Network: `GetIfTable2`, `GetIpForwardTable2`, `GetUnicastIpAddressTable`
  and the adapter's registry class key (`Ndi\Params`) for driver settings.
- Events: `EvtQuery` / `EvtFormatMessage` on the System and
  NetworkProfile/Operational logs.
- Ping: `IcmpSendEcho`. Speed test: WinHTTP (uses the system proxy).
