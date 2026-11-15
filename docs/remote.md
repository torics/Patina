# Using Patina from another device

Patina listens on loopback only. To reach it from your phone or another computer, put a tunnel in front of it that adds HTTPS and decides who may connect. Tailscale does both and is what this page uses; any reverse proxy on the same machine works the same way.

## With Tailscale

On the machine that runs Patina, with Tailscale installed and signed in:

```sh
tailscale serve --bg 7878                              # the app, at https://<machine>.<tailnet>.ts.net
tailscale serve --bg --https=8443 http://127.0.0.1:7879  # the preview, on its own origin
```

Then tell Patina the name it is reached by, in `config.toml` in the config folder:

```toml
allowed_hosts = ["localhost", "127.0.0.1", "[::1]", "box.tailnet.ts.net"]
preview_origin = "https://box.tailnet.ts.net:8443"
```

Restart Patina and open `https://box.tailnet.ts.net` from any device on your tailnet. Only devices you allow on the tailnet can connect, and the connection is encrypted end to end.

- **Why `allowed_hosts`:** Patina refuses requests whose `Host` it does not know. This stops a web page from reaching your daemon through DNS tricks. Add each name you use; `tailscale serve` passes the name through.
- **Why a second port:** the preview shows the run's dev server, which runs code an agent may have written. It gets its own origin so that it cannot read the app's cookies or call its API.
- **Cookies:** sessions are marked `Secure` when the request came over HTTPS, as through `tailscale serve`.

Do not use `tailscale funnel`, which publishes to the whole internet. The password and its lockout protect the app, but there is no reason to offer it to everyone.

## Binding to the network instead

`patina serve --bind 0.0.0.0` is refused unless a password is set and `allowed_hosts` names the host clients will use. Traffic is then plain HTTP on your network, so prefer a tunnel.

## On a phone

The workbench works on a narrow screen: the timeline and approvals come first, and every approval action is a button you can reach with a thumb.
