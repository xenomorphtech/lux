# IPv4/TCP Stack

[`examples/tcp_ip.lux`](../examples/tcp_ip.lux) is a packet-oriented IPv4/TCP
implementation written entirely in Lux. Its boundary is deliberately pure:
`input` accepts one complete IPv4 packet as a binary and returns a `TcpStep`
containing the next connection state, an optional outbound IPv4 packet, optional
delivered application bytes, and an event code.

## Included

- IPv4 header encoding and parsing, including total-length, fragmentation, protocol,
  and header-checksum validation
- TCP encoding and parsing, including the IPv4 pseudo-header checksum
- active and passive opens
- ordered payload delivery and cumulative ACKs
- RST handling
- active and passive FIN close paths through the standard TCP states
- 32-bit sequence-number wrapping for emitted sequence values
- a deterministic self-test covering a known checksum vector, corruption rejection,
  handshake, data transfer, and orderly close

The entry points used by an adapter are `listening`, `connect`, `input`, `tcp_send`,
and `close`. The adapter owns packet buffers and repeatedly feeds `TcpStep.outbound`
to its raw-IP device and received packets back to `input`.

## Deliberate Limits

This is a compact transport engine, not a production Internet host. It does not yet
implement IP reassembly, TCP option negotiation, out-of-order reassembly, retransmit
timers, congestion control, path-MTU discovery, or TIME-WAIT expiration. Packets
with IPv4 fragments are rejected. The encoder emits fixed 20-byte IPv4 and TCP
headers.

The current Yggdrasil bytecode ABI exposes numeric network-buffer handles but has no
binary term or buffer create/take instruction. Consequently this stack runs through
Lux's BEAM backend today. Running the same Lux code on Yggdrasil requires a byte-array
term plus packet-buffer bridge in the Yggdrasil runtime; the protocol code itself
does not need NIC-specific changes.

## Verify

```bash
cargo test --test tcp_ip_stack
```
