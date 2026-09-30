# DLNA MediaServer

DLNA is an opt-in, IPv4-only MediaServer for trusted local networks. The default Compose stack keeps it disabled. It exposes a small UPnP ContentDirectory and ConnectionManager surface and delivers original catalog files over HTTP; it does not control a renderer or transcode for a device profile.

## Pair a renderer

The operator first authenticates to Puffinbox and creates a pairing through `POST /Puffinbox/Dlna/Pairings`:

```json
{
  "DeviceName": "Living room display",
  "ClientAddress": "192.168.10.42"
}
```

The caller must be a local client, and the renderer address must belong to a configured local network. The server stores one exact IPv4 `/32` address per pairing; an address cannot be assigned to two accounts. The response includes a `DescriptionUrl`. The pairing UUID in that URL is only an identifier: every description, SOAP, and media request also has to arrive from the paired address over the direct socket connection. Forwarded-IP headers do not select a renderer or account.

The paired account must remain enabled and have media playback permission. Each catalog and media request checks that account again and applies its current library and parental policy. Removing the pairing, disabling the account, revoking playback, or restricting the item blocks later requests. The renderer receives no account token.

This policy associates an account with a network address; it does not cryptographically identify a physical device. Use a trusted, isolated LAN. A renderer IP change requires deleting the old pairing and pairing the new address. IPv6 is not supported, and only one renderer can use a given address.

## Supported surface

- SSDP answers unicast `M-SEARCH` only when its direct IPv4 source matches an enabled pairing. While any account has an enabled pairing and media playback permission, it multicasts `ssdp:alive` notifications with standard headers on the configured interface and refreshes their 180-second cache every 90 seconds. The messages use the stable server UDN and a common `/Puffinbox/Dlna/description.xml` location, so they contain no pairing UUID or renderer-specific URL. That direct HTTP route selects a pairing from the socket peer and applies the current account policy; unpaired peers receive `404`. When no eligible pairing remains, or on orderly shutdown, the server sends `ssdp:byebye` for the same device and services. A process crash relies on the advertised cache timeout.
- ContentDirectory supports `Browse`, `GetSystemUpdateID`, `GetSearchCapabilities`, and `GetSortCapabilities`, with policy-filtered pages capped at 200 entries. `SystemUpdateID` comes from a transactional server-wide revision counter. It advances once for each insert, update, or delete statement targeting items, libraries, item metadata, Live TV channel records, users, or library-access relationships. It may advance even when a particular renderer's view did not change. It is exposed as a 32-bit UPnP value.
- ContentDirectory eventing supports GENA `SUBSCRIBE`, renewal, `UNSUBSCRIBE`, and `NOTIFY` for `SystemUpdateID` and the root `ContainerUpdateIDs`. A new subscription receives an initial event; the server checks for catalog changes every five seconds and allows up to eight notifications in flight. Subscriptions expire after at most 30 minutes, with at most eight per pairing and 64 total. Callback URLs must use HTTP and a literal IPv4 address equal to the paired peer. Redirects and proxy routing are disabled, and the pairing's current account permission is checked before each notification.
- ConnectionManager reports the explicitly supported direct HTTP media types. No active connection IDs are tracked; `GetCurrentConnectionIDs` is empty, valid but unknown IDs receive UPnP error 706, and malformed IDs receive error 402.
- Media URLs support `GET`, `HEAD`, and byte ranges for MP4, Matroska, WebM, AVI, MPEG/TS, the listed common audio formats, and JPEG/PNG/GIF/WebP/AVIF/BMP images. Active content and unrecognized formats are not advertised as resources.

The ContentDirectory description advertises its event endpoint and evented state variables. AVTransport remote control, renderer-specific codec negotiation, and transcoding are unavailable. Some renderers require these features and may not work. No hardware renderer has been used to claim device interoperability.

## Compose on Linux

Set `PUFFINBOX_DLNA_INTERFACE_ADDRESS` to the host's LAN IPv4 address and `PUFFINBOX_DLNA_ADVERTISED_ORIGIN` to `http://<that-address>:8096` in `.env`. Then opt into the host-network overlay:

```sh
docker compose -f docker-compose.yml -f docker-compose.dlna.yml config --quiet
docker compose -f docker-compose.yml -f docker-compose.dlna.yml up -d
```

The overlay uses Linux host networking so the server can receive SSDP multicast and unicast traffic on UDP 1900. It binds the HTTP server to the configured interface at TCP 8096. PostgreSQL remains in its container and is published only on `127.0.0.1:${PUFFINBOX_POSTGRES_HOST_PORT:-55432}` so the host-networked server can reach it. Keep that database port loopback-only. Allow UDP 1900 and TCP 8096 only on the trusted LAN using the host firewall. The overlay requires a Docker Compose version that supports the `!reset` merge tag and a Linux Docker Engine with host networking.

The DLNA HTTP path is unencrypted. Do not expose it to the public Internet or an untrusted network.
