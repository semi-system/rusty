
## Behind the edge: PROXY protocol v2

The edge sends each connection to a service with a PROXY protocol v2 header (`send-proxy-v2` in HAProxy) so that the service knows the **client's** address (the per-address connection cap and the login
throttle must see the client, not the proxy). Set, per service (`[smtp]`, `[imap]`):

```toml
proxy_protocol = true
trusted_proxies = ["10.0.0.0/24"]     # the addresses the proxy connects from
```

* The header is read **only from `trusted_proxies`**; a connection from any other address is dropped without reading a byte, so a client cannot claim an address. The list is required (1 to 16 blocks) and may not
  hold every address (`0.0.0.0/0`, `::/0` are refused). Without `proxy_protocol` a list is refused too.
* The parser is strict (signature, version, command, bounded length, checked TLVs; the text protocol v1 and UNIX or datagram families are refused) and fuzzed (`fuzz_proxy_v2`). `LOCAL` (HAProxy's health checks) keeps the proxy's
  own address; a `PP2_TYPE_SSL` TLV with the client bit means the client's connection used TLS at the edge.
* **TLS split**: the implicit-TLS ports (465, 993) end TLS at the edge, so with `proxy_protocol` they need **no certificate of their own** and their streams count as protected. The STARTTLS ports (587, 143) are
  passed through by the edge and upgraded by the service itself, so they **still need `[tls]`**: the edge cannot hold the key for a protocol upgrade it does not speak.
