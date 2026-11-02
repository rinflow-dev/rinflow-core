# Security design

To report a vulnerability, see [SECURITY.md](../SECURITY.md).

A connected peer can **inject keyboard and mouse input**, so the core encrypts and authenticates connections, and can limit them to devices the user paired.

- **Identity.** Each device has a self-signed certificate and key (`identity_path`), made on first start. Peers know it by the SHA-256 fingerprint of the certificate. There is no certificate authority: trust comes from pairing.
- **Encryption.** Every connection is QUIC with TLS 1.3, for input and control as well as clipboard and files. Each side learns the other's fingerprint during the handshake. A data link is attached to a session only if it shows the same certificate.
- **Admission.** After the peer's `Hello`:

  | Peer | Result |
  | --- | --- |
  | In `trusted_peers` with the same fingerprint | Let in (`paired: true`). |
  | In `trusted_peers` with another fingerprint | Refused: `"the device's identity changed"`. |
  | Unknown, `require_pairing` on | Pairs first. |
  | Unknown, `require_pairing` off | Let in, encrypted but not authenticated. It can still ask to pair. |

- **Pairing.** The side that wants it sends `PairRequest`, and both sides pair, also one that already trusts the other: a user can only check a code against the other screen. The code comes from both fingerprints and a random 32-byte nonce from each side. The side that dialed the connection commits to its nonce first (`PairCommit`), the other side answers with its own (`PairNonce`), and the dialer then reveals its nonce (`PairReveal`), which has to match the commitment. The code is SHA-256 of `"rinflow pairing code v2\0"`, the dialer's fingerprint, `\0`, the other side's fingerprint, `\0`, the dialer's nonce and the other side's nonce; the first four bytes as a number modulo 10⁶. Since no side can change its nonce after seeing the other's, a device in the middle, which has to run a pairing with each side under its own certificate, gets the same code on both screens only one time in a million. Each shell shows the code (`pairing_requested`), and the user accepts once the other screen shows the same code. When both sides have accepted, each reports `paired` and lets the peer in. A peer without capability bit 11 (`PAIRING_V2`, an older build, whose code came from the fingerprints alone and could be forged) cannot pair (`"the other device needs a newer Rinflow to pair"`), though devices paired before still connect. A pairing message out of turn or a reveal that does not match its commitment ends the connection (`"pairing failed"`). Declining (`"pairing declined"`, and `"pairing declined on the other device"` on the other side) or two minutes without an answer (`"pairing timed out"`) closes the connection. A device that sends input before it is let in is ignored.
- **Turning `require_pairing` on** closes sessions with peers that are not paired (`"pairing required"`). With it on, `lc_core_unpair` closes the peer's session too (`"unpaired"`).
- **What is still exposed.** Discovery announcements are plain UDP and tell the whole network each device's name, ID and fingerprint. With `require_pairing` off, any Rinflow device on the LAN can connect (encrypted, but nobody checked who it is). The Windows client turns `require_pairing` on by default.
