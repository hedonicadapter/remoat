"""mDNS/zeroconf: advertise this daemon + browse LAN peers.

Browsers can't do mDNS, so the daemon discovers peers and reports them to the phone.
Selecting a device in the app reconnects the WS directly to that peer's URL.
"""
import socket
import sys

try:
    from zeroconf import ServiceInfo, Zeroconf, ServiceBrowser
    _HAVE_ZC = True
except Exception:
    _HAVE_ZC = False

SERVICE_TYPE = "_remoat._tcp.local."


def _platform_label() -> str:
    return {"darwin": "mac", "linux": "linux", "win32": "windows"}.get(sys.platform, sys.platform)


class Discovery:
    def __init__(self, name: str, ip: str, port: int):
        self.name = name
        self.ip = ip
        self.port = port
        self._zc = None
        self._info = None
        self._peers = {}  # name -> {name, platform, host, port}
        # Always include self so the dropdown is populated on a single machine
        self._peers[name] = {"name": name, "platform": _platform_label(),
                             "host": ip, "port": port, "self": True}

    def start(self) -> None:
        # mDNS is best-effort — never let discovery take down the daemon.
        # Self is always in the device list, so the app works without it.
        if not _HAVE_ZC:
            print("[mdns] zeroconf unavailable — self-only device list", file=sys.stderr)
            return
        try:
            self._zc = Zeroconf()
            self._info = ServiceInfo(
                SERVICE_TYPE,
                f"{self.name}.{SERVICE_TYPE}",
                addresses=[socket.inet_aton(self.ip)],
                port=self.port,
                properties={"platform": _platform_label(), "name": self.name},
            )
            self._zc.register_service(self._info, allow_name_change=True)
            ServiceBrowser(self._zc, SERVICE_TYPE, handlers=[self._on_change])
            print(f"[mdns] advertising {self.name} at {self.ip}:{self.port}")
        except Exception as e:
            print(f"[mdns] disabled ({e}) — self-only device list", file=sys.stderr)

    def _on_change(self, zeroconf, service_type, name, state_change):
        from zeroconf import ServiceStateChange
        short = name.replace("." + SERVICE_TYPE, "")
        if state_change is ServiceStateChange.Removed:
            if short != self.name:
                self._peers.pop(short, None)
            return
        info = zeroconf.get_service_info(service_type, name)
        if not info or not info.addresses:
            return
        host = socket.inet_ntoa(info.addresses[0])
        props = {k.decode(): v.decode() for k, v in (info.properties or {}).items() if v}
        self._peers[short] = {
            "name": props.get("name", short),
            "platform": props.get("platform", "?"),
            "host": host,
            "port": info.port,
            "self": short == self.name,
        }

    def devices(self) -> list:
        return list(self._peers.values())

    def stop(self) -> None:
        if self._zc:
            try:
                self._zc.unregister_service(self._info)
            finally:
                self._zc.close()
